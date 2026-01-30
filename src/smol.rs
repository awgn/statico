use crate::delayed_body::DelayedBody;
use crate::execute_delay;
use crate::http::{chunked_body_wire_size, request_head_size, response_head_size};
use crate::options::Options;
use crate::pretty::PrettyPrint;
use crate::ServerConfig;
use crate::REQUESTS;
use crate::REQUEST_BYTES;
use crate::RESPONSES;
use crate::RESPONSE_BYTES;

use anyhow::Result;
use http_body_util::{BodyExt, Either, Full};
use hyper::body::Bytes;
use hyper::header::{CONTENT_LENGTH, TRANSFER_ENCODING};
use hyper::rt::{Read, ReadBufCursor, Write};
use hyper::server::conn::{http1, http2};
use hyper::service::service_fn;
use hyper::{Request, Response};
use owo_colors::OwoColorize;
use smol::io::{AsyncRead, AsyncWrite};
use std::mem::MaybeUninit;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tracing::{error, info};

pub fn run_thread(
    id: usize,
    addr: SocketAddr,
    config: Arc<ServerConfig>,
    opts: &Options,
) -> Result<()> {
    info!("Thread {} listening on {} (smol-hyper)", id, addr);

    let std_listener = crate::create_listener(addr, opts)?;
    let listener = smol::net::TcpListener::from(smol::Async::new(std_listener)?);
    let ex = std::rc::Rc::new(smol::LocalExecutor::new());
    let spawn_ex = ex.clone();

    smol::block_on(ex.run(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(s) => s,
                Err(e) => {
                    error!("Thread {} accept error: {}", id, e);
                    continue;
                }
            };

            if opts.tcp_nodelay {
                if let Err(e) = stream.set_nodelay(true) {
                    error!("Failed to set TCP_NODELAY: {}", e);
                }
            }

            let io = SmolIo::new(stream);
            let config = config.clone();
            let use_http2 = opts.http2;
            let verbose = opts.verbose;
            let delay = opts.delay;
            let body_delay = opts.body_delay;
            let meter = opts.meter;

            spawn_ex
                .spawn(async move {
                    let service = service_fn(move |req: Request<hyper::body::Incoming>| {
                        let config = config.clone();
                        async move {
                            let (head_size, is_chunked) = if meter {
                                let hs = request_head_size(&req);
                                let chunked = req
                                    .headers()
                                    .get(TRANSFER_ENCODING)
                                    .and_then(|v| v.to_str().ok())
                                    .map(|s| s.contains("chunked"))
                                    .unwrap_or(false);
                                (hs, chunked)
                            } else {
                                (0, false)
                            };

                            // Collect request body if metering or verbose mode is enabled
                            let collected_req = if meter || verbose > 0 {
                                collect_request(req).await.ok()
                            } else {
                                None
                            };

                            if meter {
                                REQUESTS.add(1);
                                if let Some(ref req) = collected_req {
                                    let body_bytes = req.body().len();
                                    // For chunked encoding, calculate the wire format overhead
                                    let body_size = if is_chunked {
                                        chunked_body_wire_size(body_bytes)
                                    } else {
                                        body_bytes
                                    };
                                    REQUEST_BYTES.add(head_size + body_size);
                                } else {
                                    REQUEST_BYTES.add(head_size);
                                }
                            }

                            if verbose > 0 {
                                if let Some(ref req) = collected_req {
                                    println!("↩ {}:\n{}", "request".bold(), req.pretty(verbose));
                                }
                            }

                            let mut builder = Response::builder().status(config.status);

                            // Add configured headers
                            for (k, v) in &config.headers {
                                builder = builder.header(k, v);
                            }

                            // Always add Content-Length
                            if !config.body.is_empty() {
                                builder = builder.header(CONTENT_LENGTH, config.body.len());
                            }

                            if let Some(delay) = delay {
                                execute_delay(delay).await;
                            }

                            let body = match body_delay {
                                Some(delay) => Either::Left(DelayedBody::new(
                                    Full::new(config.body.clone()),
                                    delay,
                                )),
                                None => Either::Right(Full::new(config.body.clone())),
                            };

                            let resp = builder.body(body);

                            if let Ok(ref resp) = resp {
                                if meter {
                                    RESPONSES.add(1);
                                    let head_size = response_head_size(resp, config.body.len());
                                    RESPONSE_BYTES.add(head_size + config.body.len());
                                }
                                if verbose > 0 {
                                    // Create a response with Bytes body for printing
                                    let mut print_builder =
                                        Response::builder().status(resp.status());
                                    for (k, v) in resp.headers() {
                                        print_builder = print_builder.header(k, v);
                                    }
                                    let print_resp =
                                        print_builder.body(config.body.clone()).unwrap();
                                    println!(
                                        "↪ {}:\n{}",
                                        "response".bold(),
                                        print_resp.pretty(verbose)
                                    );
                                }
                            }

                            resp
                        }
                    });

                    let result = if use_http2 {
                        http2::Builder::new(GlobalSmolSpawn)
                            .serve_connection(io, service)
                            .await
                    } else {
                        http1::Builder::new().serve_connection(io, service).await
                    };

                    if let Err(err) = result {
                        let protocol = if use_http2 { "HTTP/2" } else { "HTTP/1.1" };
                        error!("Error serving {} connection: {:?}", protocol, err);
                    }
                })
                .detach();
        }
    }))
}

#[derive(Clone, Copy, Debug)]
struct GlobalSmolSpawn;

impl<F> hyper::rt::Executor<F> for GlobalSmolSpawn
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    fn execute(&self, fut: F) {
        smol::spawn(fut).detach();
    }
}

#[inline]
pub async fn collect_request<B>(req: Request<B>) -> Result<Request<Bytes>, B::Error>
where
    B: http_body::Body,
{
    let (parts, body) = req.into_parts();
    let collected = body.collect().await?;
    let bytes = collected.to_bytes();
    Ok(Request::from_parts(parts, bytes))
}

struct SmolIo<T>(T);

impl<T> SmolIo<T> {
    fn new(inner: T) -> Self {
        Self(inner)
    }
}

impl<T: AsyncRead + Unpin> Read for SmolIo<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut buf: ReadBufCursor<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        let slice = unsafe {
            let b = buf.as_mut();
            &mut *(b as *mut [MaybeUninit<u8>] as *mut [u8])
        };

        match Pin::new(&mut self.0).poll_read(cx, slice) {
            Poll::Ready(Ok(n)) => {
                unsafe { buf.advance(n) };
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<T: AsyncWrite + Unpin> Write for SmolIo<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.0).poll_close(cx)
    }
}
