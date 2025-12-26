use anyhow::Result;
use http_body_util::{Empty, Full};
use hyper::body::Bytes;

use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use std::convert::Infallible;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::task::{Context, Poll};
use tokio::io::{duplex, AsyncRead, AsyncWrite};
use tracing::error;

use crate::hyper_srv::create_listener;
use crate::ServerConfig;

pub fn run_thread(
    id: usize,
    addr: SocketAddr,
    config: Arc<ServerConfig>,
    http2_enabled: bool,
) -> Result<()> {
    // io_uring implementation for Linux

    use tracing::info;
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
    if http2 {
        use tracing::warn;
        warn!("HTTP/2 is not supported with io_uring raw TCP");
        return Err(anyhow::anyhow!("HTTP/2 not supported with io_uring"));
    }

    // Robust HTTP/1.1 implementation with proper request parsing
    let mut connection_buffer = Vec::new();
    let mut read_buf = vec![0u8; 4096];
    let mut requests_served = 0;

    // Generate response ONCE outside the loop for better performance
    let resp = handle_request(config.clone()).await?;
    let mut response = to_bytes(resp, false).await;

    loop {
        // Read data from stream
        let (result, buf) = stream.read(read_buf).await;
        read_buf = buf;

        let bytes_read = match result {
            Ok(0) => break, // Connection closed normally by client
            Ok(n) => n,
            Err(e) => return Err(e.into()), // Read error
        };

        if bytes_read > 0 {
            // Append new data to connection buffer
            connection_buffer.extend_from_slice(&read_buf[..bytes_read]);

            // Process complete HTTP requests from buffer
            while let Some(request_end) = find_complete_request(&connection_buffer) {
                requests_served += 1;

                // Just consume the request body and send the pre-generated response
                // Write response
                let (result, r) = stream.write_all(response).await;
                response = r; // Get buffer back for next iteration

                if let Err(e) = result {
                    return Err(e.into());
                }

                // Remove processed request from buffer
                connection_buffer.drain(..request_end);
            }
        }
    }

    Ok(requests_served)
}

/// Find the end of a complete HTTP/1.1 request in the buffer using a single-pass optimized algorithm.
#[inline]
pub(crate) fn find_complete_request(buf: &[u8]) -> Option<usize> {
    let mut pos = 0;
    let mut content_length = 0;

    // Iterate looking for the '\n' byte (New Line).
    // The Rust compiler often optimizes this iterator using vectorization (SIMD).
    while let Some(offset) = buf[pos..].iter().position(|&b| b == b'\n') {
        let end_of_line = pos + offset;

        // Extract the line excluding the trailing '\n'
        let mut line = &buf[pos..end_of_line];

        // Handle optional Carriage Return (\r\n)
        if let Some(&b'\r') = line.last() {
            line = &line[..line.len() - 1];
        }

        // If the line is empty, we found the end of headers (\r\n\r\n)
        if line.is_empty() {
            let body_start = end_of_line + 1; // Skip the last \n
            let request_end = body_start + content_length;

            if buf.len() >= request_end {
                return Some(request_end);
            } else {
                return None; // Incomplete buffer
            }
        }

        // "Fail-Fast" optimization:
        // 1. The line must be at least as long as "content-length:0" (approx 16 bytes)
        // 2. The first character must be 'c' or 'C' (0x20 is the lowercase mask)
        if line.len() >= 15 && (line[0] | 0x20) == b'c' {
            // Fast case-insensitive check using Rust's native SIMD
            if line[..15].eq_ignore_ascii_case(b"content-length:") {
                // Manual integer parsing (avoids allocations and UTF-8 checks)
                // Skip the first 15 characters ("content-length:")
                let mut value_iter = line[15..].iter();

                // Skip whitespace
                let mut val_start = value_iter.clone(); // Lightweight clone to "peek"
                while let Some(&b) = val_start.next() {
                    if b == b' ' || b == b'\t' {
                        value_iter.next();
                    } else {
                        break;
                    }
                }

                // Parse digits
                let mut val = 0;
                for &b in value_iter {
                    if b.is_ascii_digit() {
                        val = val * 10 + (b - b'0') as usize;
                    } else {
                        break;
                    }
                }
                content_length = val;
            }
        }

        // Advance to the next line
        pos = end_of_line + 1;
    }

    None
}

async fn handle_request(config: Arc<ServerConfig>) -> Result<Response<Full<Bytes>>> {
    let mut builder = Response::builder().status(config.status);

    // Add configured headers
    for (k, v) in &config.headers {
        builder = builder.header(k, v);
    }

    Ok(builder.body(Full::new(config.body.clone()))?)
}

/// Socket wrapper that captures written bytes while simulating a real connection
struct CaptureWrapper {
    inner: tokio::io::DuplexStream,
    captured: Arc<Mutex<Vec<u8>>>,
}

impl CaptureWrapper {
    fn new(inner: tokio::io::DuplexStream) -> Self {
        Self {
            inner,
            captured: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl AsyncRead for CaptureWrapper {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for CaptureWrapper {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        // Capture the bytes being written
        self.captured.lock().unwrap().extend_from_slice(buf);
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), io::Error>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

pub async fn to_bytes(response: Response<Full<Bytes>>, http2: bool) -> Vec<u8> {
    let (client, server) = duplex(8192);
    let capture_server = CaptureWrapper::new(server);
    let captured_ref = capture_server.captured.clone();

    let handle = tokio::spawn(async move {
        let service = service_fn(move |_req: Request<hyper::body::Incoming>| {
            let res = response.clone();
            async move { Ok::<_, Infallible>(res) }
        });

        if http2 {
            let _ = hyper::server::conn::http2::Builder::new(hyper_util::rt::TokioExecutor::new())
                .serve_connection(TokioIo::new(capture_server), service)
                .await;
        } else {
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(capture_server), service)
                .await;
        }
    });

    let req = hyper::Request::builder()
        .method("GET")
        .uri("/")
        .header("host", "localhost")
        .body(Empty::<Bytes>::new())
        .unwrap();

    // Send a request to trigger the response
    if http2 {
        tokio::spawn(async move {
            let client_connection =
                hyper::client::conn::http2::Builder::new(hyper_util::rt::TokioExecutor::new())
                    .handshake(TokioIo::new(client))
                    .await;

            if let Ok((mut sender, connection)) = client_connection {
                tokio::spawn(connection);

                let _ = sender.send_request(req).await;
            }
        });
    } else {
        tokio::spawn(async move {
            let client_connection = hyper::client::conn::http1::Builder::new()
                .handshake(TokioIo::new(client))
                .await;

            if let Ok((mut sender, connection)) = client_connection {
                tokio::spawn(connection);

                let _ = sender.send_request(req).await;
            }
        });
    }

    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    let _ = handle.await;
    let result = captured_ref.lock().unwrap().clone();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_http1_capture() {
        let response = Response::builder()
            .status(200)
            .header("Content-Type", "text/plain")
            .body(Full::new(Bytes::from("Hello World")))
            .unwrap();

        let bytes = to_bytes(response, false).await;
        let output = String::from_utf8_lossy(&bytes);

        println!("HTTP/1.1 Response:\n{}", output);
        assert!(output.contains("HTTP/1.1 200 OK"));
        assert!(output.contains("Hello World"));
    }

    #[tokio::test]
    async fn test_http2_capture() {
        let response = Response::builder()
            .status(200)
            .header("Content-Type", "text/plain")
            .body(Full::new(Bytes::from("Hello World")))
            .unwrap();

        let bytes = to_bytes(response, true).await;

        println!("HTTP/2 Response: {} bytes", bytes.len());
        // HTTP/2 è binario, quindi verifichiamo solo che ci siano dei dati
        assert!(bytes.len() > 0);
        // I primi bytes dovrebbero contenere il connection preface response
        assert!(bytes.len() > 9); // Almeno SETTINGS frame
    }

    #[test]
    fn test_find_complete_request_no_body() {
        // GET request without body
        let request = b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n";
        assert_eq!(find_complete_request(request), Some(request.len()));
    }

    #[test]
    fn test_find_complete_request_with_body() {
        // POST request with Content-Length
        let request = b"POST /api HTTP/1.1\r\nHost: localhost\r\nContent-Length: 5\r\n\r\nhello";
        assert_eq!(find_complete_request(request), Some(request.len()));
    }

    #[test]
    fn test_find_complete_request_incomplete_headers() {
        // Missing final \r\n\r\n
        let request = b"GET / HTTP/1.1\r\nHost: localhost\r\n";
        assert_eq!(find_complete_request(request), None);
    }

    #[test]
    fn test_find_complete_request_incomplete_body() {
        // Headers complete but body incomplete
        let request = b"POST /api HTTP/1.1\r\nHost: localhost\r\nContent-Length: 10\r\n\r\nhello";
        assert_eq!(find_complete_request(request), None); // Only 5 bytes, needs 10
    }

    #[test]
    fn test_find_complete_request_multiple_requests() {
        // Two complete requests in buffer
        let buffer = b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\nGET /api HTTP/1.1\r\nHost: localhost\r\n\r\n";
        let first_request_end = "GET / HTTP/1.1\r\nHost: localhost\r\n\r\n".len();
        assert_eq!(find_complete_request(buffer), Some(first_request_end));
    }

    #[test]
    fn test_real_world_scenario() {
        // Simulate a real POST request with JSON body
        let body = b"{\"name\":\"John\",\"age\":30}";
        let content_length = body.len();
        let request = format!(
            "POST /api/users HTTP/1.1\r\nHost: api.example.com\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            content_length,
            std::str::from_utf8(body).unwrap()
        );
        let request_bytes = request.as_bytes();
        assert_eq!(
            find_complete_request(request_bytes),
            Some(request_bytes.len())
        );

        // Test partial request (missing some body bytes)
        let partial = format!(
            "POST /api/users HTTP/1.1\r\nHost: api.example.com\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{{\"name\":\"John\"",
            content_length
        );
        let partial_bytes = partial.as_bytes();
        assert_eq!(find_complete_request(partial_bytes), None);
    }
}
