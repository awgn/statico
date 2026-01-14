use hyper::{Request, Response};
use owo_colors::OwoColorize;
use std::fmt;

/// Wrapper for pretty-printing a Request.
///
pub struct PrettyRequest<'a, B>(pub (&'a Request<B>, u8));

/// Wrapper for pretty-printing a Response.
///
pub struct PrettyResponse<'a, B>(pub (&'a Response<B>, u8));

/// Trait to check if a body is empty
pub trait IsEmpty {
    fn is_empty(&self) -> bool;
}

impl IsEmpty for &str {
    fn is_empty(&self) -> bool {
        (*self).is_empty()
    }
}

impl IsEmpty for String {
    fn is_empty(&self) -> bool {
        self.is_empty()
    }
}

impl IsEmpty for &[u8] {
    fn is_empty(&self) -> bool {
        (*self).is_empty()
    }
}

impl IsEmpty for Vec<u8> {
    fn is_empty(&self) -> bool {
        self.is_empty()
    }
}

impl IsEmpty for bytes::Bytes {
    fn is_empty(&self) -> bool {
        self.is_empty()
    }
}

impl<'a, B> fmt::Display for PrettyRequest<'a, B>
where
    B: fmt::Debug + IsEmpty,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let req = self.0 .0;
        let verb = self.0 .1;

        // Request line
        writeln!(f, "{} {} {:?}", req.method().bold().blue(), req.uri(), req.version())?;

        // Headers
        if verb > 0 {
            for (name, value) in req.headers() {
                writeln!(f, "{}: {}", name, value.to_str().unwrap_or("<binary>"))?;
            }
        }

        // Body (only in alternate/verbose mode, and only if not empty)
        if verb > 1 && !req.body().is_empty() {
            writeln!(f)?;
            writeln!(f, "{:?}", req.body())?;
        }

        Ok(())
    }
}

impl<'a, B> fmt::Display for PrettyResponse<'a, B>
where
    B: fmt::Debug + IsEmpty,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let res = self.0 .0;
        let verb = self.0 .1;

        // Status line
        writeln!(f, "{:?} {}", res.version(), res.status())?;

        // Headers
        if verb > 0 {
            for (name, value) in res.headers() {
                writeln!(f, "{}: {}", name, value.to_str().unwrap_or("<binary>"))?;
            }
        }

        // Body (only in alternate/verbose mode, and only if not empty)
        if verb > 1 && !res.body().is_empty() {
            writeln!(f)?;
            writeln!(f, "{:?}", res.body())?;
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

        let output = format!("{}", req.pretty(1));

        assert!(output.contains("GET /test"));
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

        let output = format!("{}", req.pretty(2));

        assert!(output.contains("POST /api/data"));
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

        let output = format!("{}", res.pretty(1));

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

        let output = format!("{:#}", res.pretty(2));

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

        let output = format!("{:#}",&req.pretty(2));

        // Should contain request line and no extra blank line for body
        assert!(output.contains("GET /test"));
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

        let output = format!("{:#}", res.pretty(2));

        assert!(output.contains("204 No Content"));
        // No body section should be present
        let lines: Vec<&str> = output.trim().lines().collect();
        assert!(!lines.last().unwrap().is_empty());
    }
}
