use anyhow::{Result};
use http_body_util::Full;
use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::server::conn::http2;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use tracing::error;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::duplex;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;

use crate::ServerConfig;
use crate::hyper_srv::create_listener;

pub fn run_thread(
    id: usize,
    addr: SocketAddr,
    config: Arc<ServerConfig>,
    http2_enabled: bool,
) -> Result<()> {
    // io_uring implementation for Linux

    use tracing::info;
    info!("Thread {} using io_uring runtime", id);
    tokio_uring::builder()
        .entries(32768) // Large ring size is critical for throughput
        .uring_builder(
            tokio_uring::uring_builder()
                .setup_cqsize(65536)
                .setup_sqpoll(1),
        )
        .start(async move {
            // Create socket manually with SO_REUSEPORT enabled
            let std_listener = create_listener(addr)?;
            std_listener.set_nonblocking(true)?;
            let listener = tokio_uring::net::TcpListener::from_std(std_listener);
            info!("Thread {} listening on {} (io_uring)", id, addr);

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
                    if let Err(e) = handle_connection_uring(stream, config, http2_enabled).await {
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
    // Reuse buffer for reading
    let mut buf = vec![0u8; 4096];

    // Build response using handle_request and to_bytes
    let resp = handle_request(config.clone()).await?;
    let mut response = to_bytes(resp, http2).await;
    let mut requests_served = 0;

    loop {
        // Read HTTP request
        let (result, b) = stream.read(buf).await;
        buf = b; // Get buffer back

        let n = match result {
            Ok(0) => break, // Connection closed normally by client
            Ok(n) => n,
            Err(e) => return Err(e.into()), // Read error
        };

        // Simple check if we got some data
        if n > 0 {
            requests_served += 1;

            // Write response
            let (result, r) = stream.write_all(response).await;
            response = r; // Get buffer back for next iteration

            if let Err(e) = result {
                return Err(e.into()); // Write error
            }
        }
    }

    Ok(requests_served)
}

async fn handle_request(config: Arc<ServerConfig>) -> Result<Response<Full<Bytes>>> {
    let mut builder = Response::builder().status(config.status);

    // Add configured headers
    for (k, v) in &config.headers {
        builder = builder.header(k, v);
    }

    Ok(builder.body(Full::new(config.body.clone()))?)
}

pub async fn to_bytes(response: Response<Full<Bytes>>, http2: bool) -> Vec<u8> {
    let (mut client, server) = duplex(1024 * 64);
    tokio::spawn(async move {
        let service = service_fn(move |_req: Request<hyper::body::Incoming>| {
            let res = response.clone();
            async move { Ok::<_, Infallible>(res) }
        });

        if http2 {
            let _ = http2::Builder::new(hyper_util::rt::TokioExecutor::new())
                .serve_connection(TokioIo::new(server), service)
                .await;
        } else {
            let _ = http1::Builder::new()
                //.keep_alive(false)
                .serve_connection(TokioIo::new(server), service)
                .await;
        }
    });

    if http2 {
        // Send HTTP/2 connection preface
        let _ = client.write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n").await;
        // Send SETTINGS frame
        let _ = client
            .write_all(&[0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00])
            .await;
        // Send HEADERS frame with simple GET request (stream 1)
        let _ = client
            .write_all(&[
                0x00, 0x00, 0x05, 0x01, 0x05, 0x00, 0x00, 0x00, 0x01, 0x82, 0x86, 0x84, 0x41, 0x8a,
            ])
            .await;
    } else {
        let _ = client
            .write_all(b"GET / HTTP/1.1\r\nHost: local\r\n\r\n")
            .await;
    }

    let mut bytes = Vec::new();

    if http2 {
        // For HTTP/2, we still need connection close to know when response is done
        // TODO: proper HTTP/2 frame parsing
        let _ = client.shutdown().await;
        let _ = client.read_to_end(&mut bytes).await;
    } else {
        // For HTTP/1.1, parse headers to get Content-Length
        let mut header_buf = Vec::new();
        let mut single_byte = [0u8; 1];

        // Read until we find "\r\n\r\n" (end of headers)
        loop {
            if client.read_exact(&mut single_byte).await.is_err() {
                break;
            }
            header_buf.push(single_byte[0]);

            if header_buf.len() >= 4 {
                let len = header_buf.len();
                if &header_buf[len-4..len] == b"\r\n\r\n" {
                    break;
                }
            }
        }

        bytes.extend_from_slice(&header_buf);

        // Parse Content-Length from headers
        let headers_str = String::from_utf8_lossy(&header_buf);
        let mut content_length = 0;

        for line in headers_str.lines() {
            if let Some(value) = line.strip_prefix("content-length:") {
                content_length = value.trim().parse().unwrap_or(0);
                break;
            } else if let Some(value) = line.strip_prefix("Content-Length:") {
                content_length = value.trim().parse().unwrap_or(0);
                break;
            }
        }

        // Read exactly content_length bytes for the body
        if content_length > 0 {
            let mut body = vec![0u8; content_length];
            let _ = client.read_exact(&mut body).await;
            bytes.extend_from_slice(&body);
        }
    }

    bytes
}
