use anyhow::{Context, Result};
use http_body_util::BodyExt;
use http_body_util::Full;
use hyper::body::Bytes;
use hyper::header::{CONTENT_LENGTH, TRANSFER_ENCODING};
use hyper::server::conn::http1;
use hyper::server::conn::http2;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use owo_colors::OwoColorize;
use pingora_timeout::fast_timeout::fast_sleep;
use socket2::{Domain, Protocol, Socket, Type};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tracing::{error, info, warn};

use crate::http::{request_head_size, response_head_size};
use crate::REQUESTS;
use crate::REQUEST_BYTES;
use crate::RESPONSES;
use crate::RESPONSE_BYTES;

use crate::pretty::PrettyPrint;
use crate::{Args, ServerConfig};

pub fn run_thread(
    id: usize,
    addr: SocketAddr,
    config: Arc<ServerConfig>,
    args: &Args,
) -> Result<()> {
    // Standard Tokio single-thread runtime - create socket with SO_REUSEPORT
    let std_listener = create_listener(addr, args)?;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    rt.block_on(async move {
        let listener = TcpListener::from_std(std_listener)?;
        info!("Thread {} listening on {}", id, addr);

        loop {
            let (stream, _) = match listener.accept().await {
                Ok(s) => s,
                Err(e) => {
                    error!("Thread {} accept error: {}", id, e);
                    continue;
                }
            };

            let io = TokioIo::new(stream);
            let config = config.clone();
            let use_http2 = args.http2;
            let verbose = args.verbose;
            let delay = args.delay;
            let meter = args.meter;

            // Spawn task to handle the connection
            tokio::task::spawn(async move {
                let service = service_fn(move |req: Request<hyper::body::Incoming>| {
                    let config = config.clone();
                    async move {
                        let (head_size, is_chunked) = if meter {
                            let hs = request_head_size(&req);
                            let chunked = req
                                .headers()
                                .get(TRANSFER_ENCODING)
                                .and_then(|v| v.to_str().ok())
                                .map(|s| s.contains("chunked"))
                                .unwrap_or(false);
                            (hs, chunked)
                        } else {
                            (0, false)
                        };

                        // Collect request body if metering or verbose mode is enabled
                        let collected_req = if meter || verbose > 0 {
                            collect_request(req).await.ok()
                        } else {
                            None
                        };

                        if meter {
                            REQUESTS.add(1);
                            if let Some(ref req) = collected_req {
                                let body_bytes = req.body().len();
                                // For chunked encoding, calculate the wire format overhead
                                let body_size = if is_chunked {
                                    chunked_body_wire_size(body_bytes)
                                } else {
                                    body_bytes
                                };
                                REQUEST_BYTES.add(head_size + body_size);
                            } else {
                                REQUEST_BYTES.add(head_size);
                            }
                        }

                        if verbose > 0 {
                            if let Some(ref req) = collected_req {
                                println!("↩ {}:\n{}", "request".bold(), req.pretty(verbose));
                            }
                        }
                        let mut builder = Response::builder().status(config.status);

                        // Add configured headers
                        for (k, v) in &config.headers {
                            builder = builder.header(k, v);
                        }

                        // Always add Content-Length
                        if !config.body.is_empty() {
                            builder = builder.header(CONTENT_LENGTH, config.body.len());
                        }

                        let resp = builder.body(Full::new(config.body.clone()));
                        if let Ok(ref resp) = resp {
                            if meter {
                                RESPONSES.add(1);
                                let head_size = response_head_size(resp, config.body.len());
                                RESPONSE_BYTES.add(head_size + config.body.len());
                            }
                            if verbose > 0 {
                                let resp = collect_response(resp.clone()).await.unwrap();
                                println!("↪ {}:\n{}", "response".bold(), resp.pretty(verbose));
                            }
                        }

                        if let Some(delay) = delay {
                            execute_delay(delay).await;
                        }

                        resp
                    }
                });

                let result = if use_http2 {
                    http2::Builder::new(hyper_util::rt::TokioExecutor::new())
                        .serve_connection(io, service)
                        .await
                } else {
                    http1::Builder::new().serve_connection(io, service).await
                };

                if let Err(err) = result {
                    let protocol = if use_http2 { "HTTP/2" } else { "HTTP/1.1" };
                    error!("Error serving {} connection: {:?}", protocol, err);
                }
            });
        }
    })
}

#[cold]
async fn execute_delay(delay: std::time::Duration) {
    fast_sleep(delay).await;
}

#[inline]
pub async fn collect_request<B>(req: Request<B>) -> Result<Request<Bytes>, B::Error>
where
    B: http_body::Body,
{
    let (parts, body) = req.into_parts();
    let collected = body.collect().await?;
    let bytes = collected.to_bytes();
    Ok(Request::from_parts(parts, bytes))
}

/// Calculate the wire size of a chunked body given the decoded body size.
///
/// Chunked encoding format for a single chunk:
/// <size in hex>\r\n<data>\r\n
/// Plus the terminating chunk: 0\r\n\r\n
///
/// For simplicity, we assume the body is sent as a single chunk.
fn chunked_body_wire_size(body_len: usize) -> usize {
    if body_len == 0 {
        // Just the terminating chunk: "0\r\n\r\n"
        5
    } else {
        // Calculate hex digits needed for the size
        let hex_digits = format!("{:x}", body_len).len();
        // <hex_size>\r\n<body>\r\n + terminating "0\r\n\r\n"
        hex_digits + 2 + body_len + 2 + 5
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chunked_body_wire_size_empty() {
        // Empty body: just "0\r\n\r\n"
        assert_eq!(chunked_body_wire_size(0), 5);
    }

    #[test]
    fn test_chunked_body_wire_size_small() {
        // Body "1234" (4 bytes):
        // "4\r\n" (3) + "1234\r\n" (6) + "0\r\n\r\n" (5) = 14
        assert_eq!(chunked_body_wire_size(4), 14);
    }

    #[test]
    fn test_chunked_body_wire_size_two_hex_digits() {
        // Body of 16 bytes (hex "10"):
        // "10\r\n" (4) + <16 bytes>\r\n (18) + "0\r\n\r\n" (5) = 27
        assert_eq!(chunked_body_wire_size(16), 27);
    }

    #[test]
    fn test_chunked_body_wire_size_large() {
        // Body of 4096 bytes (hex "1000"):
        // "1000\r\n" (6) + <4096 bytes>\r\n (4098) + "0\r\n\r\n" (5) = 4109
        assert_eq!(chunked_body_wire_size(4096), 4109);
    }
}

#[inline]
pub async fn collect_response<B>(res: Response<B>) -> Result<Response<Bytes>, B::Error>
where
    B: http_body::Body,
{
    let (parts, body) = res.into_parts();
    let collected = body.collect().await?;
    let bytes = collected.to_bytes();
    Ok(Response::from_parts(parts, bytes))
}

pub fn load_body_content(body: Option<&str>) -> Result<Bytes> {
    match body {
        Some(content) if content.starts_with('@') => {
            // Remove @ prefix and treat as file path
            let file_path = &content[1..];
            info!("Loading body content from file: {}", file_path);
            let file_content = std::fs::read_to_string(file_path)
                .with_context(|| format!("Failed to read body file: {}", file_path))?;
            Ok(Bytes::from(file_content))
        }
        Some(content) => Ok(Bytes::from(content.to_string())),
        None => Ok(Bytes::new()),
    }
}

pub fn create_listener(addr: SocketAddr, args: &Args) -> Result<std::net::TcpListener> {
    let domain = if addr.is_ipv6() {
        Domain::IPV6
    } else {
        Domain::IPV4
    };
    let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;

    // Enable SO_REUSEPORT on all Unix systems that support it
    #[cfg(unix)]
    {
        if let Err(e) = socket.set_reuse_port(true) {
            warn!("SO_REUSEPORT failed: {}. Falling back to SO_REUSEADDR", e);
            socket.set_reuse_address(true)?;
        }
    }

    // On non-Unix systems, use SO_REUSEADDR
    #[cfg(not(unix))]
    {
        socket.set_reuse_address(true)?;
    }

    // Apply TCP_NODELAY if requested
    if args.tcp_nodelay {
        socket.set_tcp_nodelay(true)?;
    }

    // Apply receive buffer size if specified
    if let Some(size) = args.receive_buffer_size {
        socket.set_recv_buffer_size(size)?;
    }

    // Apply send buffer size if specified
    if let Some(size) = args.send_buffer_size {
        socket.set_send_buffer_size(size)?;
    }

    socket.bind(&addr.into())?;
    socket.listen(args.listen_backlog.unwrap_or(1024))?;

    // Set nonblocking mode
    socket.set_nonblocking(true)?;

    Ok(socket.into())
}
