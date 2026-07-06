//! Integration test using the real reference client implementation
//! (`moonlight-common-rust`, GPL-3.0-or-later, dev-only) rather than our own
//! hand-rolled crypto — this exercises the exact client-side logic a real
//! Moonlight app runs, so it validates our server against reality instead of
//! just against our own (possibly-biased) reimplementation of both sides.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;

use moonlight_common::crypto::rustcrypto::RustCryptoBackend;
use moonlight_common::high::tokio::MoonlightHost;
use moonlight_common::http::client::tokio_hyper::TokioHyperClient;
use moonlight_common::http::pair::PairPin;
use moonlight_common::http::{ClientIdentifier, ClientSecret};

use redfog_moonlight::clients::ClientManager;
use redfog_moonlight::pairing::{NoopLaunchHandler, PairingServer};
use redfog_moonlight::tls::ServerIdentity;

fn pick_free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_client_pairs_and_lists_apps() {
    // Our own rustls usage (via rcgen etc.) and moonlight-common's rustls
    // usage (via hyper-rustls) pull in different crypto provider backends
    // (ring vs aws-lc-rs); rustls can't auto-pick one when both are linked
    // into the same test binary, so install one explicitly.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let _ = tracing_subscriber::fmt().with_env_filter("redfog_moonlight=debug").try_init();

    let tmp = std::env::temp_dir().join(format!("redfog-it-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&tmp).unwrap();

    let identity = ServerIdentity::generate().expect("generate server identity");
    let clients = Arc::new(ClientManager::new(&tmp, identity.cert_pem.clone(), identity.private_key_pem.clone()));

    let http_port = pick_free_port();
    let https_port = pick_free_port();

    let server = Arc::new(PairingServer {
        clients,
        identity,
        hostname: "test-server".to_string(),
        http_port,
        https_port,
        rtsp_port: pick_free_port(),
        launch_handler: Arc::new(NoopLaunchHandler),
    });

    let bind_addr = IpAddr::V4(Ipv4Addr::LOCALHOST);
    tokio::spawn({
        let server = server.clone();
        async move {
            let _ = server.serve_http(bind_addr).await;
        }
    });
    tokio::spawn({
        let server = server.clone();
        async move {
            let _ = server.serve_https(bind_addr).await;
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // A fake client identity — a real Moonlight client generates its own
    // self-signed cert+key the same way; reuse our own generator for that.
    let client_identity = ServerIdentity::generate().expect("generate client identity");
    let client_identifier = ClientIdentifier::from_pem(pem::parse(&client_identity.cert_pem).unwrap());
    let client_secret = ClientSecret::from_pem(pem::parse(&client_identity.private_key_pem).unwrap());

    let host = MoonlightHost::<TokioHyperClient>::new("127.0.0.1".to_string(), http_port, Some("test-client".to_string()))
        .expect("construct MoonlightHost");

    let pin = PairPin::new_random(&RustCryptoBackend).expect("generate pin");
    let pin_str = pin.to_string();

    // Submit the PIN concurrently, same as a human relaying it in real usage
    // (our /pair handler blocks on getservercert until this arrives).
    let submit_task = tokio::task::spawn_blocking(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        ureq::post(&format!("http://127.0.0.1:{http_port}/submit-pin"))
            .send_form(&[("uniqueid", "test-client"), ("pin", &pin_str)])
            .expect("submit-pin request");
    });

    host.pair(&client_identifier, &client_secret, "integration-test".to_string(), pin, RustCryptoBackend)
        .await
        .expect("real client pairing must succeed against our server");
    submit_task.await.unwrap();

    assert!(host.is_paired().await.expect("is_paired"), "server must report the client as paired");

    let apps = host.app_list().await.expect("app_list");
    assert!(apps.iter().any(|a| a.title == "Desktop"), "applist must contain the Desktop entry, got: {apps:?}");
}
