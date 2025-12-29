mod hyper_srv;
#[cfg(all(target_os = "linux", feature = "io_uring"))]
mod uring;

use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use clap::Parser;
use hyper::StatusCode;
use std::net::SocketAddr;
use std::sync::Arc;
use std::thread;
use tracing::{error, info, warn};

use crate::hyper_srv::load_body_content;

#[derive(Parser, Clone, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    /// Number of threads to spawn
    #[arg(short, long, default_value_t = num_cpus::get())]
    pub threads: usize,

    /// Port to listen on
    #[arg(short, long, default_value_t = 8080)]
    pub port: u16,

    /// Address to listen on. If not specified, listen on all interfaces.
    #[arg(short, long)]
    pub address: Option<String>,

    /// HTTP Status code to return
    #[arg(short, long, default_value_t = 200)]
    pub status: u16,

    /// Response body (optional)
    #[arg(short, long)]
    pub body: Option<String>,

    /// Headers in "Name: Value" format
    #[arg(long)]
    pub header: Vec<String>,

    /// Enable HTTP/2 (h2c) support
    #[arg(long)]
    pub http2: bool,

    /// Receive buffer size
    #[arg(long)]
    pub receive_buffer_size: Option<usize>,

    /// Send buffer size
    #[arg(long)]
    pub send_buffer_size: Option<usize>,

    /// Listen backlog queue
    #[arg(long)]
    pub listen_backlog: Option<i32>,

    /// Set TCP_NODELAY option
    #[arg(long)]
    pub tcp_nodelay: bool,

    /// Use io_uring (Linux only)
    #[cfg(all(target_os = "linux", feature = "io_uring"))]
    #[arg(long)]
    pub uring: bool,

    /// Size of the io_uring Submission Queue (SQ)
    #[cfg(all(target_os = "linux", feature = "io_uring"))]
    #[arg(long, default_value_t = 4096)]
    pub uring_entries: u32,

    /// Enable kernel-side submission polling with idle timeout in milliseconds.
    #[cfg(all(target_os = "linux", feature = "io_uring"))]
    #[arg(long)]
    pub uring_sqpoll: Option<u32>,
}

/// Configuration shared across threads
#[derive(Clone)]
pub struct ServerConfig {
    pub status: StatusCode,
    pub body: Bytes,
    pub headers: Vec<(String, String)>,
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
        headers: parsed_headers,
    });

    // Build SocketAddr from address option
    let addr: SocketAddr = match &args.address {
        Some(address) => {
            let addr_with_port = format!("{}:{}", address, args.port);
            addr_with_port
                .parse()
                .with_context(|| format!("Invalid address: {}", addr_with_port))?
        }
        None => SocketAddr::from(([0, 0, 0, 0], args.port)),
    };

    info!("Starting server on {} with {} threads", addr, args.threads);

    #[cfg(all(target_os = "linux", feature = "io_uring"))]
    let use_uring = args.uring;
    #[cfg(not(all(target_os = "linux", feature = "io_uring")))]
    let use_uring = false;

    if use_uring && args.http2 {
        return Err(anyhow!("HTTP/2 is not currenlty supported with io_uring"));
    }

    let args = Arc::new(args);
    let mut handles = Vec::new();

    for id in 0..args.threads {
        let config = config.clone();
        let args = args.clone();

        let handle = thread::spawn(move || {
            if let Err(e) = run_thread(id, addr, config, &args, use_uring) {
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
    args: &Args,
    _use_uring: bool,
) -> Result<()> {
    // Hyper implementation for Linux
    #[cfg(all(target_os = "linux", feature = "io_uring"))]
    if _use_uring {
        crate::uring::run_thread(id, addr, config, args)
    } else {
        crate::hyper_srv::run_thread(id, addr, config, args)
    }
    #[cfg(not(all(target_os = "linux", feature = "io_uring")))]
    crate::hyper_srv::run_thread(id, addr, config, args)
}
