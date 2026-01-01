use anyhow::Result;
use http_body_util::Full;
use hyper::body::Bytes;
use hyper::header::CONTENT_LENGTH;
use hyper::Response;
use pingora_timeout::fast_timeout::fast_sleep;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tracing::error;

use crate::hyper_srv::create_listener;
use crate::{Args, ServerConfig};

pub fn run_thread(
    id: usize,
    addr: SocketAddr,
    config: Arc<ServerConfig>,
    args: &Args,
) -> Result<()> {
    // io_uring implementation for Linux
    use tracing::info;

    let num_entries = args.uring_entries.next_power_of_two();
    let cqsize = num_entries * 2;
    let delay = args.delay;

    let mut uring = tokio_uring::uring_builder();

    uring.setup_single_issuer().setup_cqsize(cqsize);

    if let Some(idle) = args.uring_sqpoll {
        uring.setup_sqpoll(idle);
    } else {
        uring.setup_coop_taskrun().setup_taskrun_flag();
    }

    tokio_uring::builder()
        .entries(num_entries) // Large ring size is critical for throughput
        .uring_builder(&uring)
        .start(async move {
            // Create socket manually with SO_REUSEPORT enabled
            let std_listener = create_listener(addr, args)?;
            let listener = tokio_uring::net::TcpListener::from_std(std_listener);
            info!(
                "Thread {} listening on {} (io_uring, entries: {}, sqpoll: {:?})",
                id, addr, args.uring_entries, args.uring_sqpoll
            );

            loop {
                let (stream, _) = match listener.accept().await {
                    Ok(s) => s,
                    Err(e) => {
                        error!("Thread {} accept error: {}", id, e);
                        continue;
                    }
                };

                let config = config.clone();

                // info!("Accepted a new connection...");

                // Spawn task to handle the connection with io_uring
                tokio_uring::spawn(async move {
                    if let Err(e) = handle_connection_uring(stream, config, false, delay).await {
                        error!("Error handling io_uring connection: {}", e);
                    }
                });
            }
        })
}

#[cfg(all(target_os = "linux", feature = "io_uring"))]
async fn handle_connection_uring(
    stream: tokio_uring::net::TcpStream,
    config: Arc<ServerConfig>,
    http2: bool,
    delay: Option<Duration>,
) -> Result<usize> {
    use http_wire::ToWire;

    if http2 {
        // tracing::warn!("HTTP/2 is not supported with io_uring raw TCP");
        return Err(anyhow::anyhow!("HTTP/2 not supported with io_uring"));
    }

    // Pre-calculate the static response once.
    let response_bytes = match build_response(config.clone()).await {
        Ok(res) => match res.to_bytes().await {
            Ok(bytes) => bytes.to_vec(),
            Err(_) => return Ok(0),
        },
        Err(_) => return Ok(0),
    };

    // Buffer for io_uring read operations (owned by kernel during syscall)
    let mut read_buf = vec![0u8; 8192];

    // Accumulation buffer for partial requests.
    // We maintain indices to avoid moving memory constantly.
    let mut conn_buf = Vec::with_capacity(16384);
    let mut requests_served = 0;

    // Cursor pointers for conn_buf
    let mut buf_processed = 0;

    // Buffer to pass ownership to write_all
    let mut response_buf = response_bytes;

    loop {
        // 1. Read from socket
        let (result, buf) = stream.read(read_buf).await;
        read_buf = buf; // Reclaim ownership

        let n = match result {
            Ok(0) => break, // EOF
            Ok(n) => n,
            Err(e) => return Err(e.into()),
        };

        // 2. Data Management Strategy
        // We only copy data into conn_buf if we have leftovers from previous reads
        // or if the current read doesn't contain a full request (split packet).

        let parse_slice: &[u8];
        let mut using_internal = false;

        if conn_buf.is_empty() {
            // Fast Path: Try to parse directly from the read buffer (Zero Copy)
            parse_slice = &read_buf[..n];
        } else {
            // Slow Path: We have leftovers. Append new data.
            // Optimization: Check if we need to compact before appending to avoid realloc.
            if conn_buf.len() + n > conn_buf.capacity() {
                // If we have processed data at the start, remove it now.
                if buf_processed > 0 {
                    conn_buf.drain(..buf_processed);
                    buf_processed = 0;
                }
            }
            conn_buf.extend_from_slice(&read_buf[..n]);
            parse_slice = &conn_buf[buf_processed..];
            using_internal = true;
        }

        let mut consumed_in_batch = 0;
        let mut loop_slice = parse_slice;

        // 3. Parsing Loop
        while let Some(req_len) = find_complete_request(loop_slice) {
            requests_served += 1;

            if let Some(delay) = delay {
                execute_delay(delay).await;
            }

            // Submit write (io_uring)
            let (res, r) = stream.write_all(response_buf).await;
            response_buf = r;
            res?;

            consumed_in_batch += req_len;

            // Advance the slice for the next iteration in this batch
            if consumed_in_batch < loop_slice.len() {
                loop_slice = &parse_slice[consumed_in_batch..];
            } else {
                break; // Consumed everything in this batch
            }
        }

        // 4. Update Buffer State
        if using_internal {
            // We were working on conn_buf. Advance the processed cursor.
            buf_processed += consumed_in_batch;

            // If we consumed everything, clear the buffer to reset to Fast Path.
            if buf_processed == conn_buf.len() {
                conn_buf.clear();
                buf_processed = 0;
            }
            // Heuristic: If valid data is small and we have a lot of garbage at front, compact.
            // This prevents the buffer from growing indefinitely if we never drain.
            else if buf_processed > 4096 && buf_processed > conn_buf.len() / 2 {
                conn_buf.drain(..buf_processed);
                buf_processed = 0;
            }
        } else {
            // We were in Fast Path (read_buf).
            // If we didn't consume everything, we MUST move leftovers to conn_buf.
            if consumed_in_batch < n {
                conn_buf.extend_from_slice(&read_buf[consumed_in_batch..n]);
                buf_processed = 0;
            }
            // If we consumed everything, conn_buf remains empty, staying in Fast Path.
        }
    }

    Ok(requests_served)
}

#[cold]
async fn execute_delay(delay: std::time::Duration) {
    fast_sleep(delay).await;
}

#[inline(always)]
pub fn find_complete_request(buf: &[u8]) -> Option<usize> {
    // Keep headers on stack.
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut req = httparse::Request::new(&mut headers);

    match req.parse(buf) {
        Ok(httparse::Status::Complete(headers_len)) => {
            // Check Content-Length only if present.
            let content_len = req
                .headers
                .iter()
                .find(|h| h.name.eq_ignore_ascii_case("Content-Length"))
                .and_then(|h| parse_usize(h.value))
                .unwrap_or(0);

            let total_len = headers_len + content_len;
            if buf.len() >= total_len {
                Some(total_len)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Fast usize parser (decimal).
#[inline(always)]
fn parse_usize(buf: &[u8]) -> Option<usize> {
    let mut res: usize = 0;
    let mut found = false;

    for &b in buf {
        if b.is_ascii_digit() {
            // Check for overflow could be added here if needed,
            // but wrapping is standard for "fast" parsing logic.
            res = res.wrapping_mul(10).wrapping_add((b - b'0') as usize);
            found = true;
        } else if found {
            // We were parsing numbers, now we hit a non-digit: stop.
            break;
        } else if b == b' ' || b == b'\t' {
            // Skip leading whitespace
            continue;
        } else {
            // Found a non-digit before any digit (e.g., letters)
            return None;
        }
    }

    if found {
        Some(res)
    } else {
        None
    }
}

async fn build_response(config: Arc<ServerConfig>) -> Result<Response<Full<Bytes>>> {
    let mut builder = Response::builder().status(config.status);

    // Add configured headers
    for (k, v) in &config.headers {
        builder = builder.header(k, v);
    }

    if !config.body.is_empty() {
        builder = builder.header(CONTENT_LENGTH, config.body.len());
    }

    Ok(builder.body(Full::new(config.body.clone()))?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_complete_request_no_body() {
        // GET request without body
        let request = b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n";
        assert_eq!(find_complete_request(request), Some(request.len()));
    }

    #[test]
    fn test_find_complete_request_with_body() {
        // POST request with Content-Length
        let request = b"POST /api HTTP/1.1\r\nHost: localhost\r\nContent-Length: 5\r\n\r\nhello";
        assert_eq!(find_complete_request(request), Some(request.len()));
    }

    #[test]
    fn test_find_complete_request_incomplete_headers() {
        // Missing final \r\n\r\n
        let request = b"GET / HTTP/1.1\r\nHost: localhost\r\n";
        assert_eq!(find_complete_request(request), None);
    }

    #[test]
    fn test_find_complete_request_incomplete_body() {
        // Headers complete but body incomplete
        let request = b"POST /api HTTP/1.1\r\nHost: localhost\r\nContent-Length: 10\r\n\r\nhello";
        assert_eq!(find_complete_request(request), None); // Only 5 bytes, needs 10
    }

    #[test]
    fn test_find_complete_request_multiple_requests() {
        // Two complete requests in buffer
        let buffer = b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\nGET /api HTTP/1.1\r\nHost: localhost\r\n\r\n";
        let first_request_end = "GET / HTTP/1.1\r\nHost: localhost\r\n\r\n".len();
        assert_eq!(find_complete_request(buffer), Some(first_request_end));
    }

    #[test]
    fn test_real_world_scenario() {
        // Simulate a real POST request with JSON body
        let body = b"{\"name\":\"John\",\"age\":30}";
        let content_length = body.len();
        let request = format!(
            "POST /api/users HTTP/1.1\r\nHost: api.example.com\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            content_length,
            std::str::from_utf8(body).unwrap()
        );
        let request_bytes = request.as_bytes();
        assert_eq!(
            find_complete_request(request_bytes),
            Some(request_bytes.len())
        );

        // Test partial request (missing some body bytes)
        let partial = format!(
            "POST /api/users HTTP/1.1\r\nHost: api.example.com\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{{\"name\":\"John\"",
            content_length
        );
        let partial_bytes = partial.as_bytes();
        assert_eq!(find_complete_request(partial_bytes), None);
    }
}
