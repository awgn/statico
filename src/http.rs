use hyper::header::{CONTENT_LENGTH, DATE};
use hyper::{Request, Response, Version};

/// Length of HTTP date value: "Sun, 06 Nov 1994 08:49:37 GMT"
const DATE_VALUE_LENGTH: usize = 29;

pub fn request_head_size<T>(req: &Request<T>) -> usize {
    let version_len = http_version_len(req.version());
    
    // URI can be in absolute-form (http://host/path) or origin-form (/path)
    // We need to use the full URI string representation
    let uri_len = req.uri().to_string().len();
    let uri_len = if uri_len == 0 { 1 } else { uri_len }; // at minimum "/"
    
    let mut size = req.method().as_str().len()                // (es. "GET")
                   + 1                                        // space
                   + uri_len                                  // URI (absolute or origin form)
                   + 1                                        // space
                   + version_len                              // version (e.g. "HTTP/1.1")
                   + 2; // \r\n

    size += request_headers_size(req);
    size += 2; // \r\n
    size
}

pub fn request_headers_size<T>(req: &Request<T>) -> usize {
    let mut size = 0;
    for (name, value) in req.headers() {
        size += name.as_str().len()                           // header name
                + 2                                           // ": "
                + value.as_bytes().len()                      // header value
                + 2; // \r\n
    }
    size
}

/// Calculate the estimated size of the HTTP response head.
///
/// This function accounts for headers that Hyper automatically adds:
/// - `date:` header (if not present) - always added by Hyper
/// - `content-length: 0` (if body is empty and not present)
pub fn response_head_size<T>(res: &Response<T>, body_len: usize) -> usize {
    let version_len = http_version_len(res.version());
    let status_code_len = 3;
    let mut size = version_len                                    // version
                   + 1                                            // space
                   + status_code_len                              // status code (e.g. "200")
                   + 1                                            // space
                   + res.status().canonical_reason()
                        .map_or(0, str::len)                   // reason phrase (es. "OK")
                   + 2; // \r\n

    size += response_headers_size(res);

    // Hyper automatically adds `date:` header if not present
    // Format: "date: Sun, 06 Nov 1994 08:49:37 GMT\r\n" = 4 + 2 + 29 + 2 = 37 bytes
    if !res.headers().contains_key(DATE) {
        size += 4                     // "date"
                + 2                   // ": "
                + DATE_VALUE_LENGTH   // e.g. "Sun, 06 Nov 1994 08:49:37 GMT"
                + 2;                  // \r\n
    }

    // Hyper automatically adds `content-length: 0` if body is empty and header not present
    // Only for responses that can have content-length (not 1xx, 204, 304)
    let status = res.status().as_u16();
    let can_have_content_length = !(status < 200 || status == 204 || status == 304);

    if can_have_content_length && !res.headers().contains_key(CONTENT_LENGTH) {
        if body_len == 0 {
            // "content-length: 0\r\n" = 14 + 2 + 1 + 2 = 19 bytes
            size += 14                // "content-length"
                    + 2               // ": "
                    + 1               // "0"
                    + 2;              // \r\n
        } else {
            // Hyper will add content-length with the actual body length
            // "content-length: <len>\r\n"
            let len_digits = if body_len == 0 { 1 } else { (body_len as f64).log10().floor() as usize + 1 };
            size += 14                // "content-length"
                    + 2               // ": "
                    + len_digits      // digits of body length
                    + 2;              // \r\n
        }
    }

    size += 2; // \r\n (end of headers)
    size
}

pub fn response_headers_size<T>(res: &Response<T>) -> usize {
    let mut size = 0;
    for (name, value) in res.headers() {
        size += name.as_str().len()
                + 2 // ": "
                + value.as_bytes().len()
                + 2; // \r\n
    }
    size
}

#[inline]
pub fn http_version_len(version: Version) -> usize {
    match version {
        Version::HTTP_11 | Version::HTTP_10 | Version::HTTP_09 => 8, // "HTTP/1.1"
        Version::HTTP_2 | Version::HTTP_3 => 6,                      // "HTTP/2"
        _ => format!("{version:?}").len(),                           // unreachable, but just in case
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::StatusCode;

    #[test]
    fn test_response_head_size_empty_body() {
        // Simulates: HTTP/1.1 200 OK\r\n + date header + content-length: 0 + \r\n
        let res = Response::builder()
            .status(StatusCode::OK)
            .body(())
            .unwrap();

        let size = response_head_size(&res, 0);

        // HTTP/1.1 200 OK\r\n = 8 + 1 + 3 + 1 + 2 + 2 = 17
        // date: <29 bytes>\r\n = 4 + 2 + 29 + 2 = 37
        // content-length: 0\r\n = 14 + 2 + 1 + 2 = 19
        // \r\n = 2
        // Total = 17 + 37 + 19 + 2 = 75
        assert_eq!(size, 75);
    }

    #[test]
    fn test_response_head_size_with_body() {
        let res = Response::builder()
            .status(StatusCode::OK)
            .body(())
            .unwrap();

        let size = response_head_size(&res, 1234);

        // HTTP/1.1 200 OK\r\n = 17
        // date: <29 bytes>\r\n = 37
        // content-length: 1234\r\n = 14 + 2 + 4 + 2 = 22
        // \r\n = 2
        // Total = 17 + 37 + 22 + 2 = 78
        assert_eq!(size, 78);
    }

    #[test]
    fn test_response_head_size_with_existing_headers() {
        let res = Response::builder()
            .status(StatusCode::OK)
            .header(DATE, "Wed, 14 Jan 2026 08:11:16 GMT")
            .header(CONTENT_LENGTH, "0")
            .body(())
            .unwrap();

        let size = response_head_size(&res, 0);

        // HTTP/1.1 200 OK\r\n = 17
        // date: Wed, 14 Jan 2026 08:11:16 GMT\r\n = 4 + 2 + 29 + 2 = 37
        // content-length: 0\r\n = 14 + 2 + 1 + 2 = 19
        // \r\n = 2
        // Total = 17 + 37 + 19 + 2 = 75
        assert_eq!(size, 75);
    }

    #[test]
    fn test_response_head_size_no_content() {
        // 204 No Content should not have content-length added
        let res = Response::builder()
            .status(StatusCode::NO_CONTENT)
            .body(())
            .unwrap();

        let size = response_head_size(&res, 0);

        // HTTP/1.1 204 No Content\r\n = 8 + 1 + 3 + 1 + 10 + 2 = 25
        // date: <29 bytes>\r\n = 37
        // \r\n = 2
        // Total = 25 + 37 + 2 = 64
        assert_eq!(size, 64);
    }

    #[test]
    fn test_request_head_size_absolute_form() {
        // Absolute-form URI (used by proxies): GET http://127.0.0.1:8080/ HTTP/1.1
        let req = Request::builder()
            .method("GET")
            .uri("http://127.0.0.1:8080/")
            .header("host", "127.0.0.1:8080")
            .body(())
            .unwrap();

        let size = request_head_size(&req);

        // GET http://127.0.0.1:8080/ HTTP/1.1\r\n = 3 + 1 + 22 + 1 + 8 + 2 = 37
        // host: 127.0.0.1:8080\r\n = 4 + 2 + 14 + 2 = 22
        // \r\n = 2
        // Total = 37 + 22 + 2 = 61
        assert_eq!(size, 61);
    }

    #[test]
    fn test_request_head_size_origin_form() {
        // Origin-form URI (normal requests): GET / HTTP/1.1
        let req = Request::builder()
            .method("GET")
            .uri("/")
            .header("host", "127.0.0.1:8080")
            .body(())
            .unwrap();

        let size = request_head_size(&req);

        // GET / HTTP/1.1\r\n = 3 + 1 + 1 + 1 + 8 + 2 = 16
        // host: 127.0.0.1:8080\r\n = 4 + 2 + 14 + 2 = 22
        // \r\n = 2
        // Total = 16 + 22 + 2 = 40
        assert_eq!(size, 40);
    }
}