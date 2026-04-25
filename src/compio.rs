use crate::create_listener;
use crate::execute_delay;
use crate::options::Options;
use crate::pretty::PrettyPrint;
use crate::response::build_response;
use crate::ServerConfig;
use crate::PORT_COUNTERS;
use crate::REQUESTS;
use crate::REQUEST_BYTES;
use crate::RESPONSES;
use crate::RESPONSE_BYTES;
use anyhow::Result;
use compio::io::{AsyncRead, AsyncWriteExt};
use compio::net::TcpListener;
use futures::stream::{select_all, unfold, StreamExt};
use hyper::Response;
use owo_colors::OwoColorize;
use std::mem::MaybeUninit;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tracing::error;

pub fn run_thread(
    id: usize,
    addrs: Vec<SocketAddr>,
    config: Arc<ServerConfig>,
    opts: &Options,
) -> Result<()> {
    use tracing::info;

    let delay = opts.delay;
    let meter = opts.meter;
    let tcp_nodelay = opts.tcp_nodelay;

    let mut proactor_builder = compio::driver::ProactorBuilder::new();
    crate::configure_compio!(opts, proactor_builder);

    let mut builder = compio::runtime::Runtime::builder();
    builder.with_proactor(proactor_builder);
    let rt = builder.build()?;

    rt.block_on(async move {
        // Create compio listeners from std listeners (SO_REUSEPORT already configured)
        let mut listeners = Vec::new();
        for addr in &addrs {
            let std_listener = match create_listener(*addr, opts) {
                Ok(l) => l,
                Err(e) => {
                    error!("Failed to create listener for {}: {}", addr, e);
                    return;
                }
            };
            let listener = match TcpListener::from_std(std_listener) {
                Ok(l) => l,
                Err(e) => {
                    error!("Failed to create compio listener: {}", e);
                    return;
                }
            };
            listeners.push(listener);
        }

        info!("Thread {} listening on {:?} (compio)", id, addrs);

        // Convert each listener into a stream using unfold
        // compio's TcpListener is !Send, but that's fine since everything stays
        // on this thread's single-threaded runtime.
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

        // Fan-in all listener streams into one
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

            // Apply TCP_NODELAY for lower latency if requested
            if tcp_nodelay {
                if let Err(e) = stream.set_nodelay(true) {
                    error!("Failed to set TCP_NODELAY: {}", e);
                }
            }

            let config = config.clone();
            let verbose = opts.verbose;

            // Spawn a local task to handle the connection.
            // compio's spawn() only requires F: Future + 'static (not Send),
            // so !Send types like TcpStream are perfectly fine here.
            // Task::detach() lets the task run to completion independently.
            compio::runtime::spawn(async move {
                if let Err(e) =
                    handle_connection_compio(stream, port, config, meter, delay, verbose).await
                {
                    error!("Error handling compio connection: {}", e);
                }
            })
            .detach();
        }
    });

    Ok(())
}

async fn handle_connection_compio(
    mut stream: compio::net::TcpStream,
    port: u16,
    config: Arc<ServerConfig>,
    meter: bool,
    delay: Option<Duration>,
    verbose: u8,
) -> Result<usize> {
    use http_wire::WireDecode;

    let mut response_buf = build_response(&config)?;

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
        // SLOW PATH: If we have many dead bytes at the beginning, compact the buffer.
        // This only happens when requests are split across multiple reads (rare).
        else if parsed > 4096 {
            buf.copy_within(parsed.., 0);
            unsafe {
                buf.set_len(buf.len() - parsed);
            }
            parsed = 0;
        }

        // Ensure we have enough spare capacity for reading.
        // Only reserve if needed to avoid unnecessary allocations in the common case.
        let current_len = buf.len();
        if buf.capacity() - current_len < 4096 {
            buf.reserve(4096);
        }

        // TRICK: Create a temporary Vec from the spare capacity for compio's ownership-based API.
        //
        // Compio, like monoio, is a completion-based runtime (hence the name): buffers are
        // handed off to io_uring by value and returned together with the result.
        // This means we cannot pass a raw &mut slice — we must pass an owned buffer implementing IoBufMut.
        //
        // We create a zero-length Vec that *points into* our main buffer's spare capacity.
        // IoBufMut for Vec<u8> will read/write starting at buf.as_mut_ptr() + buf.len(),
        // which is exactly where we want new data to land. After the await:
        //   - we mem::forget to avoid a double-free (it aliases buf's memory)
        //   - we manually advance buf's length by the number of bytes read
        let spare_cap = buf.capacity() - current_len;
        let temp_buf =
            unsafe { Vec::from_raw_parts(buf.as_mut_ptr().add(current_len), 0, spare_cap) };

        let compio::BufResult(result, temp_buf) = stream.read(temp_buf).await;
        // Forget the alias to avoid double-free; buf retains ownership of the memory
        std::mem::forget(temp_buf);

        let n = match result {
            Ok(0) => break, // EOF — remote closed the connection
            Ok(n) => n,
            Err(e) => return Err(anyhow::Error::new(e)),
        };

        // Extend the main buffer to include the newly read bytes
        unsafe {
            buf.set_len(current_len + n);
        }

        // Parse all complete requests present in the buffer.
        // This handles HTTP pipelining: multiple requests arriving in a single read.
        let mut headers = [const { MaybeUninit::uninit() }; 128];

        loop {
            match http_wire::request::FullRequest::decode_uninit(&buf[parsed..], &mut headers) {
                Ok((req, req_len)) => {
                    requests_served += 1;
                    parsed += req_len;

                    if verbose > 0 {
                        println!("↩ {}:\n{}", "request".bold(), req.pretty(verbose));
                        // trailers are not currently decoded by http_wire FullRequest, so not printed here
                    }

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

                    if verbose > 0 {
                        let mut print_builder = Response::builder().status(config.status);
                        for (k, v) in &config.headers {
                            print_builder = print_builder.header(k, v);
                        }
                        let print_resp = print_builder.body(config.body.clone()).unwrap();
                        println!("↪ {}:\n{}", "response".bold(), print_resp.pretty(verbose));
                    }

                    // write_all transfers ownership of response_buf to io_uring, then returns
                    // it back in the BufResult — identical ownership dance to monoio.
                    let compio::BufResult(res, buf_back) = stream.write_all(response_buf).await;
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
                Err(_) => break, // Incomplete request — wait for more data
            }
        }
    }

    Ok(requests_served)
}
