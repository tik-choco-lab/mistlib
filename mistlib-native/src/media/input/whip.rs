//! Minimal WHIP (WebRTC-HTTP Ingest Protocol, RFC draft) publish endpoint —
//! `POST /whip` with an SDP offer, `201 Created` with an SDP answer. Ported
//! from mistlink/internal/sender/whip_server.go's `StartWHIPServer`.
//!
//! Hand-rolled HTTP/1.1 (same rationale as `output::server`: no new HTTP
//! dependency), but unlike `output::server` this endpoint creates a real
//! `RTCPeerConnection` per publisher and needs full SDP offer/answer +
//! ICE-gathering-complete handling, so it's meaningfully more than a static
//! GET server.
//!
//! Deviation: the Go original built its own `webrtc.API` per request (fresh
//! `MediaEngine`+`interceptor.Registry`). This port takes a pre-built
//! `Arc<webrtc::api::API>` from the caller instead — constructing one is a
//! few lines the caller almost certainly already has (e.g. from
//! `mistlib_native::transports::webrtc::WebRtcTransport`, which registers
//! H264/Opus codecs), and re-creating one per publish request is wasteful.
//! This also keeps mistlib-media from needing an opinion on ICE servers/
//! interceptor configuration.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use webrtc::api::API;
use webrtc::ice_transport::ice_gathering_state::RTCIceGatheringState;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::track::track_remote::TrackRemote;

/// Called for every remote track a WHIP publisher offers (e.g. to hand it to
/// `stream::StreamManager::add_obs_track`).
pub type OnTrack = Arc<dyn Fn(Arc<TrackRemote>, Arc<RTCPeerConnection>) + Send + Sync>;

/// Serves `POST /whip` on `addr` until the process exits or the listener
/// errors. Runs forever — spawn as a background task.
pub async fn serve(
    addr: SocketAddr,
    api: Arc<API>,
    ice_config: RTCConfiguration,
    on_track: OnTrack,
) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    loop {
        let (stream, _) = listener.accept().await?;
        let api = api.clone();
        let ice_config = ice_config.clone();
        let on_track = on_track.clone();
        tokio::spawn(async move {
            if let Err(err) = handle_connection(stream, api, ice_config, on_track).await {
                tracing::debug!("whip connection error: {err}");
            }
        });
    }
}

async fn handle_connection(
    stream: TcpStream,
    api: Arc<API>,
    ice_config: RTCConfiguration,
    on_track: OnTrack,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream);

    let mut request_line = String::new();
    if reader.read_line(&mut request_line).await? == 0 {
        return Ok(());
    }

    let mut content_length: usize = 0;
    loop {
        let mut header_line = String::new();
        let n = reader.read_line(&mut header_line).await?;
        if n == 0 || header_line.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = header_line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().unwrap_or(0);
            }
        }
    }

    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body).await?;
    }

    let mut stream = reader.into_inner();
    let (method, path) = parse_request_line(&request_line);

    match (method.as_deref(), path.as_deref()) {
        (Some("OPTIONS"), Some("/whip")) => write_no_content(&mut stream).await,
        (Some("POST"), Some("/whip")) => {
            match handle_publish(&body, api, ice_config, on_track).await {
                Ok(answer_sdp) => write_answer(&mut stream, &answer_sdp).await,
                Err(err) => write_error(&mut stream, 500, &err.to_string()).await,
            }
        }
        (Some(_), Some("/whip")) => write_error(&mut stream, 405, "method not allowed").await,
        _ => write_error(&mut stream, 404, "not found").await,
    }
}

async fn handle_publish(
    body: &[u8],
    api: Arc<API>,
    ice_config: RTCConfiguration,
    on_track: OnTrack,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let sdp = String::from_utf8_lossy(body).into_owned();
    let offer = RTCSessionDescription::offer(sdp)?;

    let pc = Arc::new(api.new_peer_connection(ice_config).await?);

    let pc_for_track = pc.clone();
    pc.on_track(Box::new(move |track, _receiver, _transceiver| {
        let on_track = on_track.clone();
        let pc = pc_for_track.clone();
        Box::pin(async move {
            on_track(track, pc);
        })
    }));

    pc.set_remote_description(offer).await?;
    let answer = pc.create_answer(None).await?;
    pc.set_local_description(answer).await?;

    wait_ice_gathering_complete(&pc).await;

    let local = pc
        .local_description()
        .await
        .ok_or("no local description after gathering")?;
    Ok(local.sdp)
}

async fn wait_ice_gathering_complete(pc: &Arc<RTCPeerConnection>) {
    if pc.ice_gathering_state() == RTCIceGatheringState::Complete {
        return;
    }
    let (tx, rx) = tokio::sync::oneshot::channel();
    let tx = Arc::new(std::sync::Mutex::new(Some(tx)));
    pc.on_ice_candidate(Box::new(move |candidate| {
        let tx = tx.clone();
        Box::pin(async move {
            if candidate.is_none() {
                if let Some(tx) = tx.lock().unwrap().take() {
                    let _ = tx.send(());
                }
            }
        })
    }));
    let _ = rx.await;
}

fn parse_request_line(line: &str) -> (Option<String>, Option<String>) {
    let mut parts = line.split_whitespace();
    (
        parts.next().map(str::to_string),
        parts.next().map(str::to_string),
    )
}

async fn write_answer(stream: &mut TcpStream, sdp: &str) -> std::io::Result<()> {
    let header = format!(
        "HTTP/1.1 201 Created\r\n\
         Content-Type: application/sdp\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Location: /whip/resource\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        sdp.len()
    );
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(sdp.as_bytes()).await?;
    stream.flush().await
}

async fn write_no_content(stream: &mut TcpStream) -> std::io::Result<()> {
    let header = "HTTP/1.1 204 No Content\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Access-Control-Allow-Methods: POST, OPTIONS\r\n\
         Access-Control-Allow-Headers: Content-Type, Accept\r\n\
         Connection: close\r\n\r\n";
    stream.write_all(header.as_bytes()).await?;
    stream.flush().await
}

async fn write_error(stream: &mut TcpStream, status: u16, message: &str) -> std::io::Result<()> {
    let status_text = match status {
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Internal Server Error",
    };
    let header = format!(
        "HTTP/1.1 {status} {status_text}\r\n\
         Content-Type: text/plain\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        message.len()
    );
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(message.as_bytes()).await?;
    stream.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_request_line_extracts_method_and_path() {
        assert_eq!(
            parse_request_line("POST /whip HTTP/1.1\r\n"),
            (Some("POST".to_string()), Some("/whip".to_string()))
        );
        assert_eq!(parse_request_line(""), (None, None));
    }
}
