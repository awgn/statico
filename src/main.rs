mod http;
mod hyper_srv;
mod pretty;
#[cfg(all(target_os = "linux", feature = "io_uring"))]
mod uring;

use crate::hyper_srv::load_body_content;
use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use clap::Parser;
use contatori::counters::monotone::Monotone;
use contatori::counters::{CounterValue, Observable};
use humantime::parse_duration;
use hyper::StatusCode;
use std::net::SocketAddr;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tracing::{error, info, warn};

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
    pub io_uring: bool,

    /// Size of the io_uring Submission Queue (SQ)
    #[cfg(all(target_os = "linux", feature = "io_uring"))]
    #[arg(long, default_value_t = 4096)]
    pub uring_entries: u32,

    /// Enable kernel-side submission polling with idle timeout in milliseconds.
    #[cfg(all(target_os = "linux", feature = "io_uring"))]
    #[arg(long)]
    pub uring_sqpoll: Option<u32>,

    /// Enable meter
    #[arg(long)]
    pub meter: bool,

    /// Increase verbosity level (can be repeated: -v, -vv, -vvv)
    #[arg(short, long, action = clap::ArgAction::Count, default_value_t = 0)]
    pub verbose: u8,

    /// Delay before sending the response (e.g., 100ms, 1s, 500us)
    #[arg(short, long, value_parser = parse_duration)]
    pub delay: Option<std::time::Duration>,
}

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
    let use_uring = args.io_uring;
    #[cfg(not(all(target_os = "linux", feature = "io_uring")))]
    let use_uring = false;

    if use_uring && args.http2 {
        return Err(anyhow!("HTTP/2 is not currently supported with io_uring"));
    }

    let args = Arc::new(args);

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
            if let Err(e) = run_thread(id, addr, config, &args, use_uring) {
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

#[inline]
fn read_counters() -> (CounterValue, CounterValue, CounterValue, CounterValue) {
    (
        REQUESTS.value(),
        REQUEST_BYTES.value(),
        RESPONSES.value(),
        RESPONSE_BYTES.value(),
    )
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
