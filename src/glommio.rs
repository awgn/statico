use crate::execute_delay;
use crate::options::Options;
use crate::REQUESTS;
use crate::REQUEST_BYTES;
use crate::RESPONSES;
use crate::RESPONSE_BYTES;
use anyhow::Result;
use futures_lite::io::{AsyncReadExt, AsyncWriteExt};
use http_body_util::Full;
use hyper::body::Bytes;
use hyper::header::CONTENT_LENGTH;
use hyper::Response;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tracing::error;

use crate::ServerConfig;

pub fn run_thread(
    id: usize,
    addr: SocketAddr,
    config: Arc<ServerConfig>,
    opts: &Options,
) -> Result<()> {
    use glommio::{CpuSet, LocalExecutorBuilder};
    use tracing::info;

    let delay = opts.delay;
    let meter = opts.meter;
    let tcp_nodelay = opts.tcp_nodelay;

    let cpu_set = CpuSet::online().unwrap();
    let cpu = cpu_set.iter().nth(id).unwrap();
    let num_entries = opts.uring_entries.next_power_of_two();

    let builder = LocalExecutorBuilder::new(glommio::Placement::Fixed(cpu.core))
        .ring_depth(num_entries as usize);

    let handle = builder
        .name(&format!("glommio-{}", id))
        .spawn(move || async move {
            // glommio's bind already uses SO_REUSEPORT on Linux
            let listener = glommio::net::TcpListener::bind(addr).unwrap();

            info!("Thread {} listening on {} (glommio)", id, addr);

            loop {
                let stream = match listener.accept().await {
                    Ok(s) => s,
                    Err(e) => {
                        error!("Thread {} accept error: {}", id, e);
                        continue;
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
                        handle_connection_glommio(stream, config, false, meter, delay).await
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
    config: Arc<ServerConfig>,
    http2: bool,
    meter: bool,
    delay: Option<Duration>,
) -> Result<usize> {
    use http_wire::{WireDecode, WireEncode};

    if http2 {
        return Err(anyhow::anyhow!("HTTP/2 not supported with glommio"));
    }

    // Pre-calculate the static response once.
    let response_bytes = match build_response(config.clone()).await {
        Ok(res) => match res.encode() {
            Ok(bytes) => bytes.to_vec(),
            Err(_) => return Ok(0),
        },
        Err(_) => return Ok(0),
    };

    let mut read_buf = vec![0u8; 16384];
    let mut conn_buf = Vec::with_capacity(32768);
    let mut requests_served = 0;
    let mut buf_processed = 0;
    let response_buf = response_bytes;

    loop {
        let n = match stream.read(&mut read_buf).await {
            Ok(0) => break, // EOF
            Ok(n) => n,
            Err(e) => return Err(anyhow::Error::new(e)),
        };

        let parse_slice: &[u8];
        let mut using_internal = false;

        if conn_buf.is_empty() {
            parse_slice = &read_buf[..n];
        } else {
            if conn_buf.len() + n > conn_buf.capacity() {
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

            stream.write_all(&response_buf).await?;

            consumed_in_batch += req_len;

            if consumed_in_batch < loop_slice.len() {
                loop_slice = &parse_slice[consumed_in_batch..];
            } else {
                break;
            }
        }

        if using_internal {
            buf_processed += consumed_in_batch;
            if buf_processed == conn_buf.len() {
                conn_buf.clear();
                buf_processed = 0;
            } else if buf_processed > 4096 && buf_processed > conn_buf.len() / 2 {
                conn_buf.drain(..buf_processed);
                buf_processed = 0;
            }
        } else if consumed_in_batch < n {
            conn_buf.extend_from_slice(&read_buf[consumed_in_batch..n]);
            buf_processed = 0;
        }
    }

    Ok(requests_served)
}

async fn build_response(config: Arc<ServerConfig>) -> Result<Response<Full<Bytes>>> {
    let mut builder = Response::builder().status(config.status);

    for (k, v) in &config.headers {
        builder = builder.header(k, v);
    }

    if !config.body.is_empty() {
        builder = builder.header(CONTENT_LENGTH, config.body.len());
    }

    Ok(builder.body(Full::new(config.body.clone()))?)
}
