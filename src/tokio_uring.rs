use crate::execute_delay;
use crate::options::Options;
use crate::PORT_COUNTERS;
use crate::REQUESTS;
use crate::REQUEST_BYTES;
use crate::RESPONSES;
use crate::RESPONSE_BYTES;
use anyhow::Result;
use futures::stream::{select_all, StreamExt};
use http_body_util::Full;
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
    addrs: Vec<SocketAddr>,
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
            // Create multiple listeners for each address
            let mut listeners = Vec::new();
            for addr in &addrs {
                let std_listener = match create_listener(*addr, opts) {
                    Ok(l) => l,
                    Err(e) => {
                        error!("Failed to create listener for {}: {}", addr, e);
                        return;
                    }
                };
                listeners.push(tokio_uring::net::TcpListener::from_std(std_listener));
            }

            info!(
                "Thread {} listening on {:?} (tokio_uring, entries: {}, sqpoll: {:?})",
                id, addrs, opts.uring_entries, opts.uring_sqpoll
            );

            // Convert each listener into a stream using unfold
            let streams: Vec<_> = listeners
                .into_iter()
                .map(|listener| {
                    Box::pin(futures::stream::unfold(listener, |l| async move {
                        match l.accept().await {
                            Ok((tcp_stream, _)) => {
                                Some((Ok((tcp_stream, l.local_addr().unwrap().port())), l))
                            }
                            Err(e) => {
                                error!("Accept error: {}", e);
                                // Continue accepting despite errors
                                Some((Err(e), l))
                            }
                        }
                    }))
                })
                .collect();

            let mut all_listeners = select_all(streams);

            loop {
                match all_listeners.next().await {
                    Some(Ok((tcp_stream, port))) => {
                        let config = config.clone();

                        // Spawn task to handle the connection with io_uring
                        tokio_uring::spawn(async move {
                            if let Err(e) = handle_connection_uring(
                                tcp_stream, port, config, false, meter, delay,
                            )
                            .await
                            {
                                error!("Error handling tokio_uring connection: {}", e);
                            }
                        });
                    }
                    Some(Err(e)) => {
                        error!("Thread {} accept error: {}", id, e);
                    }
                    None => {
                        error!("Thread {} all listeners closed", id);
                        break;
                    }
                }
            }
        });

    Ok(())
}

#[cfg(all(target_os = "linux", feature = "tokio_uring"))]
async fn handle_connection_uring(
    stream: tokio_uring::net::TcpStream,
    port: u16,
    config: Arc<ServerConfig>,
    http2: bool,
    meter: bool,
    delay: Option<Duration>,
) -> Result<usize> {
    use http_wire::WireDecode;
    use std::mem::MaybeUninit;

    if http2 {
        return Err(anyhow::anyhow!("HTTP/2 not supported with tokio_uring"));
    }

    let response_bytes = build_response_simple(&config).await?;
    let mut response_buf = response_bytes; // Reuse to avoid allocations on each write

    // Single buffer strategy for maximum performance:
    // - In the common case (complete request in one read), we parse directly with zero copies
    // - Only compact when we have leftover data from incomplete requests (rare case)
    let mut buf: Vec<u8> = Vec::with_capacity(8192);
    let mut parsed = 0; // Number of bytes already parsed/consumed
    let mut requests_served = 0;

    loop {
        // FAST PATH: If we've consumed all data, reset the buffer (zero-cost operation)
        if parsed == buf.len() && parsed > 0 {
            unsafe {
                buf.set_len(0);
            }
            parsed = 0;
        }
        // SLOW PATH: If we have many dead bytes at the beginning, compact the buffer
        // This only happens when requests are split across multiple reads (rare)
        else if parsed > 4096 {
            buf.copy_within(parsed.., 0);
            unsafe {
                buf.set_len(buf.len() - parsed);
            }
            parsed = 0;
        }

        // Ensure we have enough spare capacity for reading
        // Only reserve if needed to avoid unnecessary allocations in the common case
        let current_len = buf.len();
        if buf.capacity() - current_len < 4096 {
            buf.reserve(4096);
        }

        // TRICK: Create a temporary Vec from the spare capacity for tokio_uring's ownership-based API
        // This allows us to read directly into the buffer's unused capacity without copying.
        // tokio_uring takes ownership of the buffer, passes it to io_uring, and returns it.
        let spare_cap = buf.capacity() - current_len;

        let temp_buf =
            unsafe { Vec::from_raw_parts(buf.as_mut_ptr().add(current_len), 0, spare_cap) };

        let (result, temp_buf) = stream.read(temp_buf).await;
        std::mem::forget(temp_buf);

        let n = match result {
            Ok(0) => {
                break; // Connection closed (EOF)
            }
            Ok(n) => n,
            Err(e) => {
                return Err(e.into());
            }
        };

        // Update the main buffer's length and forget temp_buf to avoid double-free
        // (temp_buf points to the same memory as buf's spare capacity)
        unsafe {
            buf.set_len(current_len + n);
        }

        // Parse all complete requests in the buffer
        // This handles HTTP pipelining: multiple requests in a single read
        let mut headers = [const { MaybeUninit::uninit() }; 128];

        loop {
            match http_wire::request::FullRequest::decode_uninit(&buf[parsed..], &mut headers) {
                Ok((_, req_len)) => {
                    requests_served += 1;
                    parsed += req_len;

                    if meter {
                        REQUESTS.add(1);
                        REQUEST_BYTES.add(req_len);
                        let entry = PORT_COUNTERS.entry(port).or_default();
                        entry.requests.add(1);
                        entry.request_bytes.add(req_len);
                    }

                    if let Some(d) = delay {
                        execute_delay(d).await;
                    }

                    // Reuse response_buf (tokio_uring returns it after the write)
                    let (res, buf_back) = stream.write_all(response_buf).await;
                    response_buf = buf_back;
                    res?;

                    if meter {
                        RESPONSES.add(1);
                        RESPONSE_BYTES.add(response_buf.len());
                        let entry = PORT_COUNTERS.entry(port).or_default();
                        entry.responses.add(1);
                        entry.response_bytes.add(response_buf.len());
                    }
                }
                Err(_) => break, // Incomplete request or end of batch
            }
        }

        // Loop continues: common case is parsed == buf.len(), so fast path activates on next iteration
    }

    Ok(requests_served)
}

/// Build a static HTTP response and encode it to bytes once at connection start.
/// This pre-encodes the response to avoid re-encoding on every request, significantly
/// improving performance for static responses.
async fn build_response_simple(config: &ServerConfig) -> Result<Vec<u8>> {
    use http_wire::WireEncodeAsync;

    // Build the HTTP response with configured status code
    let mut builder = Response::builder().status(config.status);

    // Add all configured headers
    for (k, v) in &config.headers {
        builder = builder.header(k, v);
    }

    // Add Content-Length header if body is present
    if !config.body.is_empty() {
        builder = builder.header(CONTENT_LENGTH, config.body.len());
    }

    // Build the response and encode it to wire format (HTTP/1.1 bytes)
    let res = builder.body(Full::new(config.body.clone()))?;
    Ok(res.encode_async().await?.to_vec())
}
