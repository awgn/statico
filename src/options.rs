use clap::Parser;
use humantime::parse_duration;

#[derive(Parser, Clone, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Options {
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
