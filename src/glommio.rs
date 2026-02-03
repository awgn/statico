use crate::execute_delay;
use crate::options::Options;
use crate::PORT_COUNTERS;
use crate::REQUESTS;
use crate::REQUEST_BYTES;
use crate::RESPONSES;
use crate::RESPONSE_BYTES;
use anyhow::Result;
use futures::stream::{select_all, unfold, StreamExt};
use futures::{AsyncReadExt, AsyncWriteExt};
use http_body_util::Full;

use hyper::header::CONTENT_LENGTH;
use hyper::Response;
use std::mem::MaybeUninit;
use std::net::SocketAddr;
use std::os::unix::io::{FromRawFd, IntoRawFd};
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
    use glommio::LocalExecutorBuilder;
    use tracing::info;

    let delay = opts.delay;
    let meter = opts.meter;
    let tcp_nodelay = opts.tcp_nodelay;

    let cpu_set =
        core_affinity::get_core_ids().ok_or(anyhow::anyhow!("Failed to get thread affinity"))?;
    let num_cpus = cpu_set.len();
    let cpu_core = cpu_set.into_iter().nth(id % num_cpus).unwrap();

    let num_entries = opts.uring_entries.next_power_of_two();

    // Create multiple sockets manually with SO_REUSEPORT enabled before moving into the closure
    let mut raw_fds = Vec::new();
    for addr in &addrs {
        let std_listener = create_listener(*addr, opts)?;
        raw_fds.push(std_listener.into_raw_fd());
    }

    let builder = LocalExecutorBuilder::new(glommio::Placement::Fixed(cpu_core.id))
        .ring_depth(num_entries as usize);

    let handle = builder
        .name(&format!("glommio-{}", id))
        .spawn(move || async move {
            // Create glommio listeners from raw fds
            let listeners: Vec<glommio::net::TcpListener> = raw_fds
                .into_iter()
                .map(|fd| unsafe { glommio::net::TcpListener::from_raw_fd(fd) })
                .collect();

            // Combine all listeners into a single stream
            let mut all_listeners =
                select_all(listeners.into_iter().map(|l: glommio::net::TcpListener| {
                    let port = l.local_addr().unwrap().port();
                    Box::pin(unfold(
                        l,
                        move |listener: glommio::net::TcpListener| async move {
                            match listener.accept().await {
                                Ok(stream) => Some((Ok((stream, port)), listener)),
                                Err(e) => Some((Err(e), listener)),
                            }
                        },
                    ))
                }));

            info!(
                "Thread {} listening on {:?} (glommio cpu-{})",
                id, addrs, cpu_core.id
            );

            loop {
                let (stream, port) = match all_listeners.next().await {
                    Some(Ok((s, port))) => (s, port),
                    Some(Err(e)) => {
                        error!("Thread {} accept error: {}", id, e);
                        continue;
                    }
                    None => {
                        error!("Thread {} all listeners closed", id);
                        break;
                    }
                };

                // Enable TCP_NODELAY for lower latency
                if tcp_nodelay {
                    if let Err(e) = stream.set_nodelay(true) {
                        error!("Failed to set TCP_NODELAY: {}", e);
                    }
                }

                let config = config.clone();

                // Spawn task to handle the connection with glommio
                glommio::spawn_local(async move {
                    if let Err(e) =
                        handle_connection_glommio(stream, port, config, false, meter, delay).await
                    {
                        error!("Error handling glommio connection: {}", e);
                    }
                })
                .detach();
            }
        })
        .map_err(|e| anyhow::anyhow!("Failed to spawn glommio executor: {:?}", e))?;

    handle.join().unwrap();
    Ok(())
}

async fn handle_connection_glommio(
    mut stream: glommio::net::TcpStream,
    port: u16,
    config: Arc<ServerConfig>,
    http2: bool,
    meter: bool,
    delay: Option<Duration>,
) -> Result<usize> {
    use http_wire::WireDecode;

    if http2 {
        return Err(anyhow::anyhow!("HTTP/2 not supported with glommio"));
    }

    let response_bytes = build_response_simple(&config).await?;

    // Single buffer strategy for maximum performance:
    // - In the common case (complete request in one read), we parse directly with zero copies
    // - Only compact when we have leftover data from incomplete requests (rare case)
    // - Glommio's read() accepts a slice, so we can read directly into spare capacity without unsafe tricks
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

        // Read directly into the buffer's spare capacity using a mutable slice
        // Glommio accepts &mut [u8], which is simpler than monoio/tokio_uring's ownership model
        // We temporarily extend the buffer to its capacity, read into the spare portion, then adjust
        let capacity = buf.capacity();
        unsafe {
            buf.set_len(capacity);
        }

        let n = match stream.read(&mut buf[current_len..]).await {
            Ok(0) => break, // Connection closed (EOF)
            Ok(n) => n,
            Err(e) => {
                unsafe {
                    buf.set_len(current_len);
                }
                return Err(anyhow::Error::new(e));
            }
        };

        // Update the buffer's length to include only the newly read data
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

                    // Write the pre-encoded response
                    stream.write_all(&response_bytes).await?;

                    if meter {
                        RESPONSES.add(1);
                        RESPONSE_BYTES.add(response_bytes.len());
                        let entry = PORT_COUNTERS.entry(port).or_default();
                        entry.responses.add(1);
                        entry.response_bytes.add(response_bytes.len());
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
