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
}
