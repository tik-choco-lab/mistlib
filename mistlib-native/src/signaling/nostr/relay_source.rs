use super::NostrSignaler;
use crate::error::{MistError, Result};
use mistlib_core::signaling::nostr::{normalize_relays, parse_relay_list_json};
use tokio::net::TcpStream;
use tokio::time::{timeout, Duration};
use tokio_native_tls::{native_tls, TlsConnector};
use url::Url;

mod http;

const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

impl NostrSignaler {
    pub(super) async fn resolve_relays(&self) -> Result<Vec<String>> {
        let mut relays = self.relays.clone();
        if let Some(url) = &self.relay_list_url {
            match fetch_relay_list(url).await {
                Ok(json) => match parse_relay_list_json(&json) {
                    Ok(list) => relays.extend(list),
                    Err(err) if relays.is_empty() => return Err(err.into()),
                    Err(err) => tracing::warn!("NostrSignaler: relay list parse failed: {}", err),
                },
                Err(err) if relays.is_empty() => return Err(err),
                Err(err) => tracing::warn!("NostrSignaler: relay list fetch failed: {}", err),
            }
        }
        normalize_relays(relays).map_err(Into::into)
    }
}

async fn fetch_relay_list(url: &str) -> Result<String> {
    timeout(FETCH_TIMEOUT, fetch_relay_list_inner(url))
        .await
        .map_err(|_| MistError::Network("Nostr relay list fetch timed out".to_string()))?
}

async fn fetch_relay_list_inner(url: &str) -> Result<String> {
    let url = Url::parse(url)
        .map_err(|err| MistError::Config(format!("invalid Nostr relay list URL: {err}")))?;
    let host = url
        .host_str()
        .ok_or_else(|| MistError::Config("Nostr relay list URL is missing host".to_string()))?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| MistError::Config("unsupported Nostr relay list URL scheme".to_string()))?;
    let stream = TcpStream::connect((host, port)).await?;

    match url.scheme() {
        "http" => http::get(stream, &url).await,
        "https" => {
            let connector = native_tls::TlsConnector::new()
                .map(TlsConnector::from)
                .map_err(|err| MistError::Network(format!("TLS setup failed: {err}")))?;
            let stream = connector
                .connect(host, stream)
                .await
                .map_err(|err| MistError::Network(format!("TLS connect failed: {err}")))?;
            http::get(stream, &url).await
        }
        scheme => Err(MistError::Config(format!(
            "unsupported Nostr relay list URL scheme: {scheme}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::fetch_relay_list;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn fetches_relay_list_from_local_http_fixture() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut byte = [0u8; 1];
                socket.read_exact(&mut byte).await.unwrap();
                request.push(byte[0]);
                if request.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n[\"ws://127.0.0.1:7778\"]",
                )
                .await
                .unwrap();
            socket.shutdown().await.unwrap();
        });

        let json = fetch_relay_list(&format!("http://{addr}/relays.json"))
            .await
            .unwrap();

        server.await.unwrap();
        assert_eq!(json, r#"["ws://127.0.0.1:7778"]"#);
    }
}
