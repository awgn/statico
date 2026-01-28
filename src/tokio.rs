use anyhow::Result;
use http_body_util::BodyExt;
use http_body_util::Either;
use http_body_util::Full;
use hyper::body::Bytes;
use hyper::header::{CONTENT_LENGTH, TRANSFER_ENCODING};
use hyper::server::conn::http1;
use hyper::server::conn::http2;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use owo_colors::OwoColorize;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::task::LocalSet;
use tracing::{error, info};

use crate::delayed_body::DelayedBody;
use crate::execute_delay;
use crate::http::chunked_body_wire_size;
use crate::http::{request_head_size, response_head_size};
use crate::REQUESTS;
use crate::REQUEST_BYTES;
use crate::RESPONSES;
use crate::RESPONSE_BYTES;

use crate::create_listener;
use crate::options::Options;
use crate::pretty::PrettyPrint;
use crate::ServerConfig;

async fn tokio_srv(
    id: usize,
    addr: SocketAddr,
    std_listener: std::net::TcpListener,
    config: Arc<ServerConfig>,
    opts: &Options,
    local: Option<&LocalSet>,
) -> Result<(), anyhow::Error> {
    let listener = TcpListener::from_std(std_listener)?;
    info!("Thread {} listening on {}", id, addr);

    loop {
        let (stream, _) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                error!("Thread {} accept error: {}", id, e);
                continue;
            }
        };

        let io = TokioIo::new(stream);
        let config = config.clone();
        let use_http2 = opts.http2;
        let verbose = opts.verbose;
        let delay = opts.delay;
        let body_delay = opts.body_delay;
        let meter = opts.meter;

        let task = async move {
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
                        Some(delay) => {
                            Either::Left(DelayedBody::new(Full::new(config.body.clone()), delay))
                        }
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
                            let mut print_builder = Response::builder().status(resp.status());
                            for (k, v) in resp.headers() {
                                print_builder = print_builder.header(k, v);
                            }
                            let print_resp = print_builder.body(config.body.clone()).unwrap();
                            println!("↪ {}:\n{}", "response".bold(), print_resp.pretty(verbose));
                        }
                    }

                    resp
                }
            });

            let result = if use_http2 {
                http2::Builder::new(hyper_util::rt::TokioExecutor::new())
                    .serve_connection(io, service)
                    .await
            } else {
                http1::Builder::new().serve_connection(io, service).await
            };

            if let Err(err) = result {
                let protocol = if use_http2 { "HTTP/2" } else { "HTTP/1.1" };
                error!("Error serving {} connection: {:?}", protocol, err);
            }
        };
        if let Some(local) = local {
            local.spawn_local(task);
        } else {
            tokio::task::spawn(task);
        }
    }
}

pub fn run_thread(
    id: usize,
    addr: SocketAddr,
    config: Arc<ServerConfig>,
    opts: &Options,
) -> Result<()> {
    // Standard Tokio single-thread runtime - create socket with SO_REUSEPORT
    let std_listener = create_listener(addr, opts)?;

    let mut builder = tokio::runtime::Builder::new_current_thread();
    let rt = builder
        .enable_all()
        .thread_name(format!("thread-{}", id))
        .build()?;

    rt.block_on(tokio_srv(id, addr, std_listener, config, opts, None))
}

pub fn run_thread_local(
    id: usize,
    addr: SocketAddr,
    config: Arc<ServerConfig>,
    opts: &Options,
) -> Result<()> {
    // Standard Tokio single-thread runtime - create socket with SO_REUSEPORT
    let std_listener = create_listener(addr, opts)?;

    let mut builder = tokio::runtime::Builder::new_current_thread();
    let rt = builder
        .enable_all()
        .thread_name(format!("thread-{}", id))
        .build()?;

    rt.block_on(async move {
        let local = LocalSet::new();
        local
            .run_until(tokio_srv(
                id,
                addr,
                std_listener,
                config,
                opts,
                Some(&local),
            ))
            .await
    })
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
