mod delayed_body;
mod http;
mod options;
mod pretty;
mod tokio;
#[cfg(all(target_os = "linux", feature = "tokio_uring"))]
mod tokio_uring;

#[cfg(all(target_os = "linux", feature = "monoio"))]
mod monoio;

#[cfg(all(target_os = "linux", feature = "glommio"))]
mod glommio;

use crate::options::Options;
use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use clap::Parser;
use contatori::counters::monotone::Monotone;
use contatori::counters::{CounterValue, Observable};
use hyper::StatusCode;
use std::net::SocketAddr;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tracing::{error, info, warn};
use pingora_timeout::fast_timeout::fast_sleep;
use socket2::{Domain, Protocol, Socket, Type};

/// Configuration shared across threads
#[derive(Clone)]
pub struct ServerConfig {
    pub status: StatusCode,
    pub body: Bytes,
    pub headers: Vec<(String, String)>,
}

pub static REQUESTS: Monotone = Monotone::new();
pub static REQUEST_BYTES: Monotone = Monotone::new();
pub static RESPONSES: Monotone = Monotone::new();
pub static RESPONSE_BYTES: Monotone = Monotone::new();

fn main() -> Result<()> {
    // Initialize tracing subscriber
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let opts = Options::parse();

    // Parse headers
    let mut parsed_headers = Vec::new();
    for h in &opts.header {
        if let Some((k, v)) = h.split_once(':') {
            parsed_headers.push((k.trim().to_string(), v.trim().to_string()));
        } else {
            warn!("Invalid header format '{}', ignoring", h);
        }
    }

    // Load body content - either from string or file if starts with @
    let body_content = load_body_content(opts.body.as_deref())?;

    let status_code = StatusCode::from_u16(opts.status).context("Invalid status code")?;

    let config = Arc::new(ServerConfig {
        status: status_code,
        body: body_content.clone(),
        headers: parsed_headers,
    });

    // Build SocketAddr from address option
    let addr: SocketAddr = match &opts.address {
        Some(address) => {
            let addr_with_port = format!("{}:{}", address, opts.port);
            addr_with_port
                .parse()
                .with_context(|| format!("Invalid address: {}", addr_with_port))?
        }
        None => SocketAddr::from(([0, 0, 0, 0], opts.port)),
    };

    info!("Starting server on {} with {} threads", addr, opts.threads);

    #[cfg(all(target_os = "linux", feature = "tokio_uring"))]
    if matches!(opts.runtime, crate::options::Runtime::TokioUring) && opts.http2 {
        return Err(anyhow!(
            "HTTP/2 is not currently supported with tokio-uring"
        ));
    }
    #[cfg(all(target_os = "linux", feature = "monoio"))]
    if matches!(opts.runtime, crate::options::Runtime::Monoio) && opts.http2 {
        return Err(anyhow!("HTTP/2 is not currently supported with monoio"));
    }
    #[cfg(all(target_os = "linux", feature = "glommio"))]
    if matches!(opts.runtime, crate::options::Runtime::Glommio) && opts.http2 {
        return Err(anyhow!("HTTP/2 is not currently supported with glommio"));
    }

    let args = Arc::new(opts);

    let meter_enabled = args.meter;

    // Set up ctrlc handler to print final report
    ctrlc::set_handler(move || {
        if meter_enabled {
            print_final_report();
        }
        std::process::exit(0);
    })
    .expect("Error setting Ctrl-C handler");

    let mut handles = Vec::new();

    for id in 0..args.threads {
        let config = config.clone();
        let args = args.clone();

        let handle = thread::spawn(move || {
            if let Err(e) = run_thread(id, addr, config, &args) {
                error!("Thread {} error: {}", id, e);
            }
        });
        handles.push(handle);
    }

    if args.meter {
        let handle = thread::spawn(move || {
            let (mut prev_req, mut prev_req_bytes, mut prev_res, mut prev_res_bytes) =
                read_counters();
            loop {
                thread::sleep(Duration::from_secs(1));
                let (req, req_bytes, res, res_bytes) = read_counters();

                let req_per_sec = req - prev_req;
                let req_bytes_per_sec = req_bytes - prev_req_bytes;
                let res_per_sec = res - prev_res;
                let res_bytes_per_sec = res_bytes - prev_res_bytes;

                // Convert bytes/sec to Gbps (bytes * 8 / 1_000_000_000)
                let req_gbps = (req_bytes_per_sec.as_f64() * 8.0) / 1_000_000_000.0;
                let res_gbps = (res_bytes_per_sec.as_f64() * 8.0) / 1_000_000_000.0;

                println!(
                    "req/s: {}, req: {:.3} Gbps, res/s: {}, res: {:.3} Gbps",
                    req_per_sec, req_gbps, res_per_sec, res_gbps
                );
                prev_req = req;
                prev_req_bytes = req_bytes;
                prev_res = res;
                prev_res_bytes = res_bytes;
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

#[cold]
async fn execute_delay(delay: std::time::Duration) {
    fast_sleep(delay).await;
}

#[inline]
fn read_counters() -> (CounterValue, CounterValue, CounterValue, CounterValue) {
    (
        REQUESTS.value(),
        REQUEST_BYTES.value(),
        RESPONSES.value(),
        RESPONSE_BYTES.value(),
    )
}

pub fn create_listener(addr: SocketAddr, opts: &Options) -> Result<std::net::TcpListener> {
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
            use tracing::warn;

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
    if opts.tcp_nodelay {
        socket.set_tcp_nodelay(true)?;
    }

    // Apply receive buffer size if specified
    if let Some(size) = opts.receive_buffer_size {
        socket.set_recv_buffer_size(size)?;
    }

    // Apply send buffer size if specified
    if let Some(size) = opts.send_buffer_size {
        socket.set_send_buffer_size(size)?;
    }

    socket.bind(&addr.into())?;
    socket.listen(opts.listen_backlog.unwrap_or(1024))?;

    // Set nonblocking mode
    socket.set_nonblocking(true)?;

    Ok(socket.into())
}

fn print_final_report() {
    let (req, req_bytes, res, res_bytes) = read_counters();

    // Convert bytes to human-readable format
    let req_bytes_val = req_bytes.as_u64();
    let res_bytes_val = res_bytes.as_u64();

    println!("Total requests:  {}", req);
    println!(
        "Total req bytes: {} ({:.3} GB)",
        req_bytes,
        req_bytes_val as f64 / 1_000_000_000.0
    );
    println!("Total responses: {}", res);
    println!(
        "Total res bytes: {} ({:.3} GB)",
        res_bytes,
        res_bytes_val as f64 / 1_000_000_000.0
    );
}

fn run_thread(
    id: usize,
    addr: SocketAddr,
    config: Arc<ServerConfig>,
    opts: &Options,
) -> Result<()> {
    use crate::options::Runtime;
    match opts.runtime {
        Runtime::Tokio => crate::tokio::run_thread(id, addr, config, opts),
        Runtime::TokioLocal => crate::tokio::run_thread_local(id, addr, config, opts),
        #[cfg(all(target_os = "linux", feature = "tokio_uring"))]
        Runtime::TokioUring => crate::tokio_uring::run_thread(id, addr, config, opts),
        #[cfg(all(target_os = "linux", feature = "monoio"))]
        Runtime::Monoio => crate::monoio::run_thread(id, addr, config, opts),
        #[cfg(all(target_os = "linux", feature = "glommio"))]
        Runtime::Glommio => crate::glommio::run_thread(id, addr, config, opts),
    }
}
