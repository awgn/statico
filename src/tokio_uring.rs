use crate::execute_delay;
use crate::options::Options;
use crate::REQUESTS;
use crate::REQUEST_BYTES;
use crate::RESPONSES;
use crate::RESPONSE_BYTES;
use anyhow::Result;
use http_body_util::Full;
use hyper::body::Bytes;
use hyper::header::CONTENT_LENGTH;
use hyper::Response;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tracing::error;

use crate::create_listener;
use crate::ServerConfig;

pub fn run_thread(
    id: usize,
    addr: SocketAddr,
    config: Arc<ServerConfig>,
    opts: &Options,
) -> Result<()> {
    // tokio io_uring implementation for Linux
    use tracing::info;

    let num_entries = opts.uring_entries.next_power_of_two();
    let cqsize = num_entries * 2;
    let delay = opts.delay;

    let mut uring = tokio_uring::uring_builder();

    uring.setup_single_issuer().setup_cqsize(cqsize);

    if let Some(idle) = opts.uring_sqpoll {
        uring.setup_sqpoll(idle);
    } else {
        uring.setup_coop_taskrun().setup_taskrun_flag();
    }

    let meter = opts.meter;

    tokio_uring::builder()
        .entries(num_entries) // Large ring size is critical for throughput
        .uring_builder(&uring)
        .start(async move {
            // Create socket manually with SO_REUSEPORT enabled
            let std_listener = create_listener(addr, opts)?;
            let listener = tokio_uring::net::TcpListener::from_std(std_listener);
            info!(
                "Thread {} listening on {} (tokio_uring, entries: {}, sqpoll: {:?})",
                id, addr, opts.uring_entries, opts.uring_sqpoll
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
                    if let Err(e) =
                        handle_connection_uring(stream, config, false, meter, delay).await
                    {
                        error!("Error handling tokio_uring connection: {}", e);
                    }
                });
            }
        })
}

#[cfg(all(target_os = "linux", feature = "tokio_uring"))]
async fn handle_connection_uring(
    stream: tokio_uring::net::TcpStream,
    config: Arc<ServerConfig>,
    http2: bool,
    meter: bool,
    delay: Option<Duration>,
) -> Result<usize> {
    use http_wire::{WireDecode, WireEncodeAsync};

    if http2 {
        // tracing::warn!("HTTP/2 is not supported with io_uring raw TCP");
        return Err(anyhow::anyhow!("HTTP/2 not supported with tokio_uring"));
    }

    // Pre-calculate the static response once.
    let response_bytes = match build_response(config.clone()).await {
        Ok(res) => match res.encode_async().await {
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
        while let Some(req_len) = http_wire::request::RequestLength::decode(loop_slice) {
            requests_served += 1;
            if meter {
                REQUESTS.add(1);
                REQUEST_BYTES.add(req_len);
                RESPONSES.add(1);
                RESPONSE_BYTES.add(response_buf.len());
            }

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
