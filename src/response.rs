use crate::ServerConfig;
use anyhow::Result;
use http_body_util::Full;
use hyper::{header::CONTENT_LENGTH, Response};

/// Build a static HTTP response and encode it to bytes once at connection start.
/// This pre-encodes the response to avoid re-encoding on every request, significantly
/// improving performance for static responses.
#[allow(dead_code)]
pub fn build_response(config: &ServerConfig) -> Result<Vec<u8>> {
    use http_wire::WireEncode;

    // Build the HTTP response with configured status code
    let mut builder = Response::builder().status(config.status);

    // Add all configured headers
    for (k, v) in &config.headers {
        builder = builder.header(k, v);
    }

    // Add Content-Length header
    builder = builder.header(CONTENT_LENGTH, config.body.len());

    // Build the response and encode it to wire format (HTTP/1.1 bytes)
    let res = builder.body(Full::new(config.body.clone()))?;
    Ok(res.encode()?.to_vec())
}
