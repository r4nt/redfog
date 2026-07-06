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

use rtsp_types::headers::{CONTENT_TYPE, CSEQ, PUBLIC, SESSION};
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

pub struct RtspServer {
    pub port: u16,
    pub video_port: u16,
    pub control_port: u16,
    pub audio_port: u16,
    pub default_width: u32,
    pub default_height: u32,
    pub default_fps: u32,
    pub handler: Arc<dyn RtspHandler>,
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

    async fn handle_connection(&self, mut stream: tokio::net::TcpStream) -> Result<(), String> {
        let session_id = format!("{:016X}", rand::random::<u64>());
        let mut buf = Vec::new();
        let mut read_buf = [0u8; 4096];

        loop {
            let (message, consumed) = loop {
                match Message::<Vec<u8>>::parse(&buf) {
                    Ok((message, consumed)) => break (message, consumed),
                    Err(ParseError::Incomplete(_)) => {
                        let n = stream
                            .read(&mut read_buf)
                            .await
                            .map_err(|e| format!("read error: {e}"))?;
                        if n == 0 {
                            return Ok(()); // connection closed
                        }
                        buf.extend_from_slice(&read_buf[..n]);
                    }
                    Err(e) => return Err(format!("rtsp parse error: {e}")),
                }
            };
            buf.drain(..consumed);

            let Message::Request(request) = message else {
                continue; // ignore stray Response/Data frames
            };

            let response = self.handle_request(&request, &session_id);
            let mut out = Vec::new();
            response
                .write(&mut out)
                .map_err(|e| format!("failed to serialize rtsp response: {e}"))?;
            stream
                .write_all(&out)
                .await
                .map_err(|e| format!("write error: {e}"))?;
        }
    }

    fn handle_request(&self, request: &Request<Vec<u8>>, session_id: &str) -> Response<Vec<u8>> {
        let cseq = request.header(&CSEQ).cloned();
        let mut response = match *request.method() {
            Method::Options => Response::builder(Version::V1_0, StatusCode::Ok)
                .header(PUBLIC, "OPTIONS, DESCRIBE, SETUP, ANNOUNCE, PLAY, TEARDOWN")
                .build(Vec::new()),
            Method::Describe => Response::builder(Version::V1_0, StatusCode::Ok)
                .header(CONTENT_TYPE, "application/sdp")
                .build(self.sdp().into_bytes()),
            Method::Setup => Response::builder(Version::V1_0, StatusCode::Ok)
                .header(SESSION, session_id.to_string())
                .build(Vec::new()),
            Method::Announce => {
                self.handler.on_announce(self.parse_announce(request.body()));
                Response::builder(Version::V1_0, StatusCode::Ok).build(Vec::new())
            }
            Method::Play => {
                self.handler.on_play();
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
        format!(
            "v=0\r\n\
             o=redfog 0 0 IN IP4 0.0.0.0\r\n\
             s=redfog-server\r\n\
             a=x-ss-general.featureFlags:0\r\n\
             a=x-ss-general.encryptionSupported:0\r\n\
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
        }
    }

    #[test]
    fn options_echoes_cseq() {
        let request = Request::builder(Method::Options, Version::V1_0)
            .header(CSEQ, "1")
            .empty();
        let response = server().handle_request(&request.replace_body(Vec::new()), "deadbeef");
        assert_eq!(response.status(), StatusCode::Ok);
        assert_eq!(response.header(&CSEQ).map(|v| v.as_str()), Some("1"));
    }

    #[test]
    fn setup_returns_session_id() {
        let request = Request::builder(Method::Setup, Version::V1_0)
            .header(CSEQ, "2")
            .empty();
        let response = server().handle_request(&request.replace_body(Vec::new()), "deadbeef");
        assert_eq!(response.header(&SESSION).map(|v| v.as_str()), Some("deadbeef"));
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
