use crate::execute_delay;
use crate::options::Options;
use crate::PORT_COUNTERS;
use crate::REQUESTS;
use crate::REQUEST_BYTES;
use crate::RESPONSES;
use crate::RESPONSE_BYTES;
use anyhow::Result;
use futures::stream::{select_all, unfold, StreamExt};
use http_body_util::Full;

use hyper::header::CONTENT_LENGTH;
use hyper::Response;
use monoio::io::AsyncReadRent;
use monoio::io::AsyncWriteRentExt;
use monoio::time::TimeDriver;
use monoio::IoUringDriver;
use monoio::RuntimeBuilder;
use std::mem::MaybeUninit;
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
    // monoio implementation for Linux
    use tracing::info;

    let delay = opts.delay;
    let meter = opts.meter;

    let num_entries = opts.uring_entries.next_power_of_two();
    let cqsize = num_entries * 2;

    let mut uring = io_uring::IoUring::builder();
    uring.setup_single_issuer().setup_cqsize(cqsize);

    if let Some(idle) = opts.uring_sqpoll {
        uring.setup_sqpoll(idle);
    } else {
        uring.setup_coop_taskrun().setup_taskrun_flag();
    }

    let builder: RuntimeBuilder<TimeDriver<IoUringDriver>> = monoio::RuntimeBuilder::new()
        .enable_all()
        .uring_builder(uring);
    let mut rt = builder.build().unwrap();

    rt.block_on(async move {
        // Create multiple sockets manually with SO_REUSEPORT enabled
        let mut listeners = Vec::new();
        for addr in &addrs {
            let std_listener = match create_listener(*addr, opts) {
                Ok(l) => l,
                Err(e) => {
                    error!("Failed to create listener for {}: {}", addr, e);
                    return;
                }
            };
            let Ok(listener) = monoio::net::TcpListener::from_std(std_listener) else {
                panic!("Failed to create monoio listener");
            };
            listeners.push(listener);
        }

        info!("Thread {} listening on {:?} (monoio)", id, addrs);

        // Convert each listener into a stream using unfold
        let streams: Vec<_> = listeners
            .into_iter()
            .map(|listener| {
                Box::pin(unfold(listener, |l| async {
                    match l.accept().await {
                        Ok((stream, _addr)) => {
                            Some((Ok((stream, l.local_addr().unwrap().port())), l))
                        }
                        Err(e) => Some((Err(e), l)),
                    }
                }))
            })
            .collect();

        // Combine all listener streams
        let mut all_listeners = select_all(streams);

        loop {
            let (stream, port) = match all_listeners.next().await {
                Some(Ok(s)) => s,
                Some(Err(e)) => {
                    error!("Thread {} accept error: {}", id, e);
                    continue;
                }
                None => {
                    error!("Thread {} all listeners closed", id);
                    break;
                }
            };

            let config = config.clone();

            // Spawn task to handle the connection with monoio
            monoio::spawn(async move {
                if let Err(e) =
                    handle_connection_monoio(stream, port, config, false, meter, delay).await
                {
                    error!("Error handling monoio connection: {}", e);
                }
            });
        }
    });

    Ok(())
}

async fn handle_connection_monoio(
    mut stream: monoio::net::TcpStream,
    port: u16,
    config: Arc<ServerConfig>,
    http2: bool,
    meter: bool,
    delay: Option<Duration>,
) -> Result<usize> {
    use http_wire::WireDecode;

    if http2 {
        return Err(anyhow::anyhow!("HTTP/2 not supported with monoio"));
    }

    let mut response_buf = build_response_simple(&config).await?;

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
        // SLOW PATH: If we have dead bytes at the beginning, compact the buffer
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

        // TRICK: Create a temporary Vec from the spare capacity for monoio's rent-based API
        // This allows us to read directly into the buffer's unused capacity without copying.
        // Monoio takes ownership of the buffer, passes it to io_uring, and returns it.
        let spare_cap = buf.capacity() - current_len;

        let temp_buf =
            unsafe { Vec::from_raw_parts(buf.as_mut_ptr().add(current_len), 0, spare_cap) };

        let (result, temp_buf) = stream.read(temp_buf).await;
        // forget the temporary buffer to avoid double-free
        std::mem::forget(temp_buf);

        let n = match result {
            Ok(0) => {
                break; // Connection closed (EOF)
            }
            Ok(n) => n,
            Err(e) => {
                return Err(anyhow::Error::new(e));
            }
        };

        // Update the main buffer's length...
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

                    // Reuse response_buf (monoio returns it after the write)
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
    use http_wire::WireEncode;

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
    Ok(res.encode()?.to_vec())
}
