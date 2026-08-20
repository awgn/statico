use anyhow::{Context, Result};
use futures::stream::{select_all, unfold};
use http_body_util::Either;
use http_body_util::Full;
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
use tracing::{error, info};

use crate::delayed_body::DelayedBody;
use crate::execute_delay;
use crate::http::chunked_body_wire_size;
use crate::http::{request_head_size, response_head_size};
use crate::PORT_COUNTERS;
use crate::REQUESTS;
use crate::REQUEST_BYTES;
use crate::RESPONSES;
use crate::RESPONSE_BYTES;

use crate::create_listener;
use crate::options::Options;
use crate::pretty::collect_request;
use crate::pretty::PrettyPrint;
use crate::ServerConfig;
use futures::StreamExt;

async fn tokio_srv(
    id: usize,
    addrs: Vec<SocketAddr>,
    std_listeners: Vec<std::net::TcpListener>,
    config: Arc<ServerConfig>,
    opts: &Options,
) -> Result<()> {
    // Convert std listeners to tokio listeners
    let listeners: Vec<TcpListener> = std_listeners
        .into_iter()
        .map(|l| TcpListener::from_std(l))
        .collect::<std::result::Result<Vec<_>, std::io::Error>>()
        .context("Failed to convert std listeners to tokio listeners")?;

    info!("Thread {} listening on {:?}", id, addrs);

    // Create futures

    let mut all_listeners = select_all(listeners.into_iter().map(|l| {
        let port = l.local_addr().unwrap().port();
        Box::pin(unfold(l, move |listener| async move {
            match listener.accept().await {
                Ok((tcp_stream, _)) => Some((Ok((tcp_stream, port)), listener)),
                Err(e) => Some((Err(e), listener)),
            }
        }))
    }));

    loop {
        tokio::select! {
            incoming = all_listeners.next() => {
                match incoming {
                    Some(Ok((tcp_stream, port))) => {
                        let io = TokioIo::new(tcp_stream);
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
                                        let req_bytes_total = if let Some(ref req) = collected_req {
                                            let body_bytes = req.0.body().len();
                                            // For chunked encoding, calculate the wire format overhead
                                            let body_size = if is_chunked {
                                                chunked_body_wire_size(body_bytes)
                                            } else {
                                                body_bytes
                                            };
                                            head_size + body_size
                                        } else {
                                            head_size
                                        };

                                        REQUESTS.add(1);
                                        REQUEST_BYTES.add(req_bytes_total);
                                        let entry = PORT_COUNTERS.entry(port).or_default();
                                        entry.requests.add(1);
                                        entry.request_bytes.add(req_bytes_total);
                                    }

                                    if verbose > 0 {
                                        if let Some(ref req) = collected_req {
                                            println!("↩ {}:\n{}", "request".bold(), req.0.pretty(verbose));
                                            if let Some(ref trailers) = req.1 {
                                                println!("{}", trailers.pretty(verbose));
                                            }
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
                                            let head_size = response_head_size(resp, config.body.len());
                                            let res_bytes_total = head_size + config.body.len();
                                            RESPONSES.add(1);
                                            RESPONSE_BYTES.add(res_bytes_total);
                                            let entry = PORT_COUNTERS.entry(port).or_default();
                                            entry.responses.add(1);
                                            entry.response_bytes.add(res_bytes_total);
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
                        tokio::task::spawn(task);
                    },
                    Some(Err(e)) => {
                        error!("Thread {} accept error on listener: {}", id, e);
                    }
                    None => {
                        error!("Thread {} all listeners closed", id);
                        break;
                    }
                }
            }
        }
    }
    Ok(())
}

pub fn run_thread(
    id: usize,
    addr: Vec<SocketAddr>,
    config: Arc<ServerConfig>,
    opts: &Options,
) -> Result<()> {
    // Standard Tokio single-thread runtime - create socket with SO_REUSEPORT
    let std_listeners: Vec<std::net::TcpListener> = addr
        .iter()
        .map(|a| create_listener(a.clone(), opts))
        .collect::<Result<Vec<_>>>()?;

    let mut builder = tokio::runtime::Builder::new_current_thread();
    let rt = builder
        .enable_all()
        .thread_name(format!("thread-{}", id))
        .build()?;

    rt.block_on(tokio_srv(id, addr, std_listeners, config, opts))
}
