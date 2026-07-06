//! Fake Moonlight client: performs the real 5-step `/pair` HTTP dance
//! against a running redfog-server, over actual sockets — not just the
//! in-process `ClientManager` unit test. Validates the HTTP/hex wire layer
//! that a real client actually exercises.
//!
//! Usage: cargo run --example fake_client_pair -- [host:port] [pin]

use redfog_moonlight::crypto;
use redfog_moonlight::tls::ServerIdentity;

fn get(url: &str) -> String {
    ureq::get(url).call().unwrap_or_else(|e| panic!("GET {url} failed: {e}")).into_string().unwrap()
}

fn xml_field(body: &str, tag: &str) -> String {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = body.find(&open).unwrap_or_else(|| panic!("missing <{tag}> in: {body}")) + open.len();
    let end = body[start..].find(&close).unwrap() + start;
    body[start..end].to_string()
}

fn main() {
    let mut args = std::env::args().skip(1);
    let base = args.next().unwrap_or_else(|| "127.0.0.1:47989".to_string());
    let pin = args.next().unwrap_or_else(|| "1234".to_string());
    let base_url = format!("http://{base}");

    let unique_id = format!("{:016X}", rand::random::<u64>());
    let client_identity = ServerIdentity::generate().expect("generate client identity");
    let mut salt = [0u8; 16];
    rand::Rng::fill(&mut rand::thread_rng(), &mut salt);

    println!("== step 1: getservercert (submitting PIN {pin} concurrently) ==");
    let submit_base = base_url.clone();
    let submit_id = unique_id.clone();
    let submit_pin = pin.clone();
    let submitter = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        let url = format!("{submit_base}/submit-pin?uniqueid={submit_id}&pin={submit_pin}");
        let resp = get(&url);
        println!("   /submit-pin -> {resp}");
    });

    let url = format!(
        "{base_url}/pair?phrase=getservercert&clientcert={}&salt={}&uniqueid={unique_id}",
        hex::encode(&client_identity.cert_pem),
        hex::encode(salt),
    );
    let body = get(&url);
    submitter.join().unwrap();
    let server_cert_pem = String::from_utf8(hex::decode(xml_field(&body, "plaincert")).unwrap()).unwrap();
    println!("   got server cert ({} bytes)", server_cert_pem.len());

    let key = crypto::derive_key(&salt, &pin);

    println!("== step 2: clientchallenge ==");
    let mut client_challenge = [0u8; 16];
    rand::Rng::fill(&mut rand::thread_rng(), &mut client_challenge);
    let encrypted_challenge = crypto::ecb_encrypt(&client_challenge, &key).unwrap();
    let url = format!("{base_url}/pair?clientchallenge={}&uniqueid={unique_id}", hex::encode(encrypted_challenge));
    let body = get(&url);
    let challenge_response = hex::decode(xml_field(&body, "challengeresponse")).unwrap();
    let decrypted = crypto::ecb_decrypt(&challenge_response, &key).unwrap();
    let (server_hash, server_challenge) = decrypted.split_at(32);
    let server_challenge: [u8; 16] = server_challenge.try_into().unwrap();
    println!("   decrypted challenge response ok ({} bytes)", decrypted.len());

    println!("== step 3: serverchallengeresp ==");
    let mut client_secret_payload = [0u8; 16];
    rand::Rng::fill(&mut rand::thread_rng(), &mut client_secret_payload);
    let client_cert_sig = crypto::cert_signature_bytes(&client_identity.cert_pem).unwrap();
    let mut commit_input = server_challenge.to_vec();
    commit_input.extend(&client_cert_sig);
    commit_input.extend(client_secret_payload);
    let commit_hash = sha256(&commit_input);
    let encrypted_commit = crypto::ecb_encrypt(&commit_hash, &key).unwrap();
    let url = format!("{base_url}/pair?serverchallengeresp={}&uniqueid={unique_id}", hex::encode(encrypted_commit));
    let body = get(&url);
    let pairing_secret = hex::decode(xml_field(&body, "pairingsecret")).unwrap();
    let (server_secret, server_secret_sig) = pairing_secret.split_at(16);
    crypto::rsa_verify(server_secret, server_secret_sig, &server_cert_pem).expect("server secret signature must verify");
    let server_cert_sig = crypto::cert_signature_bytes(&server_cert_pem).unwrap();
    let mut expected_hash_input = client_challenge.to_vec();
    expected_hash_input.extend(&server_cert_sig);
    expected_hash_input.extend(server_secret);
    assert_eq!(sha256(&expected_hash_input), server_hash, "server hash must match — proves it knew the PIN");
    println!("   server identity + PIN knowledge verified");

    println!("== step 4: pairchallenge ==");
    let _ = get(&format!("{base_url}/pair?phrase=pairchallenge&uniqueid={unique_id}"));

    println!("== step 5: clientpairingsecret ==");
    let client_secret_sig = crypto::rsa_sign(&client_secret_payload, &client_identity.private_key_pem).unwrap();
    let mut client_pairing_secret = client_secret_payload.to_vec();
    client_pairing_secret.extend(client_secret_sig);
    let body = get(&format!(
        "{base_url}/pair?clientpairingsecret={}&uniqueid={unique_id}",
        hex::encode(client_pairing_secret)
    ));
    println!("   response: {}", body.trim());

    println!("== verify: /serverinfo reports PairStatus=1 ==");
    let body = get(&format!("{base_url}/serverinfo?uniqueid={unique_id}"));
    let pair_status = xml_field(&body, "PairStatus");
    println!("   PairStatus = {pair_status}");
    assert_eq!(pair_status, "1", "server did not record this client as paired");

    println!("\nPAIRING SUCCEEDED over real HTTP.");
}

fn sha256(data: &[u8]) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    Sha256::digest(data).to_vec()
}
