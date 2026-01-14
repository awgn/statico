use hyper::{Request, Response};
use owo_colors::OwoColorize;
use std::fmt::{self, Write};

/// Wrapper for pretty-printing a Request.
///
pub struct PrettyRequest<'a, B>(pub (&'a Request<B>, u8));

/// Wrapper for pretty-printing a Response.
///
pub struct PrettyResponse<'a, B>(pub (&'a Response<B>, u8));

/// Trait to check if a body is empty and get bytes
pub trait BodyBytes {
    fn is_empty(&self) -> bool;
    fn as_bytes(&self) -> &[u8];
}

impl BodyBytes for &str {
    fn is_empty(&self) -> bool {
        (*self).is_empty()
    }
    fn as_bytes(&self) -> &[u8] {
        (*self).as_bytes()
    }
}

impl BodyBytes for String {
    fn is_empty(&self) -> bool {
        self.is_empty()
    }
    fn as_bytes(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl BodyBytes for &[u8] {
    fn is_empty(&self) -> bool {
        (*self).is_empty()
    }
    fn as_bytes(&self) -> &[u8] {
        self
    }
}

impl BodyBytes for Vec<u8> {
    fn is_empty(&self) -> bool {
        self.is_empty()
    }
    fn as_bytes(&self) -> &[u8] {
        self
    }
}

impl BodyBytes for bytes::Bytes {
    fn is_empty(&self) -> bool {
        self.is_empty()
    }
    fn as_bytes(&self) -> &[u8] {
        self
    }
}

impl<'a, B> fmt::Display for PrettyRequest<'a, B>
where
    B: BodyBytes,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let req = self.0 .0;
        let verb = self.0 .1;

        // Request line
        writeln!(f, "{} {} {:?}", req.method().bold().blue(), req.uri(), req.version())?;

        // Headers
        if verb > 1 {
            for (name, value) in req.headers() {
                writeln!(f, "{}: {}", name, value.to_str().unwrap_or("<binary>"))?;
            }
        }

        // Body (only in verbose mode, and only if not empty)
        if verb > 2 && !req.body().is_empty() {
            writeln!(f)?;
            match verb {
                3 =>  format_body(req.body().as_bytes(), f)?,
                _ =>  format_body_hexdump(req.body().as_bytes(), f)?,
            }
        }

        Ok(())
    }
}

impl<'a, B> fmt::Display for PrettyResponse<'a, B>
where
    B: BodyBytes,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let res = self.0 .0;
        let verb = self.0 .1;

        // Status line
        writeln!(f, "{:?} {}", res.version(), res.status().bold())?;

        // Headers
        if verb > 1 {
            for (name, value) in res.headers() {
                writeln!(f, "{}: {}", name, value.to_str().unwrap_or("<binary>"))?;
            }
        }

        // Body (only in verbose mode, and only if not empty)
        if verb > 2 && !res.body().is_empty() {
            writeln!(f)?;
            match verb {
                3 =>  format_body(res.body().as_bytes(), f)?,
                _ =>  format_body_hexdump(res.body().as_bytes(), f)?,
            }
        }

        Ok(())
    }
}

/// Extension trait for convenient pretty-printing of Request and Response.
pub trait PrettyPrint {
    type Wrapper<'a>
    where
        Self: 'a;

    fn pretty(&self, verbose: u8) -> Self::Wrapper<'_>;
}

impl<B> PrettyPrint for Request<B> {
    type Wrapper<'a>
        = PrettyRequest<'a, B>
    where
        B: 'a;

    fn pretty(&self, verbose: u8) -> PrettyRequest<'_, B> {
        PrettyRequest((self, verbose))
    }
}

impl<B> PrettyPrint for Response<B> {
    type Wrapper<'a>
        = PrettyResponse<'a, B>
    where
        B: 'a;

    fn pretty(&self, verbose: u8) -> PrettyResponse<'_, B> {
        PrettyResponse((self, verbose))
    }
}


/// Format body bytes: printable chars as-is, non-printable as reversed hex
fn format_body(body: &[u8], f: &mut fmt::Formatter<'_>) -> fmt::Result {
    // Lookup table for hex conversion
    const HEX_CHARS: &[u8] = b"0123456789abcdef";

    for &byte in body {
        if byte.is_ascii_graphic() || byte.is_ascii_whitespace() {
            f.write_char(byte as char)?;
        } else {
            let hex_buf = [
                HEX_CHARS[(byte >> 4) as usize],
                HEX_CHARS[(byte & 0x0f) as usize],
            ];

            let hex_str = std::str::from_utf8(&hex_buf).unwrap();
            write!(f, "{}", hex_str.reversed())?;
        }
    }
    writeln!(f)?;
    Ok(())
}

fn format_body_hexdump(body: &[u8], f: &mut fmt::Formatter<'_>) -> fmt::Result {
    // Process data in chunks of 16 bytes (standard line width)
    for (chunk_idx, chunk) in body.chunks(16).enumerate() {
        // 1. Print Offset (e.g., 00000010)
        write!(f, "{:08x}  ", chunk_idx * 16)?;

        // 2. Print Hex section
        for (i, &byte) in chunk.iter().enumerate() {
            // Add extra space after the 8th byte for readability
            if i == 8 {
                write!(f, " ")?;
            }
            write!(f, "{:02x} ", byte)?;
        }

        if chunk.len() < 16 {
            let missing = 16 - chunk.len();
            for _ in 0..missing {
                write!(f, "   ")?;
            }
            if chunk.len() <= 8 {
                write!(f, " ")?;
            }
        }

        // 3. Print ASCII section
        write!(f, " |")?;
        for &byte in chunk {
            if byte.is_ascii_graphic() || byte == b' ' {
                f.write_char(byte as char)?;
            } else {
                f.write_char('.')?;
            }
        }
        writeln!(f, "|")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::{Method, Request, Response, StatusCode};

    #[test]
    fn test_request_normal_format() {
        let req = Request::builder()
            .method(Method::GET)
            .uri("/test")
            .header("Host", "localhost")
            .header("Content-Type", "application/json")
            .body("test body")
            .unwrap();

        let output = format!("{}", req.pretty(2));

        assert!(output.contains("GET") && output.contains("/test"));
        assert!(output.contains("host: localhost"));
        assert!(output.contains("content-type: application/json"));
        // Body should NOT be present in normal format (verb=1)
        assert!(!output.contains("test body"));
    }

    #[test]
    fn test_request_verbose_format() {
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/data")
            .header("Content-Type", "text/plain")
            .body("request body content")
            .unwrap();

        let output = format!("{}", req.pretty(3));

        assert!(output.contains("POST") && output.contains("/api/data"));
        assert!(output.contains("content-type: text/plain"));
        // Body SHOULD be present in alternate format
        assert!(output.contains("request body content"));
    }

    #[test]
    fn test_response_normal_format() {
        let res = Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "text/html")
            .body("response body")
            .unwrap();

        let output = format!("{}", res.pretty(2));

        assert!(output.contains("200 OK"));
        assert!(output.contains("content-type: text/html"));
        // Body should NOT be present (verb=1)
        assert!(!output.contains("response body"));
    }

    #[test]
    fn test_response_verbose_format() {
        let res = Response::builder()
            .status(StatusCode::NOT_FOUND)
            .header("X-Custom", "value")
            .body("not found body")
            .unwrap();

        let output = format!("{:#}", res.pretty(3));

        assert!(output.contains("404 Not Found"));
        assert!(output.contains("x-custom: value"));
        // Body SHOULD be present
        assert!(output.contains("not found body"));
    }

    #[test]
    fn test_request_empty_body_not_printed() {
        let req = Request::builder()
            .method(Method::GET)
            .uri("/test")
            .body("")
            .unwrap();

        let output = format!("{:#}",&req.pretty(3));

        // Should contain request line and no extra blank line for body
        assert!(output.contains("GET") && output.contains("/test"));
        // The output should end with the last header line, no body section
        let lines: Vec<&str> = output.trim().lines().collect();
        // Last line should not be empty (no body printed)
        assert!(!lines.last().unwrap().is_empty());
    }

    #[test]
    fn test_response_empty_body_not_printed() {
        let res = Response::builder()
            .status(StatusCode::NO_CONTENT)
            .body("")
            .unwrap();

        let output = format!("{:#}", res.pretty(3));

        assert!(output.contains("204 No Content"));
        // No body section should be present
        let lines: Vec<&str> = output.trim().lines().collect();
        assert!(!lines.last().unwrap().is_empty());
    }

    #[test]
    fn test_request_body_hex_non_printable() {
        let req = Request::builder()
            .method(Method::POST)
            .uri("/test")
            .body("A\x00B")
            .unwrap();
        let output = format!("{}", req.pretty(4));
        assert!(output.contains("A") && output.contains("00") && output.contains("B"));
    }

    #[test]
    fn test_response_body_hex_non_printable() {
        let res = Response::builder()
            .status(StatusCode::OK)
            .body("X\x01Y")
            .unwrap();
        let output = format!("{}", res.pretty(4));
        assert!(output.contains("X") && output.contains("01") && output.contains("Y"));
    }
}
