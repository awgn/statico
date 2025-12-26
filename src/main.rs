mod hyper_srv;
#[cfg(all(target_os = "linux", feature = "io_uring"))]
mod uring;

use anyhow::{Context, Result};
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
        crate::uring::run_thread(id, addr, config, http2_enabled)
    } else {
        crate::hyper_srv::run_thread(id, addr, config, http2_enabled)
    }
    #[cfg(not(all(target_os = "linux", feature = "io_uring")))]
    crate::hyper_srv::run_thread(id, addr, config, http2_enabled)
}
