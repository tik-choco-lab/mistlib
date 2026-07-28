use crate::error::{MistError, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use url::{Host, Url};

const MAX_RELAY_LIST_BYTES: usize = 256 * 1024;

pub(super) async fn get<S>(mut stream: S, url: &Url) -> Result<String>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let request = request(url);
    stream.write_all(request.as_bytes()).await?;
    stream.flush().await?;

    let mut response = Vec::new();
    stream
        .take(MAX_RELAY_LIST_BYTES as u64 + 1)
        .read_to_end(&mut response)
        .await?;
    if response.len() > MAX_RELAY_LIST_BYTES {
        return Err(MistError::Network(
            "Nostr relay list response is too large".to_string(),
        ));
    }
    parse_response(&response)
}

fn request(url: &Url) -> String {
    format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nAccept: application/json\r\nConnection: close\r\n\r\n",
        request_target(url),
        host_header(url),
    )
}

fn parse_response(response: &[u8]) -> Result<String> {
    let header_end = response
        .windows(4)
        .position(|bytes| bytes == b"\r\n\r\n")
        .ok_or_else(|| MistError::Network("invalid HTTP response".to_string()))?;
    let headers = String::from_utf8_lossy(&response[..header_end]);
    let status = headers.lines().next().unwrap_or_default();
    if !status_code_is_success(status) {
        return Err(MistError::Network(format!(
            "Nostr relay list request failed: {status}"
        )));
    }
    let body = &response[header_end + 4..];
    let body = if is_chunked(&headers) {
        decode_chunked_body(body)?
    } else {
        body.to_vec()
    };
    String::from_utf8(body)
        .map_err(|err| MistError::Network(format!("relay list response is not UTF-8: {err}")))
}

fn is_chunked(headers: &str) -> bool {
    headers.lines().any(|line| {
        line.to_ascii_lowercase()
            .starts_with("transfer-encoding: chunked")
    })
}

fn decode_chunked_body(mut body: &[u8]) -> Result<Vec<u8>> {
    let mut decoded = Vec::new();
    loop {
        let line_end = body
            .windows(2)
            .position(|bytes| bytes == b"\r\n")
            .ok_or_else(|| MistError::Network("invalid chunked response".to_string()))?;
        let size_line = std::str::from_utf8(&body[..line_end])
            .map_err(|_| MistError::Network("invalid chunk size".to_string()))?;
        let size = usize::from_str_radix(size_line.split(';').next().unwrap_or_default(), 16)
            .map_err(|_| MistError::Network("invalid chunk size".to_string()))?;
        body = &body[line_end + 2..];
        if size == 0 {
            return Ok(decoded);
        }
        if body.len() < size + 2 || &body[size..size + 2] != b"\r\n" {
            return Err(MistError::Network("invalid chunked response".to_string()));
        }
        decoded.extend_from_slice(&body[..size]);
        body = &body[size + 2..];
    }
}

fn request_target(url: &Url) -> String {
    match url.query() {
        Some(query) => format!("{}?{}", url.path(), query),
        None => url.path().to_string(),
    }
}

fn host_header(url: &Url) -> String {
    let host = match url.host() {
        Some(Host::Ipv6(addr)) => format!("[{addr}]"),
        Some(host) => host.to_string(),
        None => String::new(),
    };
    match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host,
    }
}

fn status_code_is_success(status: &str) -> bool {
    status
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .is_some_and(|code| (200..300).contains(&code))
}

#[cfg(test)]
mod tests {
    use super::{host_header, parse_response, request};
    use url::Url;

    #[test]
    fn parses_plain_response_body() {
        let body = parse_response(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n[\"wss://relay\"]",
        )
        .unwrap();

        assert_eq!(body, "[\"wss://relay\"]");
    }

    #[test]
    fn parses_chunked_response_body() {
        let body = parse_response(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n",
        )
        .unwrap();

        assert_eq!(body, "hello");
    }

    #[test]
    fn request_uses_generic_headers() {
        let url = Url::parse("https://relay.example/list.json").unwrap();
        let request = request(&url);

        assert!(request.contains("Accept: application/json\r\n"));
        assert!(!request.contains("User-Agent:"));
    }

    #[test]
    fn ipv6_host_header_uses_brackets() {
        let url = Url::parse("http://[::1]:8080/relays.json").unwrap();

        assert_eq!(host_header(&url), "[::1]:8080");
    }
}
