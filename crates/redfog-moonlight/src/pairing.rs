//! HTTP (47989) + HTTPS (47984) pairing/app-list server.
//!
//! Mirrors the GameStream/Sunshine wire protocol: `/serverinfo`, `/applist`,
//! the 5-step `/pair` PIN handshake (see `crypto.rs`/`clients.rs` for the
//! actual crypto), `/unpair`, `/launch`, `/resume`, `/cancel`.

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

use crate::clients::ClientManager;
use crate::tls::ServerIdentity;

/// Fixed single-entry app list for this iteration — no app scanning.
pub const APP_NAME: &str = "Desktop";
pub const APP_ID: u32 = 1;

/// (width, height, refresh_rate). A few common resolutions the client can
/// choose from — real hosts report a wider list, but the exact set doesn't
/// need to be exhaustive since resolution is ultimately fixed by whatever
/// `/launch` requests, not chosen from this list.
const SUPPORTED_DISPLAY_MODES: &[(u32, u32, u32)] = &[
    (1280, 720, 60),
    (1920, 1080, 60),
    (1920, 1080, 30),
    (2560, 1440, 60),
    (3840, 2160, 60),
];

/// The client's remote-input key, sent as `rikey`/`rikeyid` query params on
/// `/launch` (not in the RTSP SDP) — used to decrypt the ENet control channel
/// (see control.rs) and, if enabled later, audio/video stream encryption.
#[derive(Debug, Clone, Copy)]
pub struct RemoteInputKey {
    pub key: [u8; 16],
    pub key_id: i64,
}

/// Hook for `/launch`, `/resume`, `/cancel` to drive the actual session state
/// machine. Wired up properly once the session module (task 11) exists;
/// until then a no-op implementation is used.
pub trait LaunchHandler: Send + Sync {
    fn launch(&self, width: u32, height: u32, fps: u32, rikey: RemoteInputKey) -> Result<(), String>;
    fn resume(&self) -> Result<(), String>;
    fn cancel(&self) -> Result<(), String>;
}

pub struct NoopLaunchHandler;
impl LaunchHandler for NoopLaunchHandler {
    fn launch(&self, _width: u32, _height: u32, _fps: u32, _rikey: RemoteInputKey) -> Result<(), String> {
        Ok(())
    }
    fn resume(&self) -> Result<(), String> {
        Ok(())
    }
    fn cancel(&self) -> Result<(), String> {
        Ok(())
    }
}

pub struct PairingServer {
    pub clients: Arc<ClientManager>,
    pub identity: ServerIdentity,
    pub hostname: String,
    pub http_port: u16,
    pub https_port: u16,
    pub rtsp_port: u16,
    pub launch_handler: Arc<dyn LaunchHandler>,
}

impl PairingServer {
    pub async fn serve_http(self: Arc<Self>, bind_addr: std::net::IpAddr) -> Result<(), String> {
        let listener = TcpListener::bind((bind_addr, self.http_port))
            .await
            .map_err(|e| format!("failed to bind http {}:{}: {e}", bind_addr, self.http_port))?;
        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("http accept failed: {e}");
                    continue;
                }
            };
            let local_addr = stream.local_addr().unwrap_or(std::net::SocketAddr::new(bind_addr, self.http_port));
            let this = self.clone();
            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let service = service_fn(move |req| {
                    let this = this.clone();
                    async move { Ok::<_, Infallible>(this.handle(req, peer, local_addr, false).await) }
                });
                if let Err(e) = http1::Builder::new().serve_connection(io, service).await {
                    tracing::debug!("http connection from {peer} ended: {e}");
                }
            });
        }
    }

    pub async fn serve_https(self: Arc<Self>, bind_addr: std::net::IpAddr) -> Result<(), String> {
        let cert = rustls_pemfile::certs(&mut self.identity.cert_pem.as_bytes())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("failed to parse server cert: {e}"))?;
        let key = rustls_pemfile::private_key(&mut self.identity.private_key_pem.as_bytes())
            .map_err(|e| format!("failed to parse server key: {e}"))?
            .ok_or("no private key found in server identity")?;

        // No ALPN restriction: if the client sends an ALPN extension listing
        // protocols we don't include (e.g. "h2"), rustls fails the handshake
        // outright per RFC 7301 rather than falling back — better to not
        // negotiate ALPN at all than to guess wrong and hard-fail every
        // connection. We only ever speak HTTP/1.1 regardless.
        let tls_config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(cert, key)
            .map_err(|e| format!("failed to build tls config: {e}"))?;
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls_config));

        let listener = TcpListener::bind((bind_addr, self.https_port))
            .await
            .map_err(|e| format!("failed to bind https {}:{}: {e}", bind_addr, self.https_port))?;
        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("https accept failed: {e}");
                    continue;
                }
            };
            let local_addr = stream.local_addr().unwrap_or(std::net::SocketAddr::new(bind_addr, self.https_port));
            let this = self.clone();
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let tls_stream = match acceptor.accept(stream).await {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::debug!("tls handshake with {peer} failed: {e}");
                        return;
                    }
                };
                let io = TokioIo::new(tls_stream);
                let service = service_fn(move |req| {
                    let this = this.clone();
                    async move { Ok::<_, Infallible>(this.handle(req, peer, local_addr, true).await) }
                });
                if let Err(e) = http1::Builder::new().serve_connection(io, service).await {
                    tracing::debug!("https connection from {peer} ended: {e}");
                }
            });
        }
    }

    async fn handle(&self, req: Request<Incoming>, peer: SocketAddr, local_addr: SocketAddr, https: bool) -> Response<Full<Bytes>> {
        let path = req.uri().path().to_string();
        let params = parse_query(req.uri().query().unwrap_or(""));

        tracing::debug!("{peer} {} {path}{}", if https { "https" } else { "http" }, {
            let mut q = String::new();
            if req.uri().query().is_some() {
                q.push('?');
                q.push_str(req.uri().query().unwrap());
            }
            q
        });

        match path.as_str() {
            "/serverinfo" => self.server_info(&params),
            "/applist" => self.app_list(),
            "/pair" => self.pair(&params).await,
            "/unpair" => self.unpair(&params),
            "/launch" => self.launch(&params, local_addr.ip()),
            "/resume" => self.resume(&params, local_addr.ip()),
            "/cancel" => self.cancel(&params),
            "/pin" => self.pin_page(&params),
            "/submit-pin" => self.submit_pin_query(&params, req.into_body()).await,
            _ => not_found(),
        }
    }

    fn server_info(&self, params: &HashMap<String, String>) -> Response<Full<Bytes>> {
        let paired = params
            .get("uniqueid")
            .map(|id| self.clients.is_paired(id))
            .unwrap_or(false);
        let body = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<root status_code="200">
    <hostname>{hostname}</hostname>
    <appversion>7.1.415.0</appversion>
    <GfeVersion>3.23.0.74</GfeVersion>
    <uniqueid>{server_id}</uniqueid>
    <MaxLumaPixelsHEVC>0</MaxLumaPixelsHEVC>
    <ServerCodecModeSupport>0</ServerCodecModeSupport>
    <HttpsPort>{https_port}</HttpsPort>
    <ExternalPort>{http_port}</ExternalPort>
    <mac>00:00:00:00:00:00</mac>
    <LocalIP>127.0.0.1</LocalIP>
    <SupportedDisplayMode>
{display_modes}    </SupportedDisplayMode>
    <PairStatus>{pair_status}</PairStatus>
    <currentgame>0</currentgame>
    <state>SUNSHINE_SERVER_FREE</state>
</root>"#,
            hostname = self.hostname,
            server_id = self.identity.unique_id,
            https_port = self.https_port,
            http_port = self.http_port,
            pair_status = if paired { 1 } else { 0 },
            display_modes = SUPPORTED_DISPLAY_MODES
                .iter()
                .map(|(w, h, hz)| format!("        <DisplayMode><Width>{w}</Width><Height>{h}</Height><RefreshRate>{hz}</RefreshRate></DisplayMode>\n"))
                .collect::<String>(),
        );
        xml_response(body)
    }

    fn app_list(&self) -> Response<Full<Bytes>> {
        let body = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<root status_code="200">
    <App>
        <AppTitle>{APP_NAME}</AppTitle>
        <ID>{APP_ID}</ID>
        <IsHdrSupported>0</IsHdrSupported>
    </App>
</root>"#
        );
        xml_response(body)
    }

    async fn pair(&self, params: &HashMap<String, String>) -> Response<Full<Bytes>> {
        if let Some(phrase) = params.get("phrase") {
            return match phrase.as_str() {
                "getservercert" => self.pair_get_server_cert(params).await,
                "pairchallenge" => xml_response(paired_xml("")),
                other => bad_request(format!("unknown pair phrase: {other}")),
            };
        }
        if let Some(challenge) = params.get("clientchallenge") {
            return self.pair_client_challenge(params, challenge);
        }
        if let Some(resp) = params.get("serverchallengeresp") {
            return self.pair_server_challenge_response(params, resp);
        }
        if let Some(secret) = params.get("clientpairingsecret") {
            return self.pair_client_pairing_secret(params, secret);
        }
        bad_request("unrecognized /pair request".to_string())
    }

    async fn pair_get_server_cert(&self, params: &HashMap<String, String>) -> Response<Full<Bytes>> {
        let (Some(unique_id), Some(client_cert_hex), Some(salt_hex)) =
            (params.get("uniqueid"), params.get("clientcert"), params.get("salt"))
        else {
            return bad_request("missing uniqueid/clientcert/salt".to_string());
        };

        let client_cert_pem = match hex::decode(client_cert_hex).ok().and_then(|b| String::from_utf8(b).ok()) {
            Some(pem) => pem,
            None => return bad_request("invalid clientcert".to_string()),
        };
        let salt: [u8; 16] = match hex::decode(salt_hex).ok().and_then(|s| s.try_into().ok()) {
            Some(s) => s,
            None => return bad_request("invalid salt (expected 16 bytes)".to_string()),
        };

        let pin_ready = self.clients.start_pairing(unique_id, client_cert_pem, salt);

        // Block here until a human relays the PIN (via `/submit-pin`, or
        // later, a field in the redfog-login UI) — the client is blocked on
        // this same response, same as real Sunshine/moonshine. A real
        // client's own request timeout is generous (pairing involves a
        // human), but bound it so a client that never submits a PIN doesn't
        // leak a task forever.
        if tokio::time::timeout(std::time::Duration::from_secs(120), pin_ready.notified())
            .await
            .is_err()
        {
            return bad_request("timed out waiting for PIN".to_string());
        }

        xml_response(paired_xml(&format!(
            "<plaincert>{}</plaincert>",
            hex::encode(&self.identity.cert_pem)
        )))
    }

    fn pair_client_challenge(&self, params: &HashMap<String, String>, challenge_hex: &str) -> Response<Full<Bytes>> {
        let Some(unique_id) = params.get("uniqueid") else {
            return bad_request("missing uniqueid".to_string());
        };
        let Ok(challenge) = hex::decode(challenge_hex) else {
            return bad_request("invalid clientchallenge".to_string());
        };
        match self.clients.client_challenge(unique_id, &challenge) {
            Ok(response) => xml_response(paired_xml(&format!(
                "<challengeresponse>{}</challengeresponse>",
                hex::encode(response)
            ))),
            Err(e) => bad_request(e),
        }
    }

    fn pair_server_challenge_response(&self, params: &HashMap<String, String>, resp_hex: &str) -> Response<Full<Bytes>> {
        let Some(unique_id) = params.get("uniqueid") else {
            return bad_request("missing uniqueid".to_string());
        };
        let Ok(response) = hex::decode(resp_hex) else {
            return bad_request("invalid serverchallengeresp".to_string());
        };
        match self.clients.server_challenge_response(unique_id, &response) {
            Ok(pairing_secret) => xml_response(paired_xml(&format!(
                "<pairingsecret>{}</pairingsecret>",
                hex::encode(pairing_secret)
            ))),
            Err(e) => bad_request(e),
        }
    }

    fn pair_client_pairing_secret(&self, params: &HashMap<String, String>, secret_hex: &str) -> Response<Full<Bytes>> {
        let Some(unique_id) = params.get("uniqueid") else {
            return bad_request("missing uniqueid".to_string());
        };
        let Ok(secret) = hex::decode(secret_hex) else {
            return bad_request("invalid clientpairingsecret".to_string());
        };
        match self.clients.check_client_pairing_secret(unique_id, &secret) {
            Ok(()) => xml_response(paired_xml("")),
            Err(e) => bad_request(e),
        }
    }

    fn parse_rikey(&self, params: &HashMap<String, String>) -> Option<RemoteInputKey> {
        let key: [u8; 16] = hex::decode(params.get("rikey")?).ok()?.try_into().ok()?;
        let key_id: i64 = params.get("rikeyid")?.parse().ok()?;
        Some(RemoteInputKey { key, key_id })
    }

    fn unpair(&self, _params: &HashMap<String, String>) -> Response<Full<Bytes>> {
        // TODO: actually remove from the paired-clients store once ClientManager exposes that.
        xml_response(paired_xml(""))
    }

    fn launch(&self, params: &HashMap<String, String>, local_ip: std::net::IpAddr) -> Response<Full<Bytes>> {
        let width = params.get("width").and_then(|s| s.parse().ok()).unwrap_or(1920);
        let height = params.get("height").and_then(|s| s.parse().ok()).unwrap_or(1080);
        let fps = params.get("fps").and_then(|s| s.parse().ok()).unwrap_or(60);

        let Some(rikey) = self.parse_rikey(params) else {
            return bad_request("missing/invalid rikey/rikeyid".to_string());
        };

        match self.launch_handler.launch(width, height, fps, rikey) {
            // The client parses this as a raw socket address, not a hostname
            // (no DNS resolution) — mirror back the IP it actually used to
            // reach us, from the accepted connection's local address.
            Ok(()) => xml_response(format!(
                r#"<?xml version="1.0" encoding="utf-8"?>
<root status_code="200">
    <gamesession>1</gamesession>
    <sessionUrl0>rtsp://{local_ip}:{rtsp_port}</sessionUrl0>
</root>"#,
                rtsp_port = self.rtsp_port,
            )),
            Err(e) => bad_request(e),
        }
    }

    fn resume(&self, _params: &HashMap<String, String>, local_ip: std::net::IpAddr) -> Response<Full<Bytes>> {
        match self.launch_handler.resume() {
            Ok(()) => xml_response(format!(
                r#"<?xml version="1.0" encoding="utf-8"?>
<root status_code="200">
    <resume>1</resume>
    <sessionUrl0>rtsp://{local_ip}:{rtsp_port}</sessionUrl0>
</root>"#,
                rtsp_port = self.rtsp_port,
            )),
            Err(e) => bad_request(e),
        }
    }

    fn cancel(&self, _params: &HashMap<String, String>) -> Response<Full<Bytes>> {
        match self.launch_handler.cancel() {
            Ok(()) => xml_response(paired_xml("<cancel>1</cancel>")),
            Err(e) => bad_request(e),
        }
    }

    fn pin_page(&self, params: &HashMap<String, String>) -> Response<Full<Bytes>> {
        let unique_id = params.get("uniqueid").cloned().unwrap_or_default();
        let body = format!(
            r#"<!doctype html><html><body>
<form method="POST" action="/submit-pin">
  <input type="hidden" name="uniqueid" value="{unique_id}">
  <label>PIN shown on your Moonlight client: <input name="pin" autofocus></label>
  <button type="submit">Submit</button>
</form>
</body></html>"#
        );
        Response::builder()
            .header("Content-Type", "text/html")
            .body(Full::new(Bytes::from(body)))
            .unwrap()
    }

    async fn submit_pin_query(&self, query_params: &HashMap<String, String>, body: Incoming) -> Response<Full<Bytes>> {
        let mut params = query_params.clone();
        if let Ok(collected) = body.collect().await {
            let form = parse_query(&String::from_utf8_lossy(&collected.to_bytes()));
            params.extend(form);
        }
        let (Some(unique_id), Some(pin)) = (params.get("uniqueid"), params.get("pin")) else {
            return bad_request("missing uniqueid/pin".to_string());
        };
        match self.clients.submit_pin(unique_id, pin) {
            Ok(()) => Response::builder()
                .header("Content-Type", "text/plain")
                .body(Full::new(Bytes::from("ok")))
                .unwrap(),
            Err(e) => bad_request(e),
        }
    }
}

fn parse_query(query: &str) -> HashMap<String, String> {
    form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect()
}

fn paired_xml(inner: &str) -> String {
    format!(r#"<?xml version="1.0" encoding="utf-8"?><root status_code="200"><paired>1</paired>{inner}</root>"#)
}

fn xml_response(body: impl Into<String>) -> Response<Full<Bytes>> {
    Response::builder()
        .header("Content-Type", "application/xml")
        .body(Full::new(Bytes::from(body.into())))
        .unwrap()
}

fn bad_request(message: String) -> Response<Full<Bytes>> {
    tracing::warn!("bad pairing request: {message}");
    Response::builder()
        .status(400)
        .body(Full::new(Bytes::from(message)))
        .unwrap()
}

fn not_found() -> Response<Full<Bytes>> {
    Response::builder().status(404).body(Full::new(Bytes::new())).unwrap()
}
