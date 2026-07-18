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
                    async move { Ok::<_, Infallible>(this.handle(req, peer, local_addr, false, None).await) }
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
        //
        // Client auth is requested (not `with_no_client_auth`) so we can see
        // *which* certificate a request actually presents — see
        // `is_paired_by_cert`'s doc comment for why that's the only
        // trustworthy way to tell two physical clients apart.
        let tls_config = rustls::ServerConfig::builder()
            .with_client_cert_verifier(crate::tls::AcceptAnyClientCert::new())
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
                let client_cert_fingerprint = tls_stream
                    .get_ref()
                    .1
                    .peer_certificates()
                    .and_then(|certs| certs.first())
                    .map(|cert| crate::crypto::cert_fingerprint_der(cert));
                let io = TokioIo::new(tls_stream);
                let service = service_fn(move |req| {
                    let this = this.clone();
                    let client_cert_fingerprint = client_cert_fingerprint.clone();
                    async move { Ok::<_, Infallible>(this.handle(req, peer, local_addr, true, client_cert_fingerprint).await) }
                });
                if let Err(e) = http1::Builder::new().serve_connection(io, service).await {
                    tracing::debug!("https connection from {peer} ended: {e}");
                }
            });
        }
    }

    async fn handle(
        &self,
        req: Request<Incoming>,
        peer: SocketAddr,
        local_addr: SocketAddr,
        https: bool,
        client_cert_fingerprint: Option<String>,
    ) -> Response<Full<Bytes>> {
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
            "/serverinfo" => self.server_info(&params, https, client_cert_fingerprint.as_deref()),
            "/applist" => self.app_list(),
            "/pair" => self.pair(&params).await,
            "/unpair" => self.unpair(&params),
            "/launch" => self.launch(&params, local_addr.ip()).await,
            "/resume" => self.resume(&params, local_addr.ip()).await,
            "/cancel" => self.cancel(&params).await,
            "/pin" => self.pin_page(&params),
            "/submit-pin" => self.submit_pin_query(&params, req.into_body()).await,
            "/pending-pairs" => self.pending_pairs(),
            _ => not_found(),
        }
    }

    fn server_info(&self, _params: &HashMap<String, String>, https: bool, client_cert_fingerprint: Option<&str>) -> Response<Full<Bytes>> {
        // `uniqueid` alone can't tell two physical clients apart — real
        // Moonlight clients share a hardcoded placeholder uniqueid for any
        // server they don't detect as genuine Nvidia GFE (see
        // `ClientManager::is_paired_by_cert`'s doc comment). Plain HTTP has
        // no TLS session to pull a certificate from, so it's always reported
        // unpaired — matches Wolf, whose HTTP `/serverinfo` handler passes an
        // empty client unconditionally for the same reason.
        let paired = https && client_cert_fingerprint.is_some_and(|fp| self.clients.is_paired_by_cert(fp));
        // `ServerCodecModeSupport` is a bitmask (0x0001 = H.264, confirmed
        // against Wolf's implementation), not a boolean/placeholder — `0`
        // doesn't mean "no explicit info", it means "zero codecs supported".
        // moonlight-qt takes that completely literally: it strips even H.264
        // out of its own supported-formats list before ever attempting to
        // stream, so the connection silently dies before RTSP every time.
        // The 4th version component negative marks us "Sunshine-like" to real
        // clients (moonlight-common-rust's `ServerVersion::new`: `server_type
        // = Sunshine` iff this component is negative) — confirmed against
        // Wolf, which advertises the exact same "7.1.431.-1". Without it,
        // clients treat us as genuine Nvidia GameStream and skip ALL
        // Sunshine-specific negotiation, including which control-channel
        // input-encryption scheme (IV/key derivation) to use — the likely
        // cause of input packets failing to decrypt/parse on our end.
        let body = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<root status_code="200">
    <hostname>{hostname}</hostname>
    <appversion>7.1.431.-1</appversion>
    <GfeVersion>3.23.0.74</GfeVersion>
    <uniqueid>{server_id}</uniqueid>
    <MaxLumaPixelsHEVC>0</MaxLumaPixelsHEVC>
    <ServerCodecModeSupport>1</ServerCodecModeSupport>
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
        tracing::info!("pairing started for uniqueid={unique_id}, waiting for PIN via /submit-pin");

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

    /// The client's requested resolution/fps, `(width, height, fps)`.
    ///
    /// Real clients send a single combined `mode` param
    /// (`"{width}x{height}x{fps}"`, e.g. `mode=1920x1080x30` — confirmed
    /// against moonlight-common-rust's own launch-request builder,
    /// `http/launch.rs`: `key: "mode"`, not vendored into git, see
    /// scripts/fetch-patched-deps.sh) — there are no separate `width=`/
    /// `height=`/`fps=` query params in the real protocol at all. A
    /// previous version of this function looked for exactly those three
    /// separate keys, which real requests never contain, so it silently
    /// fell back to the hardcoded 1920x1080x60 default on *every* launch,
    /// completely ignoring whatever resolution the client actually
    /// requested — confirmed live against a real captured `/launch` URL,
    /// which only ever had `mode=1920x1080x30`, no `width`/`height`/`fps`
    /// keys at all.
    fn parse_mode(params: &HashMap<String, String>) -> (u32, u32, u32) {
        const DEFAULT: (u32, u32, u32) = (1920, 1080, 60);
        let Some(mode) = params.get("mode") else { return DEFAULT };
        let mut parts = mode.split('x');
        let (Some(width), Some(height), Some(fps)) = (parts.next(), parts.next(), parts.next()) else {
            return DEFAULT;
        };
        let (Ok(width), Ok(height), Ok(fps)) = (width.parse(), height.parse(), fps.parse()) else {
            return DEFAULT;
        };
        (width, height, fps)
    }

    fn unpair(&self, _params: &HashMap<String, String>) -> Response<Full<Bytes>> {
        // TODO: actually remove from the paired-clients store once ClientManager exposes that.
        xml_response(paired_xml(""))
    }

    // `LaunchHandler`'s methods are synchronous and genuinely block (process
    // spawning, D-Bus activation, and — for `launch` specifically — up to a
    // 15s condvar wait for a concurrent spawn to finish). Calling them
    // directly from an async handler blocks a tokio *worker* thread for that
    // whole duration; with enough concurrent `/launch` calls piling up (real
    // clients retry it on their own), that starves the entire small worker
    // pool and even trivial requests like `/serverinfo` stop being served at
    // all — confirmed live. `spawn_blocking` runs it on tokio's separate,
    // much larger blocking-thread pool instead.
    async fn launch(&self, params: &HashMap<String, String>, local_ip: std::net::IpAddr) -> Response<Full<Bytes>> {
        tracing::info!("HTTP /launch query parameters: {:?}", params);
        let (width, height, fps) = Self::parse_mode(params);

        let Some(rikey) = self.parse_rikey(params) else {
            return bad_request("missing/invalid rikey/rikeyid".to_string());
        };

        let handler = self.launch_handler.clone();
        let result = tokio::task::spawn_blocking(move || handler.launch(width, height, fps, rikey))
            .await
            .unwrap_or_else(|e| Err(format!("launch task panicked: {e}")));

        match result {
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

    async fn resume(&self, _params: &HashMap<String, String>, local_ip: std::net::IpAddr) -> Response<Full<Bytes>> {
        let handler = self.launch_handler.clone();
        let result = tokio::task::spawn_blocking(move || handler.resume())
            .await
            .unwrap_or_else(|e| Err(format!("resume task panicked: {e}")));

        match result {
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

    async fn cancel(&self, _params: &HashMap<String, String>) -> Response<Full<Bytes>> {
        let handler = self.launch_handler.clone();
        let result = tokio::task::spawn_blocking(move || handler.cancel())
            .await
            .unwrap_or_else(|e| Err(format!("cancel task panicked: {e}")));

        match result {
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

    /// Not part of the Moonlight protocol — a small tooling hook so
    /// `redfog-pair` (and anything else that wants to relay a PIN) can find
    /// out which `uniqueid` is actually waiting, one per line, instead of
    /// needing to already know it (e.g. from grepping server logs).
    fn pending_pairs(&self) -> Response<Full<Bytes>> {
        let ids = self.clients.pending_unique_ids().join("\n");
        Response::builder()
            .header("Content-Type", "text/plain")
            .body(Full::new(Bytes::from(ids)))
            .unwrap()
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Guards the actual bug found live: a real client sends a single
    /// combined `mode` param (`mode=1920x1080x30`), never separate
    /// `width`/`height`/`fps` keys — an earlier version of this parser
    /// looked for exactly those separate keys, which real requests never
    /// contain, so every real launch silently used the hardcoded default
    /// resolution regardless of what was actually requested.
    #[test]
    fn parses_real_client_mode_param() {
        let mut params = HashMap::new();
        params.insert("mode".to_string(), "1280x720x30".to_string());
        assert_eq!(PairingServer::parse_mode(&params), (1280, 720, 30));
    }

    #[test]
    fn falls_back_to_default_when_mode_is_missing() {
        let params = HashMap::new();
        assert_eq!(PairingServer::parse_mode(&params), (1920, 1080, 60));
    }

    #[test]
    fn falls_back_to_default_when_mode_is_malformed() {
        let mut params = HashMap::new();
        params.insert("mode".to_string(), "not-a-mode".to_string());
        assert_eq!(PairingServer::parse_mode(&params), (1920, 1080, 60));

        let mut params = HashMap::new();
        params.insert("mode".to_string(), "1280x720".to_string()); // missing fps
        assert_eq!(PairingServer::parse_mode(&params), (1920, 1080, 60));
    }

    /// The literal separate `width`/`height`/`fps` keys the old (buggy)
    /// parser looked for must NOT be picked up even if present — `mode` is
    /// the only real source of truth, matching moonlight-common-rust's own
    /// launch-request builder.
    #[test]
    fn ignores_separate_width_height_fps_keys() {
        let mut params = HashMap::new();
        params.insert("width".to_string(), "640".to_string());
        params.insert("height".to_string(), "480".to_string());
        params.insert("fps".to_string(), "15".to_string());
        assert_eq!(PairingServer::parse_mode(&params), (1920, 1080, 60));
    }
}
