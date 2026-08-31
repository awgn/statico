use crate::execute_delay;
use crate::options::Options;
use crate::pretty::PrettyPrint;
use crate::response::build_response;
use crate::port_counters;
use anyhow::Result;
use futures::stream::{select_all, unfold, StreamExt};
use hyper::Response;
use owo_colors::OwoColorize;

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
    let verbose = opts.verbose;

    let mut uring = io_uring::IoUring::builder();
    use crate::uring::UringConfigurator;
    uring.configure_uring(opts);

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
                let port = listener.local_addr().unwrap().port();
                Box::pin(unfold(listener, move |l| async move {
                    match l.accept().await {
                        Ok((stream, _addr)) => Some((Ok((stream, port)), l)),
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
                    handle_connection_monoio(stream, port, config, false, meter, delay, verbose)
                        .await
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
    verbose: u8,
) -> Result<usize> {
    use http_wire::WireDecode;

    if http2 {
        return Err(anyhow::anyhow!("HTTP/2 not supported with monoio"));
    }

    let mut response_buf = build_response(&config)?;
    let counters = meter.then(|| port_counters(port));

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
                Ok((req, req_len)) => {
                    requests_served += 1;
                    parsed += req_len;

                    if verbose > 0 {
                        println!("↩ {}:\n{}", "request".bold(), req.pretty(verbose));
                    }

                    if let Some(c) = counters {
                        c.record_request(req_len);
                    }

                    if let Some(d) = delay {
                        execute_delay(d).await;
                    }

                    if verbose > 0 {
                        let mut print_builder = Response::builder().status(config.status);
                        for (k, v) in &config.headers {
                            print_builder = print_builder.header(k, v);
                        }
                        let print_resp = print_builder.body(config.body.clone()).unwrap();
                        println!("↪ {}:\n{}", "response".bold(), print_resp.pretty(verbose));
                    }

                    // Reuse response_buf (monoio returns it after the write)
                    let (res, buf_back) = stream.write_all(response_buf).await;
                    response_buf = buf_back;
                    res?;

                    if let Some(c) = counters {
                        c.record_response(response_buf.len());
                    }
                }
                Err(_) => break, // Incomplete request or end of batch
            }
        }

        // Loop continues: common case is parsed == buf.len(), so fast path activates on next iteration
    }

    Ok(requests_served)
}
