use anyhow::Result;
use http_body_util::Full;
use hyper::body::Bytes;
use hyper::Response;
use std::net::SocketAddr;
use std::sync::Arc;
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
            info!("Thread {} listening on {} (io_uring, entries: {}, sqpoll: {:?})", id, addr, args.uring_entries, args.uring_sqpoll);

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
                    if let Err(e) = handle_connection_uring(stream, config, false).await {
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
) -> Result<usize> {
    use http_wire::ToWire;

    if http2 {
        use tracing::warn;
        warn!("HTTP/2 is not supported with io_uring raw TCP");
        return Err(anyhow::anyhow!("HTTP/2 not supported with io_uring"));
    }

    // Robust HTTP/1.1 implementation with proper request parsing
    let mut connection_buffer = Vec::new();
    let mut read_buf = vec![0u8; 4096];
    let mut requests_served = 0;

    // Generate response ONCE outside the loop for better performance
    let resp = handle_request(config.clone()).await?;
    let response = resp.to_bytes().await?;
    let mut response: Vec<u8> = response.into();

    loop {
        // Read data from stream
        let (result, buf) = stream.read(read_buf).await;
        read_buf = buf;

        let bytes_read = match result {
            Ok(0) => break, // Connection closed normally by client
            Ok(n) => n,
            Err(e) => return Err(e.into()), // Read error
        };

        if bytes_read > 0 {
            // Append new data to connection buffer
            connection_buffer.extend_from_slice(&read_buf[..bytes_read]);

            // Process complete HTTP requests from buffer
            while let Some(request_end) = find_complete_request(&connection_buffer) {
                requests_served += 1;

                // Just consume the request body and send the pre-generated response
                // Write response
                let (result, r) = stream.write_all(response).await;
                response = r; // Get buffer back for next iteration

                if let Err(e) = result {
                    return Err(e.into());
                }

                // Remove processed request from buffer
                connection_buffer.drain(..request_end);
            }
        }
    }

    Ok(requests_served)
}

/// Find the end of a complete HTTP/1.1 request using the `httparse` crate.
/// This approach uses SIMD optimizations internally and avoids allocations.
pub fn find_complete_request(buf: &[u8]) -> Option<usize> {
    // Pre-allocate a fixed-size array of headers on the stack.
    // 32 headers are usually sufficient for standard requests.
    // If there are more headers, `httparse` will return a TooManyHeaders error
    // (mapped to None here), which acts as a safety limit against DoS.
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut req = httparse::Request::new(&mut headers);

    // Attempt to parse the buffer
    // `req.parse` returns the number of bytes consumed by the headers (the body offset)
    match req.parse(buf) {
        Ok(httparse::Status::Complete(headers_len)) => {
            let mut content_length = 0;

            // Iterate through the parsed headers to find Content-Length
            for header in req.headers {
                if header.name.eq_ignore_ascii_case("Content-Length") {
                    // Parse the byte slice directly to usize.
                    // Using std::str::from_utf8 + parse is safe and fast enough here,
                    // as `httparse` validates token structure.
                    if let Ok(s) = std::str::from_utf8(header.value) {
                        // trim() handles surrounding whitespace allowed by RFC
                        if let Ok(val) = s.trim().parse::<usize>() {
                            content_length = val;
                            break; // Found it, stop searching
                        }
                    }
                }
            }

            let total_len = headers_len + content_length;

            // Check if we have the full body in the buffer
            if buf.len() >= total_len {
                Some(total_len)
            } else {
                None // Body is not fully received yet
            }
        }
        // Returns None if headers are incomplete (Status::Partial) or invalid (Error)
        _ => None,
    }
}

async fn handle_request(config: Arc<ServerConfig>) -> Result<Response<Full<Bytes>>> {
    let mut builder = Response::builder().status(config.status);

    // Add configured headers
    for (k, v) in &config.headers {
        builder = builder.header(k, v);
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
