//! A very small HTTP/1.1 server, enough for one local control panel.
//!
//! Hand written rather than pulled from a framework, for the same reason the
//! CLI parser and the invite encoder are: this serves eight routes to one
//! browser on the loopback interface, and a framework would add more binary to
//! the installer than the whole panel weighs.
//!
//! It is deliberately not a general-purpose server. It reads a bounded request,
//! answers it, and closes — no keep-alive, no chunked bodies, no upgrades.

use anyhow::{Context, bail};
use std::collections::HashMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Largest request accepted. The panel's biggest POST is a device id, so this
/// is generous by three orders of magnitude and exists only so a stray
/// connection cannot make the daemon allocate without bound.
const MAX_REQUEST_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub method: String,
    /// Path with the query string removed.
    pub path: String,
    pub query: HashMap<String, String>,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }

    /// The panel token, from either the query string or a header.
    ///
    /// The query string is what a freshly opened browser tab carries; the
    /// header is what the page's own fetch calls use, so the token never has to
    /// be re-appended to every URL.
    pub fn token(&self) -> Option<&str> {
        self.query
            .get("token")
            .map(String::as_str)
            .or_else(|| self.header("x-waypad-token"))
    }
}

/// Reads one request off a socket.
pub async fn read_request<S>(stream: &mut S) -> anyhow::Result<Request>
where
    S: AsyncReadExt + Unpin,
{
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    // Headers first: read until the blank line that ends them, then read
    // exactly as many more bytes as Content-Length asks for.
    let header_end = loop {
        if let Some(at) = find_header_end(&buffer) {
            break at;
        }
        if buffer.len() > MAX_REQUEST_BYTES {
            bail!("request headers exceeded {MAX_REQUEST_BYTES} bytes");
        }
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            bail!("connection closed before the request was complete");
        }
        buffer.extend_from_slice(&chunk[..read]);
    };

    let head = std::str::from_utf8(&buffer[..header_end])
        .context("request headers are not valid UTF-8")?;
    let mut lines = head.split("\r\n");
    let request_line = lines.next().context("empty request")?;
    let mut parts = request_line.split(' ');
    let method = parts.next().context("request has no method")?.to_string();
    let target = parts.next().context("request has no target")?;

    let mut headers = HashMap::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }

    let (path, query) = split_target(target);
    let content_length: usize = headers
        .get("content-length")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    if content_length > MAX_REQUEST_BYTES {
        bail!("request body of {content_length} bytes is too large");
    }

    let mut body = buffer[header_end + 4..].to_vec();
    while body.len() < content_length {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            bail!(
                "connection closed with {} of {content_length} body bytes",
                body.len()
            );
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(content_length);

    Ok(Request {
        method,
        path,
        query,
        headers,
        body,
    })
}

/// A response, ready to write.
pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

impl Response {
    pub fn html(body: impl Into<Vec<u8>>) -> Self {
        Self {
            status: 200,
            content_type: "text/html; charset=utf-8",
            body: body.into(),
        }
    }

    pub fn json(body: impl Into<Vec<u8>>) -> Self {
        Self {
            status: 200,
            content_type: "application/json",
            body: body.into(),
        }
    }

    pub fn svg(body: impl Into<Vec<u8>>) -> Self {
        Self {
            status: 200,
            content_type: "image/svg+xml",
            body: body.into(),
        }
    }

    /// An error the page can display. Always JSON, so the panel's fetch code
    /// has one shape to handle rather than two.
    pub fn error(status: u16, message: impl AsRef<str>) -> Self {
        let body = serde_json::json!({ "error": message.as_ref() }).to_string();
        Self {
            status,
            content_type: "application/json",
            body: body.into_bytes(),
        }
    }
}

pub async fn write_response<S>(stream: &mut S, response: Response) -> anyhow::Result<()>
where
    S: AsyncWriteExt + Unpin,
{
    let reason = match response.status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Internal Server Error",
    };
    let mut head = format!(
        "HTTP/1.1 {} {reason}\r\n\
         Content-Type: {}\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n",
        response.status,
        response.content_type,
        response.body.len()
    );
    // The panel is not a website and must never be treated as one by anything
    // else on the machine: no framing, no sniffing, no cross-origin reads.
    head.push_str("X-Content-Type-Options: nosniff\r\n");
    head.push_str("X-Frame-Options: DENY\r\n");
    head.push_str("Referrer-Policy: no-referrer\r\n");
    head.push_str("Connection: close\r\n\r\n");

    stream.write_all(head.as_bytes()).await?;
    stream.write_all(&response.body).await?;
    stream.flush().await?;
    Ok(())
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

fn split_target(target: &str) -> (String, HashMap<String, String>) {
    match target.split_once('?') {
        Some((path, query)) => (path.to_string(), parse_query(query)),
        None => (target.to_string(), HashMap::new()),
    }
}

pub fn parse_query(query: &str) -> HashMap<String, String> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .filter_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            Some((url_decode(key), url_decode(value)))
        })
        .collect()
}

/// Percent-decoding, with `+` meaning a space as form encoding requires.
pub fn url_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    // A stray percent is kept rather than dropped: it is more
                    // likely a literal than a truncated escape.
                    Err(_) => {
                        out.push(b'%');
                        index += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Compares two secrets without leaking where they first differ.
///
/// The panel token guards every route, and `==` on strings returns as soon as a
/// byte differs, which is enough to recover a token a byte at a time from a
/// local process that can time the answer.
pub fn secret_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn parse(raw: &str) -> anyhow::Result<Request> {
        let mut cursor = std::io::Cursor::new(raw.as_bytes().to_vec());
        read_request(&mut cursor).await
    }

    #[tokio::test]
    async fn parses_a_get_with_a_query_string() {
        let request = parse("GET /api/status?token=abc123 HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .await
            .unwrap();
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, "/api/status");
        assert_eq!(request.token(), Some("abc123"));
        assert_eq!(request.header("host"), Some("127.0.0.1"));
    }

    #[tokio::test]
    async fn header_lookup_ignores_case() {
        // Browsers send whatever casing they like, and the panel's own fetch
        // calls send a custom header that must be found either way.
        let request = parse("GET / HTTP/1.1\r\nX-Waypad-Token: secret\r\n\r\n")
            .await
            .unwrap();
        assert_eq!(request.token(), Some("secret"));
        assert_eq!(request.header("X-WAYPAD-TOKEN"), Some("secret"));
    }

    #[tokio::test]
    async fn reads_a_body_of_exactly_content_length() {
        // The body must not absorb bytes beyond Content-Length, or a pipelined
        // request would be swallowed into this one's body.
        let request =
            parse("POST /api/revoke HTTP/1.1\r\nContent-Length: 4\r\n\r\n{\"a\"}trailing")
                .await
                .unwrap();
        assert_eq!(request.body, b"{\"a\"".to_vec());
    }

    #[tokio::test]
    async fn a_missing_body_is_empty_rather_than_an_error() {
        let request = parse("POST /api/pair-code HTTP/1.1\r\n\r\n").await.unwrap();
        assert!(request.body.is_empty());
    }

    #[tokio::test]
    async fn rejects_a_body_larger_than_the_cap() {
        let raw = format!(
            "POST / HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            MAX_REQUEST_BYTES + 1
        );
        assert!(parse(&raw).await.is_err());
    }

    #[tokio::test]
    async fn rejects_a_request_that_ends_mid_body() {
        let err = parse("POST / HTTP/1.1\r\nContent-Length: 100\r\n\r\nshort")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("body bytes"), "{err}");
    }

    #[test]
    fn decodes_percent_escapes_and_form_spaces() {
        assert_eq!(url_decode("hello+world"), "hello world");
        assert_eq!(url_decode("aa%3Abb"), "aa:bb");
        assert_eq!(url_decode("caff%C3%A8"), "caffè");
        // A stray percent is a literal, not a reason to lose the rest.
        assert_eq!(url_decode("100%"), "100%");
    }

    #[test]
    fn query_parsing_survives_junk() {
        let query = parse_query("a=1&&b=2&novalue&c=");
        assert_eq!(query.get("a").map(String::as_str), Some("1"));
        assert_eq!(query.get("b").map(String::as_str), Some("2"));
        assert_eq!(query.get("c").map(String::as_str), Some(""));
        assert!(!query.contains_key("novalue"));
    }

    #[test]
    fn secret_comparison_is_length_safe_and_correct() {
        assert!(secret_eq("token", "token"));
        assert!(!secret_eq("token", "tokes"));
        assert!(!secret_eq("token", "token "));
        assert!(!secret_eq("", "x"));
        assert!(secret_eq("", ""));
    }

    #[tokio::test]
    async fn responses_carry_the_headers_that_keep_the_panel_out_of_a_web_page() {
        let mut out = Vec::new();
        write_response(&mut out, Response::json("{}"))
            .await
            .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(text.contains("Content-Type: application/json"));
        assert!(text.contains("Content-Length: 2"));
        assert!(text.contains("X-Frame-Options: DENY"));
        assert!(text.contains("X-Content-Type-Options: nosniff"));
        assert!(text.contains("Cache-Control: no-store"));
        assert!(text.ends_with("\r\n\r\n{}"));
    }

    #[tokio::test]
    async fn an_error_response_is_json_like_every_other_answer() {
        let mut out = Vec::new();
        write_response(&mut out, Response::error(403, "nope"))
            .await
            .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("HTTP/1.1 403 Forbidden"));
        assert!(text.contains(r#"{"error":"nope"}"#));
    }
}
