//! Minimal MCP mock over Streamable HTTP.
//!
//! Only tokio + hyper, no other dependencies: the JSON-RPC payload is scanned
//! by hand (single-pass, zero-alloc) extracting only `method`, `id` (raw) and,
//! for `initialize`, `protocolVersion` inside `params`.
//!
//! Supported methods:
//! - `initialize` -> 200 with the `tools` capability
//! - `notifications/*` (including `notifications/initialized`) -> 202, no body
//! - `tools/list` -> 200 with the supported tools (`answer`, `echo`)
//! - `ping` -> 200 at maximum speed (static body + spliced `id`)
//! - `tools/call` -> 200 invoking `answer` or `echo` (`id` spliced, result built)
//! - unknown with `id` -> 200 with JSON-RPC `-32601`; without `id` -> 202
//! - malformed JSON -> 400 with `-32700`
//!
//! HTTP:
//! - `POST <endpoint>` = MCP endpoint (single JSON-RPC per request)
//! - `GET`/`DELETE` on `<endpoint>` -> 405 (no SSE stream, allowed by the spec)
//! - different path -> 404
//! - stateless: no `Mcp-Session-Id` header emitted or required.

use anyhow::{Context, Result};
use bytes::Bytes;
use futures::stream::{select_all, unfold};
use http_body_util::Full;
use hyper::header::{CONTENT_LENGTH, CONTENT_TYPE};
use hyper::server::conn::{http1, http2};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use owo_colors::OwoColorize;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tracing::error;

use crate::create_listener;
use crate::http::{chunked_body_wire_size, request_head_size, response_head_size};
use crate::options::Options;
use crate::port_counters;
use crate::pretty::{collect_request, PrettyPrint};
use futures::StreamExt;

const JSON_CONTENT_TYPE: &str = "application/json";
const DEFAULT_PROTOCOL_VERSION: &str = "2025-06-18";
const SERVER_NAME: &str = "statico-everything";

const PREFIX: &[u8] = b"{\"jsonrpc\":\"2.0\",\"id\":";

const INIT_RESULT_PREFIX: &[u8] = b",\"result\":{\"protocolVersion\":\"";
const INIT_RESULT_MID: &[u8] =
    b"\",\"capabilities\":{\"tools\":{\"listChanged\":false}},\"serverInfo\":{\"name\":\"";
const INIT_RESULT_MID2: &[u8] = b"\",\"version\":\"";
const INIT_RESULT_SUFFIX: &[u8] =
    b"\"},\"instructions\":\"Minimal everything mock for benchmarking\"}}";

const TOOLS_LIST_RESULT: &[u8] = b",\"result\":{\"tools\":[{\"name\":\"answer\",\"description\":\"Always answers 42\",\"inputSchema\":{\"type\":\"object\",\"properties\":{}}},{\"name\":\"echo\",\"description\":\"Echoes back the input text\",\"inputSchema\":{\"type\":\"object\",\"properties\":{\"text\":{\"type\":\"string\",\"description\":\"Text to echo\"}},\"required\":[\"text\"]}}]}}";

const PING_RESULT: &[u8] = b",\"result\":{}}";

/// `tools/call` result for the `answer` tool (always 42).
const TOOLS_CALL_ANSWER_RESULT: &[u8] =
    b",\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"42\"}],\"isError\":false}}";
/// Fragments used to build dynamic `tools/call` results (`answer` aside).
const TOOLS_CALL_TEXT_PREFIX: &[u8] = b",\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"";
const TOOLS_CALL_TEXT_SUFFIX: &[u8] = b"\"}],\"isError\":false}}";
const TOOLS_CALL_UNKNOWN_PREFIX: &[u8] =
    b",\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"unknown tool ";
const TOOLS_CALL_UNKNOWN_SUFFIX: &[u8] = b"\"}],\"isError\":true}}";

const ERR_PARSE: &[u8] =
    b"{\"jsonrpc\":\"2.0\",\"id\":null,\"error\":{\"code\":-32700,\"message\":\"Parse error\"}}";
const ERR_INVALID_SUFFIX: &[u8] = b",\"error\":{\"code\":-32600,\"message\":\"Invalid Request\"}}";
const ERR_INVALID_PARAMS_SUFFIX: &[u8] =
    b",\"error\":{\"code\":-32602,\"message\":\"Invalid params\"}}";
const ERR_METHOD_PREFIX: &[u8] = b"{\"jsonrpc\":\"2.0\",\"id\":";
const ERR_METHOD_SUFFIX: &[u8] = b",\"error\":{\"code\":-32601,\"message\":\"Method not found\"}}";

#[inline]
fn is_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r')
}

#[inline]
fn skip_ws(body: &[u8], mut i: usize) -> usize {
    while i < body.len() && is_ws(body[i]) {
        i += 1;
    }
    i
}

/// Parses a JSON string starting at the opening `"`.
/// Returns (content_start, content_end, pos_after_closing_quote).
#[inline]
fn parse_string(body: &[u8], pos: usize) -> Option<(usize, usize, usize)> {
    if *body.get(pos)? != b'"' {
        return None;
    }
    let start = pos + 1;
    let mut i = start;
    while i < body.len() {
        match body[i] {
            b'\\' => i += 2, // skip the escaped char (\" \\ \u...: never a raw `"`)
            b'"' => return Some((start, i, i + 1)),
            _ => i += 1,
        }
    }
    None
}

/// Skips any JSON value starting at `pos`. Returns the pos right after the value.
#[inline]
fn skip_value(body: &[u8], pos: usize) -> Option<usize> {
    let mut i = skip_ws(body, pos);
    let c = *body.get(i)?;
    match c {
        b'"' => {
            let (_, _, next) = parse_string(body, i)?;
            Some(next)
        }
        b'{' | b'[' => {
            let open = c;
            let close = if open == b'{' { b'}' } else { b']' };
            i += 1;
            let mut depth = 1usize;
            while i < body.len() {
                match body[i] {
                    b'"' => {
                        let (_, _, next) = parse_string(body, i)?;
                        i = next;
                    }
                    b'{' if open == b'{' => {
                        depth += 1;
                        i += 1;
                    }
                    b'[' if open == b'[' => {
                        // Nested array inside an array: every opening counts as +1
                        // and every closing as -1; for skipping this stays correct
                        // as long as the JSON is well-formed (valid JSON is).
                        depth += 1;
                        i += 1;
                    }
                    b'{' | b'[' => {
                        depth += 1;
                        i += 1;
                    }
                    b'}' | b']' => {
                        // Decrement on any closing bracket; for skipping this stays
                        // correct on well-formed JSON (mixed nesting closes in order).
                        if body[i] == close {
                            depth -= 1;
                            i += 1;
                            if depth == 0 {
                                return Some(i);
                            }
                        } else {
                            // Closing a nested level of a different kind.
                            depth -= 1;
                            i += 1;
                        }
                    }
                    b'\\' => i += 1, // cannot happen outside a string; harmless
                    _ => i += 1,
                }
            }
            None
        }
        _ => {
            // number / true / false / null: consume up to a delimiter
            while i < body.len()
                && !matches!(body[i], b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r')
            {
                i += 1;
            }
            Some(i)
        }
    }
}

struct Rpc<'a> {
    method: Option<&'a [u8]>,
    id_raw: Option<&'a [u8]>,
    params_raw: Option<&'a [u8]>,
}

/// Scans the top-level JSON-RPC object. `None` = malformed (not an object).
fn parse_rpc(body: &[u8]) -> Option<Rpc<'_>> {
    let mut i = skip_ws(body, 0);
    if *body.get(i)? != b'{' {
        return None;
    }
    i += 1;

    let mut method: Option<&[u8]> = None;
    let mut id_raw: Option<&[u8]> = None;
    let mut params_raw: Option<&[u8]> = None;

    loop {
        i = skip_ws(body, i);
        let c = *body.get(i)?;
        if c == b'}' {
            return Some(Rpc {
                method,
                id_raw,
                params_raw,
            });
        }
        // key
        let (ks, ke, next) = parse_string(body, i)?;
        let key = &body[ks..ke];
        i = skip_ws(body, next);
        if *body.get(i)? != b':' {
            return None;
        }
        i = skip_ws(body, i + 1);

        if key == b"method" {
            let (ms, me, next) = parse_string(body, i)?;
            method = Some(&body[ms..me]);
            i = next;
        } else if key == b"id" {
            if *body.get(i)? == b'"' {
                let (_, _, next) = parse_string(body, i)?;
                id_raw = Some(&body[i..next]); // keep the quotes
                i = next;
            } else {
                let start = i;
                // raw token (number / null / true / false)
                while i < body.len()
                    && !matches!(body[i], b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r')
                {
                    i += 1;
                }
                if start == i {
                    return None;
                }
                id_raw = Some(&body[start..i]);
            }
        } else if key == b"params" {
            let start = i;
            i = skip_value(body, i)?;
            params_raw = Some(&body[start..i]);
        } else {
            i = skip_value(body, i)?;
        }

        i = skip_ws(body, i);
        match *body.get(i)? {
            b',' => i += 1,
            b'}' => {
                return Some(Rpc {
                    method,
                    id_raw,
                    params_raw,
                })
            }
            _ => return None,
        }
    }
}

/// Looks for `"protocolVersion": "<v>"` among the top-level keys of `params`.
fn find_protocol_version(params: &[u8]) -> Option<&[u8]> {
    let mut i = skip_ws(params, 0);
    if *params.get(i)? != b'{' {
        return None;
    }
    i += 1;
    loop {
        i = skip_ws(params, i);
        if *params.get(i)? == b'}' {
            return None;
        }
        let (ks, ke, next) = parse_string(params, i)?;
        let key = &params[ks..ke];
        i = skip_ws(params, next);
        if *params.get(i)? != b':' {
            return None;
        }
        i = skip_ws(params, i + 1);
        if key == b"protocolVersion" {
            let (vs, ve, _) = parse_string(params, i)?;
            return Some(&params[vs..ve]);
        } else {
            i = skip_value(params, i)?;
        }
        i = skip_ws(params, i);
        match *params.get(i)? {
            b',' => i += 1,
            b'}' => return None,
            _ => return None,
        }
    }
}

/// Returns the raw JSON value slice for top-level `wanted` inside object `obj`.
/// For strings the quotes are included; for anything else the exact raw token.
fn find_raw_value<'a>(obj: &'a [u8], wanted: &[u8]) -> Option<&'a [u8]> {
    let mut i = skip_ws(obj, 0);
    if *obj.get(i)? != b'{' {
        return None;
    }
    i += 1;
    loop {
        i = skip_ws(obj, i);
        if *obj.get(i)? == b'}' {
            return None;
        }
        let (ks, ke, next) = parse_string(obj, i)?;
        let key = &obj[ks..ke];
        i = skip_ws(obj, next);
        if *obj.get(i)? != b':' {
            return None;
        }
        i = skip_ws(obj, i + 1);
        let start = i;
        i = skip_value(obj, i)?;
        if key == wanted {
            return Some(&obj[start..i]);
        }
        i = skip_ws(obj, i);
        match *obj.get(i)? {
            b',' => i += 1,
            b'}' => return None,
            _ => return None,
        }
    }
}

/// Strips the surrounding quotes of a raw JSON string. `None` if not a string.
#[inline]
fn string_inner(raw: &[u8]) -> Option<&[u8]> {
    if raw.len() >= 2 && raw[0] == b'"' && raw[raw.len() - 1] == b'"' {
        Some(&raw[1..raw.len() - 1])
    } else {
        None
    }
}

/// Scans the top-level fields of a `tools/call` `params` object in a single pass,
/// returning the raw `name` value and the raw `arguments` value (if present).
/// `None` = malformed params or missing `name`.
fn find_call_target(params: &[u8]) -> Option<(&[u8], Option<&[u8]>)> {
    let mut i = skip_ws(params, 0);
    if *params.get(i)? != b'{' {
        return None;
    }
    i += 1;
    let mut name: Option<&[u8]> = None;
    let mut arguments: Option<&[u8]> = None;
    loop {
        i = skip_ws(params, i);
        if *params.get(i)? == b'}' {
            return Some((name?, arguments));
        }
        let (ks, ke, next) = parse_string(params, i)?;
        let key = &params[ks..ke];
        i = skip_ws(params, next);
        if *params.get(i)? != b':' {
            return None;
        }
        i = skip_ws(params, i + 1);
        let start = i;
        i = skip_value(params, i)?;
        if key == b"name" {
            name = Some(&params[start..i]);
        } else if key == b"arguments" {
            arguments = Some(&params[start..i]);
        }
        i = skip_ws(params, i);
        match *params.get(i)? {
            b',' => i += 1,
            b'}' => return Some((name?, arguments)),
            _ => return None,
        }
    }
}

/// Handles `tools/call`: dispatches to `answer` (always 42) or `echo` (returns
/// the `text` argument verbatim).
/// Returns `None` when the params are missing or malformed (caller maps it to
/// JSON-RPC `-32602 Invalid params`). Unknown tool names produce a result
/// with `isError: true` instead of a protocol error.
fn handle_tools_call(id: &[u8], params_raw: Option<&[u8]>) -> Option<Vec<u8>> {
    let (name_raw, arguments_raw) = find_call_target(params_raw?)?;
    let name_inner = string_inner(name_raw)?;
    if name_inner == b"answer" {
        return Some(build_with_id(id, TOOLS_CALL_ANSWER_RESULT));
    }
    if name_inner == b"echo" {
        let text_inner = string_inner(find_raw_value(arguments_raw?, b"text")?)?;
        // `text_inner` is the already-escaped content of a JSON string, so it
        // can be spliced verbatim into the output text field.
        return Some(build_text_result(id, text_inner));
    }
    Some(build_unknown_tool(id, name_inner))
}

fn build_text_result(id: &[u8], text: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(
        PREFIX.len()
            + id.len()
            + TOOLS_CALL_TEXT_PREFIX.len()
            + text.len()
            + TOOLS_CALL_TEXT_SUFFIX.len(),
    );
    out.extend_from_slice(PREFIX);
    out.extend_from_slice(id);
    out.extend_from_slice(TOOLS_CALL_TEXT_PREFIX);
    out.extend_from_slice(text);
    out.extend_from_slice(TOOLS_CALL_TEXT_SUFFIX);
    out
}

fn build_unknown_tool(id: &[u8], name_inner: &[u8]) -> Vec<u8> {
    // `name_inner` is the already-escaped content of a JSON string, so copying
    // it verbatim into the output string cannot break out of the quotes.
    let mut out = Vec::with_capacity(
        PREFIX.len()
            + id.len()
            + TOOLS_CALL_UNKNOWN_PREFIX.len()
            + name_inner.len()
            + TOOLS_CALL_UNKNOWN_SUFFIX.len(),
    );
    out.extend_from_slice(PREFIX);
    out.extend_from_slice(id);
    out.extend_from_slice(TOOLS_CALL_UNKNOWN_PREFIX);
    out.extend_from_slice(name_inner);
    out.extend_from_slice(TOOLS_CALL_UNKNOWN_SUFFIX);
    out
}

#[inline]
fn has_id(id_raw: Option<&[u8]>) -> bool {
    match id_raw {
        None => false,
        Some(b"null") => false,
        Some(_) => true,
    }
}

#[inline]
fn is_notifications(method: &[u8]) -> bool {
    method.len() > 14 && method.starts_with(b"notifications/")
}

fn build_with_id(id: &[u8], result_fragment: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(PREFIX.len() + id.len() + result_fragment.len());
    out.extend_from_slice(PREFIX);
    out.extend_from_slice(id);
    out.extend_from_slice(result_fragment);
    out
}

fn build_initialize(id: &[u8], version: &[u8]) -> Vec<u8> {
    let ver = env!("CARGO_PKG_VERSION");
    let mut out = Vec::with_capacity(
        PREFIX.len()
            + id.len()
            + INIT_RESULT_PREFIX.len()
            + version.len()
            + INIT_RESULT_MID.len()
            + SERVER_NAME.len()
            + INIT_RESULT_MID2.len()
            + ver.len()
            + INIT_RESULT_SUFFIX.len(),
    );
    out.extend_from_slice(PREFIX);
    out.extend_from_slice(id);
    out.extend_from_slice(INIT_RESULT_PREFIX);
    out.extend_from_slice(version);
    out.extend_from_slice(INIT_RESULT_MID);
    out.extend_from_slice(SERVER_NAME.as_bytes());
    out.extend_from_slice(INIT_RESULT_MID2);
    out.extend_from_slice(ver.as_bytes());
    out.extend_from_slice(INIT_RESULT_SUFFIX);
    out
}

fn build_error_with_id(id: &[u8], suffix: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(ERR_METHOD_PREFIX.len() + id.len() + suffix.len());
    out.extend_from_slice(ERR_METHOD_PREFIX);
    out.extend_from_slice(id);
    out.extend_from_slice(suffix);
    out
}

/// Returns (status, body). Empty body = no JSON Tx.
fn dispatch(body: &[u8]) -> (StatusCode, Option<Vec<u8>>) {
    if body.is_empty() {
        return (StatusCode::BAD_REQUEST, Some(ERR_PARSE.to_vec()));
    }
    let Some(rpc) = parse_rpc(body) else {
        return (StatusCode::BAD_REQUEST, Some(ERR_PARSE.to_vec()));
    };

    let Some(method) = rpc.method else {
        // notification without method -> accept; request without method -> Invalid Request
        if has_id(rpc.id_raw) {
            let id = rpc.id_raw.unwrap_or(b"null");
            return (
                StatusCode::BAD_REQUEST,
                Some(build_error_with_id(id, ERR_INVALID_SUFFIX)),
            );
        }
        return (StatusCode::ACCEPTED, None);
    };

    if is_notifications(method) {
        // notifications/initialized and friends: discard, no Tx
        return (StatusCode::ACCEPTED, None);
    }

    // From here on: only requests with an id produce a Tx; notifications are just accepted.
    if !has_id(rpc.id_raw) {
        return (StatusCode::ACCEPTED, None);
    }
    let id = rpc.id_raw.unwrap_or(b"null");

    if method == b"initialize" {
        let version = rpc
            .params_raw
            .and_then(find_protocol_version)
            .unwrap_or(DEFAULT_PROTOCOL_VERSION.as_bytes());
        return (StatusCode::OK, Some(build_initialize(id, version)));
    }
    if method == b"tools/list" {
        return (StatusCode::OK, Some(build_with_id(id, TOOLS_LIST_RESULT)));
    }
    if method == b"ping" {
        return (StatusCode::OK, Some(build_with_id(id, PING_RESULT)));
    }
    if method == b"tools/call" {
        match handle_tools_call(id, rpc.params_raw) {
            Some(body) => return (StatusCode::OK, Some(body)),
            None => {
                return (
                    StatusCode::OK,
                    Some(build_error_with_id(id, ERR_INVALID_PARAMS_SUFFIX)),
                );
            }
        }
    }

    (
        StatusCode::OK,
        Some(build_error_with_id(id, ERR_METHOD_SUFFIX)),
    )
}

/// Cold path only (unreadable request body): kept out of line so the hot path
/// stays tight in icache.
#[cold]
fn json_response(status: StatusCode, body: Vec<u8>) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, JSON_CONTENT_TYPE)
        .header(CONTENT_LENGTH, body.len())
        .body(Full::new(Bytes::from(body)))
        .unwrap()
}

fn empty_response(status: StatusCode) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header(CONTENT_LENGTH, 0u32)
        .body(Full::new(Bytes::new()))
        .unwrap()
}

/// Logs the head of a request rejected with 404/405 before its body is read.
/// The main server (`tokio.rs`) dumps the whole collected request instead;
/// here the body is deliberately left unread, so only the head is shown.
#[cold]
fn log_request_head(req: &Request<hyper::body::Incoming>, verbose: u8) {
    println!(
        "↩ {}:\n{} {} {:?}",
        "request".bold(),
        req.method().bold().blue(),
        req.uri(),
        req.version()
    );
    if verbose > 1 {
        for (name, value) in req.headers() {
            println!("{}: {}", name, value.to_str().unwrap_or("<binary>"));
        }
    }
}

/// Logs a response the same way the main server does (see `tokio.rs`):
/// status line, headers (`-vv` and up) and body (`-vvv` and up, via `pretty`).
#[cold]
fn log_response<B>(resp: &Response<B>, body: Bytes, verbose: u8) {
    let mut print_builder = Response::builder()
        .status(resp.status())
        .version(resp.version());
    for (k, v) in resp.headers() {
        print_builder = print_builder.header(k, v);
    }
    let print_resp = print_builder.body(body).unwrap();
    println!("↪ {}:\n{}", "response".bold(), print_resp.pretty(verbose));
}

async fn serve_http<I, S, Bd>(
    io: TokioIo<I>,
    service: S,
    use_http2: bool,
) -> Result<(), hyper::Error>
where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    S: hyper::service::Service<Request<hyper::body::Incoming>, Response = Response<Bd>>,
    S::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    S::Future: Send + 'static,
    Bd: http_body::Body + Send + 'static,
    Bd::Data: Send,
    Bd::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    if use_http2 {
        http2::Builder::new(hyper_util::rt::TokioExecutor::new())
            .serve_connection(io, service)
            .await
    } else {
        http1::Builder::new().serve_connection(io, service).await
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_conn(
    tcp_stream: tokio::net::TcpStream,
    tls_cfg: Option<Arc<rustls::ServerConfig>>,
    use_http2: bool,
    verbose: u8,
    counters: Option<&'static crate::PortCounters>,
    endpoint: Arc<str>,
    port: u16,
    meter: bool,
) {
    let service = service_fn(move |req: Request<hyper::body::Incoming>| {
        let endpoint = endpoint.clone();
        async move {
            let req_version = req.version();

            let (head_size, is_chunked) = if meter {
                let hs = request_head_size(&req);
                let chunked = req
                    .headers()
                    .get(hyper::header::TRANSFER_ENCODING)
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.contains("chunked"))
                    .unwrap_or(false);
                (hs, chunked)
            } else {
                (0, false)
            };

            // Only the MCP endpoint accepts POST; anything else -> 404/405 without reading the body.
            if req.uri().path() != &*endpoint {
                let resp = empty_response(StatusCode::NOT_FOUND);
                if verbose > 0 {
                    log_request_head(&req, verbose);
                    log_response(&resp, Bytes::new(), verbose);
                }
                if let Some(c) = counters {
                    c.record_request(head_size);
                    let hs = response_head_size(&resp, 0);
                    c.record_response(hs);
                }
                return Ok::<_, std::convert::Infallible>(resp);
            }

            if req.method() != Method::POST {
                let resp = empty_response(StatusCode::METHOD_NOT_ALLOWED);
                if verbose > 0 {
                    log_request_head(&req, verbose);
                    log_response(&resp, Bytes::new(), verbose);
                }
                if let Some(c) = counters {
                    c.record_request(head_size);
                    let hs = response_head_size(&resp, 0);
                    c.record_response(hs);
                }
                return Ok::<_, std::convert::Infallible>(resp);
            }

            // POST on the endpoint: collect the body (needed to parse method/id),
            // keeping the headers so verbose output matches the main server.
            let (collected_req, trailers) = match collect_request(req).await {
                Ok(pair) => pair,
                Err(_) => {
                    let resp = json_response(StatusCode::BAD_REQUEST, ERR_PARSE.to_vec());
                    if let Some(c) = counters {
                        c.record_request(head_size);
                        let hs = response_head_size(&resp, ERR_PARSE.len());
                        c.record_response(hs + ERR_PARSE.len());
                    }
                    return Ok(resp);
                }
            };
            let collected: &[u8] = collected_req.body();

            if let Some(c) = counters {
                let body_size = if is_chunked {
                    chunked_body_wire_size(collected.len())
                } else {
                    collected.len()
                };
                c.record_request(head_size + body_size);
            }

            if verbose > 0 {
                println!("↩ {}:\n{}", "request".bold(), collected_req.pretty(verbose));
                if let Some(ref trailers) = trailers {
                    println!("{}", trailers.pretty(verbose));
                }
            }

            let (status, body_opt) = dispatch(collected);

            let mut builder = Response::builder().status(status).version(req_version);
            if let Some(body) = body_opt {
                let body_len = body.len();
                builder = builder
                    .header(CONTENT_TYPE, JSON_CONTENT_TYPE)
                    .header(CONTENT_LENGTH, body_len);
                // Clone for the log before moving the body into the response.
                let print_body = if verbose > 0 {
                    Some(Bytes::copy_from_slice(&body))
                } else {
                    None
                };
                let resp = builder.body(Full::new(Bytes::from(body))).unwrap();
                if let Some(c) = counters {
                    let hs = response_head_size(&resp, body_len);
                    c.record_response(hs + body_len);
                }
                if let Some(print_body) = print_body {
                    log_response(&resp, print_body, verbose);
                }
                Ok(resp)
            } else {
                builder = builder.header(CONTENT_LENGTH, 0u32);
                let resp = builder.body(Full::new(Bytes::new())).unwrap();
                if let Some(c) = counters {
                    let hs = response_head_size(&resp, 0);
                    c.record_response(hs);
                }
                if verbose > 0 {
                    log_response(&resp, Bytes::new(), verbose);
                }
                Ok(resp)
            }
        }
    });

    let result = if let Some(server_config) = tls_cfg {
        let acceptor = TlsAcceptor::from(server_config);
        match acceptor.accept(tcp_stream).await {
            Ok(tls_stream) => serve_http(TokioIo::new(tls_stream), service, use_http2).await,
            Err(e) => {
                error!("TLS handshake error: {:?}", e);
                return;
            }
        }
    } else {
        serve_http(TokioIo::new(tcp_stream), service, use_http2).await
    };

    if let Err(err) = result {
        let protocol = if use_http2 { "HTTP/2" } else { "HTTP/1.1" };
        error!("Error serving MCP {} connection: {:?}", protocol, err);
    }
    let _ = port;
}

pub fn run_thread(
    id: usize,
    addr: Vec<SocketAddr>,
    tls: Option<Arc<rustls::ServerConfig>>,
    opts: &Options,
) -> Result<()> {
    let endpoint: Arc<str> = {
        let ep = opts.mcp_everything.as_deref().unwrap_or("/mcp");
        Arc::from(ep)
    };
    let std_listeners: Vec<std::net::TcpListener> = addr
        .iter()
        .map(|a| create_listener(*a, opts))
        .collect::<Result<Vec<_>>>()?;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .thread_name(format!("mcp-thread-{}", id))
        .build()?;

    rt.block_on(mcp_srv_with_tls(
        id,
        addr,
        std_listeners,
        endpoint,
        tls,
        opts,
    ))
}

async fn mcp_srv_with_tls(
    id: usize,
    addrs: Vec<SocketAddr>,
    std_listeners: Vec<std::net::TcpListener>,
    endpoint: Arc<str>,
    tls: Option<Arc<rustls::ServerConfig>>,
    opts: &Options,
) -> Result<()> {
    let listeners: Vec<TcpListener> = std_listeners
        .into_iter()
        .map(TcpListener::from_std)
        .collect::<std::result::Result<Vec<_>, std::io::Error>>()
        .context("Failed to convert std listeners to tokio listeners")?;

    tracing::info!(
        "MCP thread {} listening on {:?} (endpoint {})",
        id,
        addrs,
        endpoint.as_ref()
    );

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
                        let endpoint = endpoint.clone();
                        let tls_cfg = tls.clone();
                        let use_http2 = opts.http2;
                        let verbose = opts.verbose;
                        let meter = opts.meter;
                        let counters = meter.then(|| port_counters(port));
                        tokio::task::spawn(async move {
                            handle_conn(tcp_stream, tls_cfg, use_http2, verbose, counters, endpoint, port, meter).await;
                        });
                    },
                    Some(Err(e)) => {
                        error!("MCP thread {} accept error: {}", id, e);
                    }
                    None => {
                        error!("MCP thread {} all listeners closed", id);
                        break;
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn post(body: &[u8]) -> (StatusCode, Option<Vec<u8>>) {
        dispatch(body)
    }

    #[test]
    fn initialize_numeric_id() {
        let (st, b) = post(br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"c","version":"1"}}}"#);
        assert_eq!(st, StatusCode::OK);
        let b = b.unwrap();
        let s = String::from_utf8_lossy(&b);
        assert!(s.contains(r#""id":1"#), "{s}");
        assert!(s.contains(r#""protocolVersion":"2025-06-18""#), "{s}");
        assert!(s.contains(r#""tools""#), "{s}");
    }

    #[test]
    fn initialize_string_id_and_version_echo() {
        let (st, b) = post(br#"{"jsonrpc":"2.0","id":"abc-1","method":"initialize","params":{"protocolVersion":"2024-11-05"}}"#);
        assert_eq!(st, StatusCode::OK);
        let b = b.unwrap();
        let s = String::from_utf8_lossy(&b);
        assert!(s.contains(r#""id":"abc-1""#), "{s}");
        assert!(s.contains(r#""protocolVersion":"2024-11-05""#), "{s}");
    }

    #[test]
    fn initialized_notification_is_discarded() {
        let (st, b) = post(br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
        assert_eq!(st, StatusCode::ACCEPTED);
        assert!(b.is_none());
    }

    #[test]
    fn tools_list() {
        let (st, b) = post(br#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#);
        assert_eq!(st, StatusCode::OK);
        let b = b.unwrap();
        let s = String::from_utf8_lossy(&b);
        assert!(s.contains(r#""id":2"#), "{s}");
        assert!(s.contains(r#""name":"answer""#), "{s}");
        assert!(s.contains(r#""name":"echo""#), "{s}");
    }

    #[test]
    fn ping_fast() {
        let (st, b) = post(br#"{"jsonrpc":"2.0","id":3,"method":"ping"}"#);
        assert_eq!(st, StatusCode::OK);
        let b = b.unwrap();
        let s = String::from_utf8_lossy(&b);
        assert_eq!(s, r#"{"jsonrpc":"2.0","id":3,"result":{}}"#);
    }

    #[test]
    fn tools_call_answer_is_42() {
        let (st, b) =
            post(br#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"answer"}}"#);
        assert_eq!(st, StatusCode::OK);
        let b = b.unwrap();
        let s = String::from_utf8_lossy(&b);
        assert!(s.contains(r#""id":9"#), "{s}");
        assert!(s.contains(r#""text":"42""#), "{s}");
        assert!(s.contains(r#""isError":false"#), "{s}");
    }

    #[test]
    fn tools_call_echo_returns_argument() {
        let (st, b) = post(br#"{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"echo","arguments":{"text":"hello"}}}"#);
        assert_eq!(st, StatusCode::OK);
        let b = b.unwrap();
        let s = String::from_utf8_lossy(&b);
        assert!(s.contains(r#""id":10"#), "{s}");
        assert!(s.contains(r#""text":"hello""#), "{s}");
        assert!(s.contains(r#""isError":false"#), "{s}");
    }

    #[test]
    fn tools_call_echo_preserves_escapes() {
        let (st, b) = post(br#"{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"echo","arguments":{"text":"a\"b\\c"}}}"#);
        assert_eq!(st, StatusCode::OK);
        let b = b.unwrap();
        let s = String::from_utf8_lossy(&b);
        assert!(s.contains(r#""id":11"#), "{s}");
        assert!(s.contains(r#""text":"a\"b\\c""#), "{s}");
    }

    #[test]
    fn tools_call_unknown_tool_is_error_result() {
        let (st, b) =
            post(br#"{"jsonrpc":"2.0","id":12,"method":"tools/call","params":{"name":"nope"}}"#);
        assert_eq!(st, StatusCode::OK);
        let b = b.unwrap();
        let s = String::from_utf8_lossy(&b);
        assert!(s.contains(r#""id":12"#), "{s}");
        assert!(s.contains(r#""isError":true"#), "{s}");
    }

    #[test]
    fn tools_call_missing_args_is_invalid_params() {
        let (st, b) = post(br#"{"jsonrpc":"2.0","id":13,"method":"tools/call","params":{"name":"echo","arguments":{}}}"#);
        assert_eq!(st, StatusCode::OK);
        let b = b.unwrap();
        let s = String::from_utf8_lossy(&b);
        assert!(s.contains("-32602"), "{s}");
    }

    #[test]
    fn empty_body_is_parse_error() {
        let (st, b) = post(b"");
        assert_eq!(st, StatusCode::BAD_REQUEST);
        let b = b.unwrap();
        let s = String::from_utf8_lossy(&b);
        assert!(s.contains("-32700"), "{s}");
    }

    #[test]
    fn batch_array_is_parse_error() {
        let (st, b) = post(br#"[{"jsonrpc":"2.0","id":1,"method":"ping"}]"#);
        assert_eq!(st, StatusCode::BAD_REQUEST);
        let b = b.unwrap();
        let s = String::from_utf8_lossy(&b);
        assert!(s.contains("-32700"), "{s}");
    }

    #[test]
    fn request_without_method_is_invalid_request() {
        let (st, b) = post(br#"{"jsonrpc":"2.0","id":5}"#);
        assert_eq!(st, StatusCode::BAD_REQUEST);
        let b = b.unwrap();
        let s = String::from_utf8_lossy(&b);
        assert!(s.contains("-32600"), "{s}");
    }

    #[test]
    fn methodless_notification_is_discarded() {
        let (st, b) = post(br#"{"jsonrpc":"2.0","foo":1}"#);
        assert_eq!(st, StatusCode::ACCEPTED);
        assert!(b.is_none());
    }

    #[test]
    fn notifications_with_id_are_still_discarded() {
        let (st, b) = post(br#"{"jsonrpc":"2.0","id":6,"method":"notifications/cancelled"}"#);
        assert_eq!(st, StatusCode::ACCEPTED);
        assert!(b.is_none());
    }

    #[test]
    fn null_id_is_treated_as_notification() {
        let (st, b) = post(br#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#);
        assert_eq!(st, StatusCode::ACCEPTED);
        assert!(b.is_none());
    }

    #[test]
    fn unknown_method_without_id_is_discarded() {
        let (st, b) = post(br#"{"jsonrpc":"2.0","method":"resources/list"}"#);
        assert_eq!(st, StatusCode::ACCEPTED);
        assert!(b.is_none());
    }

    #[test]
    fn initialize_without_params_uses_default_version() {
        let (st, b) = post(br#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#);
        assert_eq!(st, StatusCode::OK);
        let b = b.unwrap();
        let s = String::from_utf8_lossy(&b);
        assert!(s.contains(r#""protocolVersion":"2025-06-18""#), "{s}");
    }

    #[test]
    fn tools_call_without_params_is_invalid_params() {
        let (st, b) = post(br#"{"jsonrpc":"2.0","id":14,"method":"tools/call"}"#);
        assert_eq!(st, StatusCode::OK);
        let b = b.unwrap();
        let s = String::from_utf8_lossy(&b);
        assert!(s.contains("-32602"), "{s}");
    }

    #[test]
    fn tools_call_non_string_name_is_invalid_params() {
        let (st, b) =
            post(br#"{"jsonrpc":"2.0","id":15,"method":"tools/call","params":{"name":42}}"#);
        assert_eq!(st, StatusCode::OK);
        let b = b.unwrap();
        let s = String::from_utf8_lossy(&b);
        assert!(s.contains("-32602"), "{s}");
    }

    #[test]
    fn tools_call_echo_non_string_text_is_invalid_params() {
        let (st, b) = post(br#"{"jsonrpc":"2.0","id":16,"method":"tools/call","params":{"name":"echo","arguments":{"text":123}}}"#);
        assert_eq!(st, StatusCode::OK);
        let b = b.unwrap();
        let s = String::from_utf8_lossy(&b);
        assert!(s.contains("-32602"), "{s}");
    }

    #[test]
    fn tools_call_echo_missing_arguments_is_invalid_params() {
        let (st, b) =
            post(br#"{"jsonrpc":"2.0","id":17,"method":"tools/call","params":{"name":"echo"}}"#);
        assert_eq!(st, StatusCode::OK);
        let b = b.unwrap();
        let s = String::from_utf8_lossy(&b);
        assert!(s.contains("-32602"), "{s}");
    }

    #[test]
    fn tools_call_string_id_is_spliced_quoted() {
        let (st, b) = post(
            br#"{"jsonrpc":"2.0","id":"x-1","method":"tools/call","params":{"name":"answer"}}"#,
        );
        assert_eq!(st, StatusCode::OK);
        let b = b.unwrap();
        let s = String::from_utf8_lossy(&b);
        assert!(s.contains(r#""id":"x-1""#), "{s}");
        assert!(s.contains(r#""text":"42""#), "{s}");
    }

    #[test]
    fn tools_call_echo_empty_string() {
        let (st, b) = post(br#"{"jsonrpc":"2.0","id":18,"method":"tools/call","params":{"name":"echo","arguments":{"text":""}}}"#);
        assert_eq!(st, StatusCode::OK);
        let b = b.unwrap();
        let s = String::from_utf8_lossy(&b);
        assert!(s.contains(r#""text":"""#), "{s}");
        assert!(s.contains(r#""isError":false"#), "{s}");
    }

    #[test]
    fn whitespace_around_tokens_is_accepted() {
        let (st, b) = post(
            b"{\n  \"jsonrpc\" : \"2.0\" ,\n  \"id\" : 20 ,\n  \"method\" : \"tools/call\" ,\n  \"params\" : { \"name\" : \"echo\" , \"arguments\" : { \"text\" : \"spaced\" } }\n}",
        );
        assert_eq!(st, StatusCode::OK);
        let b = b.unwrap();
        let s = String::from_utf8_lossy(&b);
        assert!(s.contains(r#""id":20"#), "{s}");
        assert!(s.contains(r#""text":"spaced""#), "{s}");
    }

    #[test]
    fn unknown_method_error() {
        let (st, b) = post(br#"{"jsonrpc":"2.0","id":4,"method":"resources/list"}"#);
        assert_eq!(st, StatusCode::OK);
        let b = b.unwrap();
        let s = String::from_utf8_lossy(&b);
        assert!(s.contains("-32601"), "{s}");
    }

    #[test]
    fn parse_error() {
        let (st, b) = post(b"not json");
        assert_eq!(st, StatusCode::BAD_REQUEST);
        let b = b.unwrap();
        let s = String::from_utf8_lossy(&b);
        assert!(s.contains("-32700"), "{s}");
    }

    #[test]
    fn nested_tricky_params_do_not_confuse_parser() {
        // params holds nested "method"/"id" strings: the parser must use the top-level ones.
        let (st, b) = post(br#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"answer","arguments":{"method":"initialize","id":999,"protocolVersion":"x"}}}"#);
        assert_eq!(st, StatusCode::OK);
        let b = b.unwrap();
        let s = String::from_utf8_lossy(&b);
        assert!(s.contains(r#""id":7"#), "{s}");
        assert!(s.contains(r#""text":"42""#), "{s}");
    }
}
