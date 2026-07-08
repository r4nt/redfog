//! Server identity: a persisted, self-signed RSA-2048 certificate.
//!
//! The pairing handshake RSA-signs data with this cert's private key
//! (`crypto::rsa_sign`) and embeds the cert's own ASN.1 signature bytes into
//! the hash chain (`crypto::cert_signature_bytes`), so the same cert must be
//! used across restarts or every paired client would need to re-pair.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rsa::pkcs8::{EncodePrivateKey, LineEnding};
use rsa::RsaPrivateKey;

pub struct ServerIdentity {
    pub cert_pem: String,
    pub private_key_pem: String,
    /// The `<uniqueid>` reported in `/serverinfo`. Real clients parse this as
    /// a UUID (not an arbitrary string like a hostname) and reject the
    /// response otherwise — confirmed by `moonlight-common-rust`'s
    /// `ServerInfoResponse::unique_id: Uuid` and caught by the integration
    /// test that uses it.
    pub unique_id: uuid::Uuid,
}

impl ServerIdentity {
    /// Load the persisted identity from `state_dir`, generating and saving a
    /// fresh one if none exists yet.
    pub fn load_or_create(state_dir: impl AsRef<Path>) -> Result<Self, String> {
        let state_dir = state_dir.as_ref();
        let cert_path = state_dir.join("server-cert.pem");
        let key_path = state_dir.join("server-key.pem");
        let uuid_path = state_dir.join("server-uuid.txt");

        if cert_path.exists() && key_path.exists() && uuid_path.exists() {
            let cert_pem = std::fs::read_to_string(&cert_path).map_err(|e| e.to_string())?;
            let private_key_pem = std::fs::read_to_string(&key_path).map_err(|e| e.to_string())?;
            let unique_id = std::fs::read_to_string(&uuid_path)
                .map_err(|e| e.to_string())?
                .trim()
                .parse()
                .map_err(|e| format!("invalid persisted server uuid: {e}"))?;
            return Ok(Self {
                cert_pem,
                private_key_pem,
                unique_id,
            });
        }

        std::fs::create_dir_all(state_dir).map_err(|e| e.to_string())?;
        let identity = Self::generate()?;
        std::fs::write(&cert_path, &identity.cert_pem).map_err(|e| e.to_string())?;
        std::fs::write(&key_path, &identity.private_key_pem).map_err(|e| e.to_string())?;
        std::fs::write(&uuid_path, identity.unique_id.to_string()).map_err(|e| e.to_string())?;
        Ok(identity)
    }

    /// Generate a fresh self-signed RSA-2048 identity (not persisted). Also
    /// useful for anything that needs "a self-signed cert+key" generically,
    /// e.g. simulating a client's own cert in integration tests.
    pub fn generate() -> Result<Self, String> {
        let mut rng = rand::thread_rng();
        let private_key = RsaPrivateKey::new(&mut rng, 2048).map_err(|e| format!("rsa keygen failed: {e}"))?;
        let private_key_pem = private_key
            .to_pkcs8_pem(LineEnding::LF)
            .map_err(|e| format!("failed to encode private key: {e}"))?
            .to_string();

        let key_der = private_key
            .to_pkcs8_der()
            .map_err(|e| format!("failed to der-encode private key: {e}"))?;
        let key_pair = rcgen::KeyPair::from_pkcs8_der_and_sign_algo(
            &rustls_pki_types::PrivatePkcs8KeyDer::from(key_der.as_bytes().to_vec()),
            &rcgen::PKCS_RSA_SHA256,
        )
        .map_err(|e| format!("failed to build rcgen key pair: {e}"))?;

        // A cert with only a CommonName and no subjectAltName is rejected
        // outright by many modern TLS stacks (SAN has been required since
        // ~2017). Cover localhost/loopback plus every local IP we can find,
        // since we don't know in advance which address a client will
        // actually connect through.
        let mut san = vec!["localhost".to_string(), "127.0.0.1".to_string()];
        if let Ok(hostname) = std::env::var("HOSTNAME").or_else(|_| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
                .ok_or(std::env::VarError::NotPresent)
        }) {
            if !hostname.is_empty() {
                san.push(hostname);
            }
        }
        // `hostname -I` isn't portable (busybox's `hostname` lacks it); `ip`
        // is standard on Linux and gives us every interface's IPv4 address.
        if let Ok(output) = std::process::Command::new("ip").args(["-4", "-o", "addr", "show"]).output() {
            for line in String::from_utf8_lossy(&output.stdout).lines() {
                if let Some(field) = line.split_whitespace().nth(3) {
                    if let Some(ip) = field.split('/').next() {
                        if ip != "127.0.0.1" {
                            san.push(ip.to_string());
                        }
                    }
                }
            }
        }

        let mut params =
            rcgen::CertificateParams::new(san).map_err(|e| format!("failed to build cert params: {e}"))?;
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "redfog");
        let cert = params
            .self_signed(&key_pair)
            .map_err(|e| format!("failed to self-sign cert: {e}"))?;

        Ok(Self {
            cert_pem: cert.pem(),
            private_key_pem,
            unique_id: uuid::Uuid::new_v4(),
        })
    }
}

pub fn default_state_dir() -> PathBuf {
    PathBuf::from(redfog_core::default_runtime_dir()).join("moonlight")
}

/// Requests (but doesn't require, and doesn't validate against any CA) a
/// client certificate on the HTTPS listener.
///
/// Real Moonlight clients present a self-signed cert with no shared trust
/// anchor — GameStream's actual trust model is "whatever cert came through
/// during PIN pairing, remembered forever", not a CA chain. Confirmed against
/// Wolf (Games on Whales)'s server, which does the same: it accepts any
/// client cert at the TLS layer, then separately checks whether *that
/// specific certificate* is on its paired-clients list before treating the
/// request as authenticated (`get_client_via_ssl` in
/// `state/config.hpp`) — a client's `uniqueid` string isn't trustworthy for
/// this on its own, since moonlight-qt reuses a shared placeholder uniqueid
/// for any server it doesn't detect as genuine Nvidia GFE software.
#[derive(Debug)]
pub struct AcceptAnyClientCert {
    schemes: Vec<rustls::SignatureScheme>,
}

impl AcceptAnyClientCert {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            schemes: rustls::crypto::aws_lc_rs::default_provider()
                .signature_verification_algorithms
                .supported_schemes(),
        })
    }
}

impl rustls::server::danger::ClientCertVerifier for AcceptAnyClientCert {
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        // Optional: `/pair` itself and the client's very first discovery
        // poll happen before it has any reason to present a cert tied to us.
        false
    }

    fn root_hint_subjects(&self) -> &[rustls::DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        _end_entity: &rustls_pki_types::CertificateDer<'_>,
        _intermediates: &[rustls_pki_types::CertificateDer<'_>],
        _now: rustls_pki_types::UnixTime,
    ) -> Result<rustls::server::danger::ClientCertVerified, rustls::Error> {
        Ok(rustls::server::danger::ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls_pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &rustls::crypto::aws_lc_rs::default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls_pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::aws_lc_rs::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.schemes.clone()
    }
}
