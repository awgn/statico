use clap::{Parser, ValueEnum};
use humantime::parse_duration;
use number_range::NumberRangeOptions;
use std::fmt::Display;
use std::ops::Deref;
use std::str::FromStr;

#[derive(Clone, Debug)]
pub struct Ports(pub Vec<u16>);

impl FromStr for Ports {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let ports: Vec<u16> = NumberRangeOptions::<u16>::new()
            .with_list_sep(',')
            .with_range_sep('-')
            .parse(s)
            .map_err(|e| format!("Invalid port range: {}", e))?
            .collect();

        if ports.is_empty() {
            return Err("No ports specified".to_string());
        }

        Ok(Ports(ports))
    }
}

impl Default for Ports {
    fn default() -> Self {
        Ports(vec![8080])
    }
}

impl Display for Ports {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let ports_str: Vec<String> = self.0.iter().map(|p| p.to_string()).collect();
        write!(f, "{}", ports_str.join(","))
    }
}

impl Deref for Ports {
    type Target = Vec<u16>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum, Debug)]
pub enum Runtime {
    Tokio,
    TokioLocal,
    #[cfg(all(target_os = "linux", feature = "tokio_uring"))]
    TokioUring,
    #[cfg(all(target_os = "linux", feature = "monoio"))]
    Monoio,
    #[cfg(all(target_os = "linux", feature = "glommio"))]
    Glommio,
    #[cfg(feature = "smol")]
    Smol,
    #[cfg(feature = "compio")]
    Compio,
}

impl FromStr for Runtime {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "tokio" => Ok(Runtime::Tokio),
            "tokio-local" => Ok(Runtime::TokioLocal),
            #[cfg(all(target_os = "linux", feature = "tokio_uring"))]
            "tokio-uring" => Ok(Runtime::TokioUring),
            #[cfg(all(target_os = "linux", feature = "monoio"))]
            "monoio" => Ok(Runtime::Monoio),
            #[cfg(all(target_os = "linux", feature = "glommio"))]
            "glommio" => Ok(Runtime::Glommio),
            #[cfg(feature = "smol")]
            "smol" => Ok(Runtime::Smol),
            #[cfg(feature = "compio")]
            "compio" => Ok(Runtime::Compio),
            _ => Err(format!("Unknown runtime: {}", s)),
        }
    }
}

#[derive(Parser, Clone, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Options {
    /// Number of threads to spawn
    #[arg(short, long, default_value_t = num_cpus::get())]
    pub threads: usize,

    /// Ports to listen on (e.g 8080,8100-8200)
    #[arg(short = 'p', long, default_value_t = Ports::default())]
    pub ports: Ports,

    /// Threads bind to all ports
    #[arg(long)]
    pub bind_all: bool,

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
    #[arg(short = 'H', long)]
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

    /// Size of the io_uring Submission Queue (SQ)
    #[cfg(all(target_os = "linux"))]
    #[arg(long, default_value_t = 4096)]
    pub uring_entries: u32,

    /// Enable kernel-side submission polling with idle timeout in milliseconds.
    #[cfg(all(target_os = "linux"))]
    #[arg(long)]
    pub uring_sqpoll: Option<u32>,

    /// Enable meter
    #[arg(short = 'm', long)]
    pub meter: bool,

    /// Increase verbosity level (can be repeated: -v, -vv, -vvv, -vvvv)
    #[arg(short, long, action = clap::ArgAction::Count, default_value_t = 0)]
    pub verbose: u8,

    /// Delay before sending the response (e.g., 100ms, 1s, 500us)
    #[arg(short, long, value_parser = parse_duration)]
    pub delay: Option<std::time::Duration>,

    /// Delay before sending the body of the response
    #[arg(long, value_parser = parse_duration)]
    pub body_delay: Option<std::time::Duration>,

    /// Runtime to use
    #[arg(long, default_value = "tokio")]
    pub runtime: Runtime,
}
