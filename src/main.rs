use anyhow::{Context, Result};
use clap::Parser;
use http_body_util::Full;
use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::server::conn::http2;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use socket2::{Domain, Protocol, Socket, Type};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::thread;
use tokio::io::duplex;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tracing::{error, info, warn};

#[derive(Parser, Clone, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Number of threads to spawn
    #[arg(short, long, default_value_t = num_cpus::get())]
    threads: usize,

    /// Port to listen on
    #[arg(short, long, default_value_t = 8080)]
    port: u16,

    /// HTTP Status code to return
    #[arg(short, long, default_value_t = 200)]
    status: u16,

    /// Response body (optional)
    #[arg(short, long)]
    body: Option<String>,

    /// Headers in "Name: Value" format
    #[arg(long)]
    header: Vec<String>,

    /// Use io_uring (Linux only)
    #[cfg(all(target_os = "linux", feature = "io_uring"))]
    #[arg(long)]
    uring: bool,

    /// Enable HTTP/2 support (with HTTP/1.1 fallback)
    #[arg(long)]
    http2: bool,
}

/// Configuration shared across threads
#[derive(Clone)]
struct ServerConfig {
    status: StatusCode,
    body: Bytes,
    headers: Vec<(String, String)>,
}

fn main() -> Result<()> {
    // Initialize tracing subscriber
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    // Parse headers
    let mut parsed_headers = Vec::new();
    for h in &args.header {
        if let Some((k, v)) = h.split_once(':') {
            parsed_headers.push((k.trim().to_string(), v.trim().to_string()));
        } else {
            warn!("Invalid header format '{}', ignoring", h);
        }
    }

    // Load body content - either from string or file if starts with @
    let body_content = load_body_content(args.body.as_deref())?;

    let status_code = StatusCode::from_u16(args.status).context("Invalid status code")?;

    let config = Arc::new(ServerConfig {
        status: status_code,
        body: body_content.clone(),
        headers: parsed_headers.clone(),
    });

    let addr = SocketAddr::from(([0, 0, 0, 0], args.port));
    info!("Starting server on {} with {} threads", addr, args.threads);

    let mut handles = Vec::new();

    for id in 0..args.threads {
        let config = config.clone();

        #[cfg(all(target_os = "linux", feature = "io_uring"))]
        let use_uring = args.uring;
        #[cfg(not(all(target_os = "linux", feature = "io_uring")))]
        let use_uring = false;

        let http2_enabled = args.http2;

        let handle = thread::spawn(move || {
            if let Err(e) = run_thread(id, addr, config, use_uring, http2_enabled) {
                error!("Thread {} error: {}", id, e);
            }
        });
        handles.push(handle);
    }

    // Wait for all threads to complete (they run forever unless error)
    for handle in handles {
        handle.join().unwrap();
    }

    Ok(())
}

fn run_thread(
    id: usize,
    addr: SocketAddr,
    config: Arc<ServerConfig>,
    _use_uring: bool,
    http2_enabled: bool,
) -> Result<()> {
    // Hyper implementation for Linux
    info!("Thread {} using Hyper runtime", id);
    #[cfg(all(target_os = "linux", feature = "io_uring"))]
    if _use_uring {
        run_thread_uring(id, addr, config, http2_enabled)
    } else {
        run_thread_hyper(id, addr, config, http2_enabled)
    }
    #[cfg(not(all(target_os = "linux", feature = "io_uring")))]
    run_thread_hyper(id, addr, config, http2_enabled)
}

fn run_thread_hyper(
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
                } else {
                    if let Err(err) = http1::Builder::new().serve_connection(io, service).await {
                        error!("Error serving HTTP/1.1 connection: {:?}", err);
                    }
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

#[cfg(all(target_os = "linux", feature = "io_uring"))]
fn run_thread_uring(
    id: usize,
    addr: SocketAddr,
    config: Arc<ServerConfig>,
    http2_enabled: bool,
) -> Result<()> {
    // io_uring implementation for Linux
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

fn load_body_content(body: Option<&str>) -> Result<Bytes> {
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

fn create_listener(addr: SocketAddr) -> Result<std::net::TcpListener> {
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
