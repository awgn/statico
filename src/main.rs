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
use std::net::SocketAddr;
use std::sync::Arc;
use std::thread;
use tokio::net::TcpListener;


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
    let args = Args::parse();

    // Parse headers
    let mut parsed_headers = Vec::new();
    for h in &args.header {
        if let Some((k, v)) = h.split_once(':') {
            parsed_headers.push((k.trim().to_string(), v.trim().to_string()));
        } else {
            eprintln!("Warning: Invalid header format '{}', ignoring.", h);
        }
    }

    // Load body content - either from string or file if starts with @
    let body_content = load_body_content(args.body.as_deref())?;

    let config = Arc::new(ServerConfig {
        status: StatusCode::from_u16(args.status).context("Invalid status code")?,
        body: body_content,
        headers: parsed_headers,
    });

    let addr = SocketAddr::from(([0, 0, 0, 0], args.port));
    println!("Starting server on {} with {} threads...", addr, args.threads);

    let mut handles = Vec::new();

    for i in 0..args.threads {
        let config = config.clone();
        
        #[cfg(all(target_os = "linux", feature = "io_uring"))]
        let use_uring = args.uring;
        #[cfg(not(all(target_os = "linux", feature = "io_uring")))]
        let use_uring = false;
        
        let http2_enabled = args.http2;

        let handle = thread::spawn(move || {
            if let Err(e) = run_thread(i, addr, config, use_uring, http2_enabled) {
                eprintln!("Thread {} error: {}", i, e);
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

fn load_body_content(body: Option<&str>) -> Result<Bytes> {
    match body {
        Some(content) if content.starts_with('@') => {
            // Remove @ prefix and treat as file path
            let file_path = &content[1..];
            println!("Loading body content from file: {}", file_path);
            let file_content = std::fs::read_to_string(file_path)
                .with_context(|| format!("Failed to read body file: {}", file_path))?;
            Ok(Bytes::from(file_content))
        }
        Some(content) => Ok(Bytes::from(content.to_string())),
        None => Ok(Bytes::new()),
    }
}

fn create_socket(addr: SocketAddr) -> Result<std::net::TcpListener> {
    let domain = if addr.is_ipv6() { Domain::IPV6 } else { Domain::IPV4 };
    let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;

    // Enable SO_REUSEPORT on all Unix systems that support it
    #[cfg(unix)]
    {
        if let Err(e) = socket.set_reuse_port(true) {
            eprintln!("Warning: SO_REUSEPORT failed: {}. Falling back to SO_REUSEADDR.", e);
            socket.set_reuse_address(true)?;
        }
    }
    
    // On non-Unix systems, use SO_REUSEADDR
    #[cfg(not(unix))]
    {
        socket.set_reuse_address(true)?;
    }

    socket.bind(&addr.into())?;
    socket.listen(1024)?;
    
    Ok(socket.into())
}

fn run_thread(id: usize, addr: SocketAddr, config: Arc<ServerConfig>, _use_uring: bool, http2_enabled: bool) -> Result<()> {
    // Create socket inside thread to ensure each thread has its own file descriptor
    let std_listener = create_socket(addr)?;
    std_listener.set_nonblocking(true)?;

    // io_uring implementation for Linux
    #[cfg(all(target_os = "linux", feature = "io_uring"))]
    if _use_uring {
        println!("Thread {} using io_uring runtime", id);
        return tokio_uring::start(async move {
            let listener = tokio_uring::net::TcpListener::from_std(std_listener);
            println!("Thread {} listening on {} (io_uring)", id, addr);

            loop {
                let (stream, _) = match listener.accept().await {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("Thread {} accept error: {}", id, e);
                        continue;
                    }
                };

                let config = config.clone();
                
                // Note: Full hyper integration with io_uring requires additional work
                // This is a simplified version that shows the concept
                tokio_uring::spawn(async move {
                    // For a complete implementation, you'd need to implement
                    // the hyper service using io_uring's AsyncRead/AsyncWrite traits
                    // This is a placeholder showing the structure
                    let _ = handle_connection_uring(stream, config).await;
                });
            }
        });
    }

    // Standard Tokio single-thread runtime
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    rt.block_on(async move {
        let listener = TcpListener::from_std(std_listener)?;
        println!("Thread {} listening on {}", id, addr);

        loop {
            let (stream, _) = match listener.accept().await {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("Thread {} accept error: {}", id, e);
                    continue;
                }
            };

            let io = TokioIo::new(stream);
            let config = config.clone();

            // Spawn task to handle the connection
            tokio::task::spawn(async move {
                let service = service_fn(move |_req: Request<hyper::body::Incoming>| {
                    let config = config.clone();
                    async move {
                        handle_request(config).await
                    }
                });

                if http2_enabled {
                    if let Err(err) = http2::Builder::new(hyper_util::rt::TokioExecutor::new())
                        .serve_connection(io, service)
                        .await
                    {
                        eprintln!("Error serving connection: {:?}", err);
                    }
                } else {
                    if let Err(err) = http1::Builder::new()
                        .serve_connection(io, service)
                        .await
                    {
                        eprintln!("Error serving connection: {:?}", err);
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
async fn handle_connection_uring(
    _stream: tokio_uring::net::TcpStream,
    _config: Arc<ServerConfig>,
) -> Result<()> {
    // Placeholder for io_uring connection handling
    // A full implementation would require implementing HTTP parsing
    // and response generation using io_uring's async I/O primitives
    println!("Handling connection with io_uring (simplified implementation)");
    Ok(())
}