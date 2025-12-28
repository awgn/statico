use anyhow::{Context, Result};
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

use crate::ServerConfig;

pub fn run_thread(
    id: usize,
    addr: SocketAddr,
    config: Arc<ServerConfig>,
    http2_enabled: bool,
) -> Result<()> {
    // Standard Tokio single-thread runtime - create socket with SO_REUSEPORT
    let std_listener = create_listener(addr)?;
    std_listener.set_nonblocking(true)?;

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

            // Spawn task to handle the connection
            tokio::task::spawn(async move {
                let service = service_fn(move |_req: Request<hyper::body::Incoming>| {
                    let config = config.clone();
                    async move { handle_request(config).await }
                });

                if http2_enabled {
                    if let Err(err) = http2::Builder::new(hyper_util::rt::TokioExecutor::new())
                        .serve_connection(io, service)
                        .await
                    {
                        error!("Error serving HTTP/2 connection: {:?}", err);
                    }
                } else if let Err(err) = http1::Builder::new().serve_connection(io, service).await {
                    error!("Error serving HTTP/1.1 connection: {:?}", err);
                }
            });
        }
    })
}

async fn handle_request(config: Arc<ServerConfig>) -> Result<Response<Full<Bytes>>> {
    let mut builder = Response::builder().status(config.status);

    // Add configured headers
    for (k, v) in &config.headers {
        builder = builder.header(k, v);
    }

    Ok(builder.body(Full::new(config.body.clone()))?)
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

pub fn create_listener(addr: SocketAddr) -> Result<std::net::TcpListener> {
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

    socket.set_tcp_nodelay(true)?;
    socket.bind(&addr.into())?;
    socket.listen(1024)?;

    Ok(socket.into())
}
