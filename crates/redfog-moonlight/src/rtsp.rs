//! RTSP (48010/tcp) handshake server.
//!
//! Real Moonlight clients speak RFC 2326 RTSP framing (`rtsp-types` handles
//! that part faithfully) with a Moonlight-specific dialect layered on top:
//! `a=x-nv-*` SDP attributes describing capabilities/negotiated parameters,
//! and no real dynamic port negotiation (video/audio/control UDP ports are
//! fixed, exchanged out of band rather than via `Transport` headers). Exact
//! attribute names/shapes are the part most likely to need adjustment once
//! tested against a real client (see plan doc "known risks").

use std::sync::Arc;

use rtsp_types::headers::{CONTENT_TYPE, CSEQ, PUBLIC, SESSION, TRANSPORT};
use rtsp_types::{Message, Method, ParseError, Request, Response, StatusCode, Version};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Parameters negotiated by the client's `ANNOUNCE` body. Falls back to the
/// `/launch` HTTP query params for anything not found in the SDP.
#[derive(Debug, Clone, Copy)]
pub struct AnnouncedParams {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: Option<u32>,
}

pub trait RtspHandler: Send + Sync {
    fn on_announce(&self, params: AnnouncedParams);
    fn on_play(&self);
}

pub struct NoopRtspHandler;
impl RtspHandler for NoopRtspHandler {
    fn on_announce(&self, _params: AnnouncedParams) {}
    fn on_play(&self) {}
}

/// Real clients send `SETUP` requests with a request-target like
/// `streamid=audio/0/0` — not a valid absolute URI or `*`, so `rtsp-types`'
/// strict RFC 2326 parser rejects the whole message outright rather than
/// just failing to make sense of that one token. Once the buffer contains a
/// full request line, rewrite the target to `*` in place so the rest of the
/// message (method, CSeq, headers) still parses normally. Returns the
/// original target once rewritten (or already valid), `None` if the first
/// line isn't fully buffered yet.
fn rewrite_request_target(buf: &mut Vec<u8>) -> Option<String> {
    let line_end = buf.windows(2).position(|w| w == b"\r\n")?;
    let line = String::from_utf8_lossy(&buf[..line_end]).into_owned();
    let mut parts = line.splitn(3, ' ');
    let method = parts.next()?;
    let target = parts.next()?.to_string();
    let version = parts.next()?;

    if target == "*" || target.starts_with("rtsp://") {
        return Some(target);
    }

    let new_line = format!("{method} * {version}");
    buf.splice(..line_end, new_line.into_bytes());
    Some(target)
}

pub struct RtspServer {
    pub port: u16,
    pub video_port: u16,
    pub control_port: u16,
    pub audio_port: u16,
    pub default_width: u32,
    pub default_height: u32,
    pub default_fps: u32,
    pub handler: Arc<dyn RtspHandler>,
    /// One session at a time in this iteration, so a single id generated
    /// once (not per-connection — see `handle_connection`'s doc comment for
    /// why each request is now its own TCP connection) is enough to keep
    /// `Session:` consistent across SETUP/PLAY.
    pub session_id: String,
}

impl RtspServer {
    pub async fn serve(self: Arc<Self>, bind_addr: std::net::IpAddr) -> Result<(), String> {
        let listener = TcpListener::bind((bind_addr, self.port))
            .await
            .map_err(|e| format!("failed to bind rtsp {}:{}: {e}", bind_addr, self.port))?;
        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("rtsp accept failed: {e}");
                    continue;
                }
            };
            let this = self.clone();
            tokio::spawn(async move {
                if let Err(e) = this.handle_connection(stream).await {
                    tracing::debug!("rtsp connection from {peer} ended: {e}");
                }
            });
        }
    }

    /// Handles exactly one request then closes the connection. Real Moonlight
    /// clients (confirmed via `moonlight-common-rust`'s sans-io RTSP client:
    /// it only parses the response once it observes TCP disconnect) use one
    /// TCP connection per RTSP request — HTTP/1.0-style, not a persistent
    /// connection for the whole OPTIONS/DESCRIBE/SETUP*/ANNOUNCE/PLAY
    /// sequence like RFC 2326 RTSP normally would.
    async fn handle_connection(&self, mut stream: tokio::net::TcpStream) -> Result<(), String> {
        let mut buf = Vec::new();
        let mut read_buf = [0u8; 4096];
        let mut original_target = None;

        let (message, consumed) = loop {
            if original_target.is_none() {
                original_target = rewrite_request_target(&mut buf);
            }
            match Message::<Vec<u8>>::parse(&buf) {
                Ok((message, consumed)) => break (message, consumed),
                Err(ParseError::Incomplete(_)) => {
                    let n = stream.read(&mut read_buf).await.map_err(|e| format!("read error: {e}"))?;
                    if n == 0 {
                        return Ok(()); // connection closed before a full request arrived
                    }
                    buf.extend_from_slice(&read_buf[..n]);
                }
                Err(e) => return Err(format!("rtsp parse error: {e}, buf: {:?}", String::from_utf8_lossy(&buf))),
            }
        };
        let _ = consumed;

        let Message::Request(request) = message else {
            return Ok(()); // ignore stray Response/Data frames
        };
        tracing::debug!("rtsp: {:?} cseq={:?} target={:?}", request.method(), request.header(&CSEQ), original_target);

        let response = self
            .handle_request(&request, &self.session_id, original_target.as_deref().unwrap_or(""))
            .await;
        let mut out = Vec::new();
        response
            .write(&mut out)
            .map_err(|e| format!("failed to serialize rtsp response: {e}"))?;
        tracing::debug!("rtsp: responding {:?}", String::from_utf8_lossy(&out));
        stream.write_all(&out).await.map_err(|e| format!("write error: {e}"))?;
        stream.shutdown().await.map_err(|e| format!("shutdown error: {e}"))?;
        Ok(())
    }

    async fn handle_request(&self, request: &Request<Vec<u8>>, session_id: &str, target: &str) -> Response<Vec<u8>> {
        let cseq = request.header(&CSEQ).cloned();
        let mut response = match *request.method() {
            Method::Options => Response::builder(Version::V1_0, StatusCode::Ok)
                .header(PUBLIC, "OPTIONS, DESCRIBE, SETUP, ANNOUNCE, PLAY, TEARDOWN")
                .build(Vec::new()),
            Method::Describe => Response::builder(Version::V1_0, StatusCode::Ok)
                .header(CONTENT_TYPE, "application/sdp")
                .build(self.sdp().into_bytes()),
            // The client reads the actual per-stream port from this
            // response's `Transport: server_port=X` header (confirmed via
            // moonlight-common-rust's SETUP response parsing) — not from
            // the SDP. Without it, it falls back to hardcoded defaults,
            // which for the control stream is a bug in that crate: it uses
            // the *video* port constant as the control-port fallback. So
            // this header isn't optional — get it wrong and the client
            // connects its control channel to the wrong port.
            Method::Setup => {
                // Real targets: "streamid=audio/0/0", "streamid=video/0/0",
                // but "stream=control/13/0" — note "control" drops the "id",
                // an actual inconsistency in the wire protocol, not a typo.
                let port = if target.contains("=audio") {
                    self.audio_port
                } else if target.contains("=control") {
                    self.control_port
                } else {
                    self.video_port
                };
                Response::builder(Version::V1_0, StatusCode::Ok)
                    .header(SESSION, session_id.to_string())
                    .header(TRANSPORT, format!("unicast;server_port={port}-{}", port + 1))
                    .build(Vec::new())
            }
            Method::Announce => {
                self.handler.on_announce(self.parse_announce(request.body()));
                Response::builder(Version::V1_0, StatusCode::Ok).build(Vec::new())
            }
            Method::Play => {
                // `on_play` can block (waiting for a concurrent `/launch`'s
                // slow compositor spawn to finish) — run it on the blocking
                // pool so it can't stall this connection's tokio worker
                // thread, same reasoning as the `pairing.rs` launch/resume/
                // cancel call sites.
                let handler = self.handler.clone();
                if let Err(e) = tokio::task::spawn_blocking(move || handler.on_play()).await {
                    tracing::error!("on_play task panicked: {e}");
                }
                Response::builder(Version::V1_0, StatusCode::Ok)
                    .header(SESSION, session_id.to_string())
                    .build(Vec::new())
            }
            Method::Teardown => Response::builder(Version::V1_0, StatusCode::Ok).build(Vec::new()),
            _ => Response::builder(Version::V1_0, StatusCode::MethodNotAllowed).build(Vec::new()),
        };
        if let Some(cseq) = cseq {
            response.insert_header(CSEQ, cseq);
        }
        response
    }

    fn parse_announce(&self, body: &[u8]) -> AnnouncedParams {
        let text = String::from_utf8_lossy(body);
        let mut width = self.default_width;
        let mut height = self.default_height;
        let mut fps = self.default_fps;
        let mut bitrate_kbps = None;

        for line in text.lines() {
            let Some(rest) = line.strip_prefix("a=x-nv-video[0].") else {
                continue;
            };
            let Some((key, value)) = rest.split_once(':') else {
                continue;
            };
            let value = value.trim();
            match key {
                "clientViewportWd" => width = value.parse().unwrap_or(width),
                "clientViewportHt" => height = value.parse().unwrap_or(height),
                "maxFPS" => fps = value.parse().unwrap_or(fps),
                "initialBitrate" => bitrate_kbps = value.parse().ok(),
                _ => {}
            }
        }

        AnnouncedParams {
            width,
            height,
            fps,
            bitrate_kbps,
        }
    }

    fn sdp(&self) -> String {
        // `encryptionSupported` is a bitmask: CONTROL_V2=0x01, VIDEO=0x02,
        // AUDIO=0x04 (confirmed against moonlight-common-rust's
        // `SunshineEncryptionFlags`). Only CONTROL_V2 — control.rs already
        // implements it, video/audio don't. Without this bit set, real
        // clients (having also been told we're "Sunshine-like" via
        // `<appversion>`, see pairing.rs) still fall back to skipping
        // control-channel encryption negotiation entirely.
        format!(
            "v=0\r\n\
             o=redfog 0 0 IN IPv4 0.0.0.0\r\n\
             s=redfog-server\r\n\
             a=x-ss-general.featureFlags:0\r\n\
             a=x-ss-general.encryptionSupported:1\r\n\
             a=x-nv-video[0].videoPort:{video_port}\r\n\
             a=x-nv-general.serverControlPort:{control_port}\r\n\
             a=x-nv-general.serverAudioPort:{audio_port}\r\n",
            video_port = self.video_port,
            control_port = self.control_port,
            audio_port = self.audio_port,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server() -> RtspServer {
        RtspServer {
            port: 48010,
            video_port: 47998,
            control_port: 47999,
            audio_port: 48000,
            default_width: 1920,
            default_height: 1080,
            default_fps: 60,
            handler: Arc::new(NoopRtspHandler),
            session_id: "deadbeef".to_string(),
        }
    }

    #[tokio::test]
    async fn options_echoes_cseq() {
        let request = Request::builder(Method::Options, Version::V1_0)
            .header(CSEQ, "1")
            .empty();
        let response = server()
            .handle_request(&request.replace_body(Vec::new()), "deadbeef", "*")
            .await;
        assert_eq!(response.status(), StatusCode::Ok);
        assert_eq!(response.header(&CSEQ).map(|v| v.as_str()), Some("1"));
    }

    #[tokio::test]
    async fn setup_returns_session_id_and_correct_transport_port() {
        let request = Request::builder(Method::Setup, Version::V1_0)
            .header(CSEQ, "2")
            .empty();
        // Real clients send "stream=control/N/0" (not "streamid=") for this one — see rtsp.rs comment.
        let response = server()
            .handle_request(&request.replace_body(Vec::new()), "deadbeef", "stream=control/13/0")
            .await;
        assert_eq!(response.header(&SESSION).map(|v| v.as_str()), Some("deadbeef"));
        assert_eq!(response.header(&TRANSPORT).map(|v| v.as_str()), Some("unicast;server_port=47999-48000"));
    }

    #[test]
    fn announce_parses_viewport_and_falls_back_for_missing_fields() {
        let body = "a=x-nv-video[0].clientViewportWd:1280\r\na=x-nv-video[0].clientViewportHt:720\r\n";
        let params = server().parse_announce(body.as_bytes());
        assert_eq!(params.width, 1280);
        assert_eq!(params.height, 720);
        assert_eq!(params.fps, 60); // fell back to default, wasn't in the body
        assert_eq!(params.bitrate_kbps, None);
    }
}
