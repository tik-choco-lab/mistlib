//! Minimal hand-rolled HTTP/1.1 GET server for `.m3u8`/`.ts` files, so the
//! only new dependency this whole `output/` module needs is what's already
//! in the workspace (`tokio`). Not a general-purpose HTTP server: no
//! keep-alive, no request bodies, GET only, closes the connection after
//! every response. That's enough for HLS polling clients (VRChat's AVPro
//! Video included) and keeps this dependency-free per the test-harness goal.

use std::net::SocketAddr;
use std::sync::{Arc, RwLock};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use super::hls::Segmenter;

/// Serves `GET /stream.m3u8` and `GET /segment{N}.ts` from `segmenter` until
/// the process exits or the listener errors. Runs forever — spawn this as a
/// background task.
pub async fn serve(addr: SocketAddr, segmenter: Arc<RwLock<Segmenter>>) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    loop {
        let (stream, _) = listener.accept().await?;
        let segmenter = segmenter.clone();
        tokio::spawn(async move {
            if let Err(err) = handle_connection(stream, segmenter).await {
                tracing::debug!("hls connection error: {err}");
            }
        });
    }
}

async fn handle_connection(
    stream: TcpStream,
    segmenter: Arc<RwLock<Segmenter>>,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream);

    let mut request_line = String::new();
    if reader.read_line(&mut request_line).await? == 0 {
        return Ok(()); // client closed without sending anything
    }
    // Drain and discard headers up to the blank line; we don't need them.
    loop {
        let mut header_line = String::new();
        let n = reader.read_line(&mut header_line).await?;
        if n == 0 || header_line.trim().is_empty() {
            break;
        }
    }

    let mut stream = reader.into_inner();
    let path = parse_get_path(&request_line);

    match path.as_deref() {
        Some("/stream.m3u8") | Some("/") => {
            let body = segmenter.read().unwrap().playlist();
            write_response(
                &mut stream,
                200,
                "OK",
                "application/vnd.apple.mpegurl",
                body.as_bytes(),
            )
            .await
        }
        Some(path) => {
            if let Some(index) = parse_segment_path(path) {
                let body = segmenter.read().unwrap().segment(index).map(<[u8]>::to_vec);
                match body {
                    Some(data) => write_response(&mut stream, 200, "OK", "video/mp2t", &data).await,
                    None => {
                        write_response(&mut stream, 404, "Not Found", "text/plain", b"not found")
                            .await
                    }
                }
            } else {
                write_response(&mut stream, 404, "Not Found", "text/plain", b"not found").await
            }
        }
        None => {
            write_response(
                &mut stream,
                400,
                "Bad Request",
                "text/plain",
                b"bad request",
            )
            .await
        }
    }
}

fn parse_get_path(request_line: &str) -> Option<String> {
    let mut parts = request_line.split_whitespace();
    if parts.next()? != "GET" {
        return None;
    }
    Some(parts.next()?.to_string())
}

fn parse_segment_path(path: &str) -> Option<u64> {
    path.strip_prefix('/')?
        .strip_prefix("segment")?
        .strip_suffix(".ts")?
        .parse()
        .ok()
}

async fn write_response(
    stream: &mut TcpStream,
    status: u16,
    status_text: &str,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let header = format!(
        "HTTP/1.1 {status} {status_text}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-cache\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_get_path_accepts_get_only() {
        assert_eq!(
            parse_get_path("GET /stream.m3u8 HTTP/1.1\r\n"),
            Some("/stream.m3u8".to_string())
        );
        assert_eq!(parse_get_path("POST /stream.m3u8 HTTP/1.1\r\n"), None);
        assert_eq!(parse_get_path(""), None);
    }

    #[test]
    fn parse_segment_path_extracts_index() {
        assert_eq!(parse_segment_path("/segment0.ts"), Some(0));
        assert_eq!(parse_segment_path("/segment42.ts"), Some(42));
        assert_eq!(parse_segment_path("/stream.m3u8"), None);
        assert_eq!(parse_segment_path("/segmentX.ts"), None);
    }
}
