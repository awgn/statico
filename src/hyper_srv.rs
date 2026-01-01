use anyhow::{Context, Result};
use http_body_util::BodyExt;
use http_body_util::Full;
use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::server::conn::http2;
use hyper::service::service_fn;

use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use socket2::{Domain, Protocol, Socket, Type};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tracing::{error, info, warn};

use crate::pretty::PrettyPrint;
use crate::{Args, ServerConfig};

pub fn run_thread(id: usize, addr: SocketAddr, config: Arc<ServerConfig>, args: &Args) -> Result<()> {
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

            // Spawn task to handle the connection
            tokio::task::spawn(async move {
                let service = service_fn(move |req: Request<hyper::body::Incoming>| {
                    let config = config.clone();
                    async move {
                        if verbose > 0 {
                            if let Ok(req) = collect_request(req).await {
                                match verbose {
                                    1 => println!("↩ request:\n{}", req.pretty()),
                                    _ => println!("↩ request:\n{:#}", req.pretty()),
                                }
                            }
                        }
                        let mut builder = Response::builder().status(config.status);

                        // Add configured headers
                        for (k, v) in &config.headers {
                            builder = builder.header(k, v);
                        }

                        let resp = builder.body(Full::new(config.body.clone()));
                        if let Ok(ref resp) = resp {
                            if verbose > 0 {
                                let Ok(ref resp) = collect_response(resp.clone()).await;
                                match verbose {
                                    1 => println!("↪ response:\n{}", resp.pretty()),
                                    _ => println!("↪ response:\n{:#}", resp.pretty()),
                                }
                            }
                        }
                        resp
                    }
                });

                let result = if use_http2 {
                    http2::Builder::new(hyper_util::rt::TokioExecutor::new())
                        .serve_connection(io, service)
                        .await
                } else {
                    http1::Builder::new()
                        .serve_connection(io, service)
                        .await
                };

                if let Err(err) = result {
                    let protocol = if use_http2 { "HTTP/2" } else { "HTTP/1.1" };
                    error!("Error serving {} connection: {:?}", protocol, err);
                }
            });
        }
    })
}

pub async fn collect_request<B>(req: Request<B>) -> Result<Request<Bytes>, B::Error>
where
    B: http_body::Body,
{
    let (parts, body) = req.into_parts();
    let collected = body.collect().await?;
    let bytes = collected.to_bytes();
    Ok(Request::from_parts(parts, bytes))
}

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
