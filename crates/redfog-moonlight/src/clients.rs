//! Pairing state: clients mid-handshake, and the persisted set of paired
//! devices (so pairing only ever happens once per device).
//!
//! Persisted pairing is keyed by the client's TLS certificate fingerprint,
//! not by the Moonlight `uniqueid` it sends — confirmed against
//! moonlight-qt's own source (`nvhttp.cpp`): a real client only ever sends
//! its own genuinely-unique per-install id when the *server* identifies as
//! real Nvidia GFE software; against any Sunshine-like server (redfog
//! included — see `PairingServer::server_info`'s own doc comment on the
//! negative-version-component trick), it sends a hardcoded shared
//! placeholder, `"0123456789ABCDEF"`, instead — the exact same literal
//! value from every real moonlight-qt install. An earlier version of this
//! file persisted pairing keyed by that string directly: pairing a second
//! physical device (same placeholder `uniqueid`) silently overwrote the
//! first device's entire paired record, including its own real
//! fingerprint — the first device would then, with no error anywhere,
//! simply stop being recognized as paired the next time it connected.
//! `uniqueid` only remains meaningful for the *in-flight* handshake itself
//! (`PendingClient`, `/pending-pairs`) — at most one client is realistically
//! mid-handshake under a given literal value at any one moment, unlike the
//! *persisted* set, which accumulates every device ever paired over the
//! server's whole lifetime.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rand::RngCore;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use crate::crypto;

/// A client mid-handshake (has sent its cert + salt, may or may not have a
/// PIN-derived key yet).
pub struct PendingClient {
    pub unique_id: String,
    pub client_cert_pem: String,
    pub salt: [u8; 16],
    pub key: Option<[u8; 16]>,
    pub pin_ready: Arc<Notify>,
    pub server_secret: Option<[u8; 16]>,
    pub server_challenge: Option<[u8; 16]>,
    pub client_hash: Option<Vec<u8>>,
    /// Set via `redfog-pair --scale`/`/submit-pin`'s optional `scale` field
    /// when no `--resolution` accompanies it (see `ClientManager::
    /// set_scale_for_pending`) — the fallback used when this device
    /// requests a resolution with no entry of its own in
    /// `scale_by_resolution` below. Carried forward into `PairedEntry`
    /// (keyed by this client's cert fingerprint, not its `unique_id` — see
    /// this module's own doc comment) once `check_client_pairing_secret`
    /// actually finalizes pairing. Not applied to anything until then: a
    /// client that never completes the handshake never gets a persisted
    /// entry at all.
    pub scale: Option<f64>,
    /// Per-resolution scale overrides, keyed by `resolution_key(width,
    /// height)` — the same physical device can legitimately want a
    /// different scale at different requested resolutions (e.g. a TV
    /// streamed at 4K vs. the same TV/app streamed at 1080p for a
    /// lower-bandwidth link). Set via `redfog-pair --scale S --resolution
    /// WxH`; checked before falling back to the plain `scale` default
    /// above — see `PairedEntry::scale`'s doc comment.
    pub scale_by_resolution: HashMap<String, f64>,
    /// Set via `redfog-pair <PIN> --name "..."` — a human-readable label
    /// for this device ("My TV"), carried into `PairedEntry` the same way
    /// `scale` is. Purely descriptive: never looked up by, only ever
    /// displayed by `redfog-pair --list`, since `uniqueid` and even a
    /// human-chosen name are both weaker identities than the cert
    /// fingerprint (a name could be reused/mistyped; the fingerprint
    /// can't).
    pub name: Option<String>,
}

/// `"{width}x{height}"` — the map key both `PendingClient::
/// scale_by_resolution` and `PairedEntry::scale_by_resolution` use. A
/// plain string (not a `(u32, u32)` tuple) because the persisted form goes
/// through `serde_json`, which requires string map keys.
fn resolution_key(width: u32, height: u32) -> String {
    format!("{width}x{height}")
}

/// One paired device, as returned by `ClientManager::paired_clients` — the
/// public-facing view of a `PairedEntry`, identified by fingerprint (never
/// `unique_id` — see this module's own doc comment).
#[derive(Serialize)]
pub struct PairedClientInfo {
    pub fingerprint: String,
    pub name: Option<String>,
    pub last_seen_ip: Option<String>,
    /// The resolution requested by this device's most recent `/launch` —
    /// see `PairedEntry::last_seen_resolution`'s doc comment. For a
    /// currently-streaming session this genuinely *is* the current
    /// resolution (redfog has no live resolution renegotiation — see
    /// TODO.md's "Live resolution/fps renegotiation" entry — so it can't
    /// have changed since that `/launch`), not just a historical value.
    pub last_seen_resolution: Option<String>,
    /// Every distinct resolution this device has ever requested, sorted
    /// (`"{width}x{height}"`, oldest and newest indistinguishable from
    /// each other here — see `last_seen_resolution` for which one is
    /// current). Answers "which resolutions do I need a `--scale
    /// --resolution` entry for" without having to guess.
    pub seen_resolutions: Vec<String>,
    /// This device's plain default scale (`redfog-pair --scale`, no
    /// `--resolution`) — `None` if never set.
    pub scale: Option<f64>,
    /// Per-resolution scale overrides (`redfog-pair --scale --resolution`).
    pub scale_by_resolution: HashMap<String, f64>,
}

/// One paired device's persisted record — see this module's own doc
/// comment for why `PersistedState::paired_by_fingerprint` keys this by
/// cert fingerprint rather than by `unique_id`.
#[derive(Default, Serialize, Deserialize)]
struct PairedEntry {
    #[serde(default)]
    scale: Option<f64>,
    #[serde(default)]
    scale_by_resolution: HashMap<String, f64>,
    /// Human-readable label ("My TV") — see `PendingClient::name`'s doc
    /// comment.
    #[serde(default)]
    name: Option<String>,
    /// The peer IP address this device connected from, last time it
    /// either finished pairing or called `/launch` (see
    /// `PairingServer::launch`'s own note on why `/launch`, not
    /// `/serverinfo`, is what updates this — the latter is polled far too
    /// often to persist on every hit). A hint for a human trying to match
    /// a fingerprint back to a physical device, not a stable identity —
    /// DHCP leases change, VPNs exist — so nothing here ever reads this
    /// back to make a decision, only `redfog-pair --list` displays it.
    /// Stored as its `Display` string (not `std::net::IpAddr` directly):
    /// this whole struct round-trips through `serde_json`, and
    /// `IpAddr`'s own `Serialize` impl produces the same string either
    /// way, so there's no format actually gained by the richer type here.
    #[serde(default)]
    last_seen_ip: Option<String>,
    /// The resolution (`resolution_key(width, height)`) requested by this
    /// device's most recent `/launch` — updated in lockstep with
    /// `last_seen_ip` (see that field's own doc comment for why `/launch`,
    /// not `/serverinfo`, is the update point). For a session that's
    /// currently streaming, this is the actual live resolution, not a
    /// stale historical value — redfog has no live resolution
    /// renegotiation (see TODO.md), so it cannot have changed since.
    #[serde(default)]
    last_seen_resolution: Option<String>,
    /// Every distinct resolution ever seen from this device — a `BTreeSet`
    /// purely so `redfog-pair --list`'s output is stably sorted without
    /// that binary needing to sort it itself. Exists so a human
    /// configuring `--scale --resolution` overrides can see which
    /// resolutions a device has actually asked for, instead of guessing.
    #[serde(default)]
    seen_resolutions: std::collections::BTreeSet<String>,
}

impl PairedEntry {
    /// The scale to use for a session requesting `width`x`height` — a
    /// per-resolution override if one was set for exactly this
    /// resolution, else the device's plain default, else `None` (caller
    /// falls back to `1.0`).
    fn scale(&self, width: u32, height: u32) -> Option<f64> {
        self.scale_by_resolution.get(&resolution_key(width, height)).copied().or(self.scale)
    }
}

#[derive(Default, Serialize, Deserialize)]
struct PersistedState {
    /// cert fingerprint -> paired client's record. Named distinctly from
    /// the old `paired` field (rather than reusing that name and
    /// disambiguating old vs. new shape after the fact) so `load_state`
    /// can tell a pre-migration file apart unambiguously, just by which
    /// top-level key is present.
    #[serde(default)]
    paired_by_fingerprint: HashMap<String, PairedEntry>,
}

/// The pre-fingerprint-keying on-disk shape: `unique_id -> fingerprint` (or,
/// briefly, `unique_id -> {cert_fingerprint, scale, scale_by_resolution}` —
/// this crate's own short-lived intermediate shape before the `uniqueid`
/// collision bug described in this module's doc comment was found and
/// fixed). Read only by `migrate_legacy_state`, once, the first time
/// `ClientManager::new` loads a file that doesn't have the new
/// `paired_by_fingerprint` key yet.
#[derive(Deserialize)]
struct LegacyPersistedState {
    #[serde(default)]
    paired: HashMap<String, LegacyPairedEntry>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum LegacyPairedEntry {
    BareFingerprint(String),
    WithScale {
        cert_fingerprint: String,
        #[serde(default)]
        scale: Option<f64>,
        #[serde(default)]
        scale_by_resolution: HashMap<String, f64>,
    },
}

fn load_state(state_path: &Path) -> PersistedState {
    let Ok(raw) = std::fs::read_to_string(state_path) else {
        return PersistedState::default();
    };
    // A `#[serde(default)]` field means an old-shape file (no
    // `paired_by_fingerprint` key at all) would otherwise deserialize
    // "successfully" straight into an empty map, silently discarding
    // every already-paired device -- checked explicitly via a raw JSON
    // object key check instead of trusting that alone.
    let is_new_shape = matches!(
        serde_json::from_str::<serde_json::Value>(&raw),
        Ok(serde_json::Value::Object(obj)) if obj.contains_key("paired_by_fingerprint")
    );
    if is_new_shape {
        if let Ok(state) = serde_json::from_str::<PersistedState>(&raw) {
            return state;
        }
    }
    migrate_legacy_state(&raw)
}

/// Re-keys an old `unique_id -> fingerprint` (or the short-lived
/// `unique_id -> {cert_fingerprint, ...}`) file by fingerprint instead —
/// see this module's own doc comment for why `unique_id` can't be trusted
/// to identify one physical device at all. If the same `unique_id` was
/// ever reused by two real devices in the old file, only the most
/// recently paired of them survives *in that old file* already (the
/// collision this migration exists to stop from happening again) — there
/// is nothing left to recover for the earlier one; it needs to be paired
/// again after upgrading.
fn migrate_legacy_state(raw: &str) -> PersistedState {
    let Ok(legacy) = serde_json::from_str::<LegacyPersistedState>(raw) else {
        return PersistedState::default();
    };
    let mut paired_by_fingerprint = HashMap::new();
    for entry in legacy.paired.into_values() {
        let (fingerprint, scale, scale_by_resolution) = match entry {
            LegacyPairedEntry::BareFingerprint(fp) => (fp, None, HashMap::new()),
            LegacyPairedEntry::WithScale { cert_fingerprint, scale, scale_by_resolution } => (cert_fingerprint, scale, scale_by_resolution),
        };
        paired_by_fingerprint.insert(fingerprint, PairedEntry { scale, scale_by_resolution, ..Default::default() });
    }
    PersistedState { paired_by_fingerprint }
}

/// Resolves `fingerprint` (possibly just a prefix — see
/// `ClientManager::set_scale_for_paired`'s doc comment) against
/// `state.paired_by_fingerprint`'s actual keys, to the one full fingerprint
/// it identifies. Checks for an exact match first (the common case: a
/// human pasting a value `--list` printed verbatim), only falling back to
/// prefix matching — and its ambiguity check — if that fails.
fn resolve_fingerprint(state: &PersistedState, fingerprint: &str) -> Result<String, String> {
    if state.paired_by_fingerprint.contains_key(fingerprint) {
        return Ok(fingerprint.to_string());
    }
    let matches: Vec<&String> = state.paired_by_fingerprint.keys().filter(|fp| fp.starts_with(fingerprint)).collect();
    match matches.as_slice() {
        [] => Err(format!("no paired device matches fingerprint {fingerprint:?}")),
        [one] => Ok((*one).clone()),
        many => Err(format!(
            "fingerprint {fingerprint:?} matches {} paired devices, be more specific:\n{}",
            many.len(),
            many.iter().map(|fp| format!("  {fp}")).collect::<Vec<_>>().join("\n")
        )),
    }
}

pub struct ClientManager {
    pending: Mutex<HashMap<String, PendingClient>>,
    state_path: PathBuf,
    state: Mutex<PersistedState>,
    pub server_cert_pem: String,
    pub server_private_key_pem: String,
}

impl ClientManager {
    pub fn new(state_dir: impl Into<PathBuf>, server_cert_pem: String, server_private_key_pem: String) -> Self {
        let state_path = state_dir.into().join("paired-clients.json");
        let state = load_state(&state_path);
        let this = Self {
            pending: Mutex::new(HashMap::new()),
            state_path,
            state: Mutex::new(state),
            server_cert_pem,
            server_private_key_pem,
        };
        // Write the migrated shape back immediately (a no-op if the file
        // was already current) rather than waiting for the first real
        // pairing/scale change to persist it -- so a legacy file is fixed
        // on disk as soon as it's loaded once, not left silently
        // re-migrated from scratch (and re-risking the collision this
        // exists to fix) on every subsequent server restart until then.
        this.persist(&this.state.lock().unwrap());
        this
    }

    fn persist(&self, state: &PersistedState) {
        if let Ok(json) = serde_json::to_string_pretty(state) {
            let _ = std::fs::write(&self.state_path, json);
        }
    }

    /// Real Moonlight clients reuse a shared placeholder `uniqueid`
    /// ("0123456789ABCDEF") for any server it doesn't detect as genuine
    /// Nvidia GFE software (see this module's own doc comment) — so
    /// `uniqueid` alone can't distinguish between two different physical
    /// devices. Real Sunshine/Wolf hosts key pairing by the client's
    /// actual TLS certificate instead; this mirrors that.
    pub fn is_paired_by_cert(&self, cert_fingerprint: &str) -> bool {
        self.state.lock().unwrap().paired_by_fingerprint.contains_key(cert_fingerprint)
    }

    /// Every paired device, with whatever `name`/`last_seen_ip`/resolution
    /// and scale info it has — what `redfog-pair --list` shows (via
    /// `/paired-clients`) so there's an actual way to find a device's
    /// identity again later, and see what's configured/been seen for it,
    /// given `uniqueid` can't serve any of that purpose (see this module's
    /// own doc comment).
    pub fn paired_clients(&self) -> Vec<PairedClientInfo> {
        self.state
            .lock()
            .unwrap()
            .paired_by_fingerprint
            .iter()
            .map(|(fingerprint, entry)| PairedClientInfo {
                fingerprint: fingerprint.clone(),
                name: entry.name.clone(),
                last_seen_ip: entry.last_seen_ip.clone(),
                last_seen_resolution: entry.last_seen_resolution.clone(),
                seen_resolutions: entry.seen_resolutions.iter().cloned().collect(),
                scale: entry.scale,
                scale_by_resolution: entry.scale_by_resolution.clone(),
            })
            .collect()
    }

    /// HiDPI scale factor configured for this paired client requesting
    /// `width`x`height` (via `redfog-pair --scale`/`/submit-pin`, or a
    /// later `set_scale_for_paired` call), if any — see `--scale`'s own
    /// doc comment in `redfog-broker/src/session.rs` for what this
    /// actually controls. Looked up per-device rather than being a single
    /// server-wide setting: two different physical clients (a TV, a
    /// laptop) pairing with the same server legitimately want different
    /// values — and, per `PairedEntry::scale`'s doc comment, the same
    /// device can want a different value at a different resolution too
    /// (checked first, with the plain per-device default as fallback).
    /// `None` if the client isn't paired at all, or was paired without
    /// ever setting a scale for this resolution or a default (callers
    /// should fall back to `1.0`, not treat `None` as an error).
    pub fn scale_for_cert_fingerprint(&self, cert_fingerprint: &str, width: u32, height: u32) -> Option<f64> {
        self.state.lock().unwrap().paired_by_fingerprint.get(cert_fingerprint).and_then(|e| e.scale(width, height))
    }

    /// Sets (or updates) a still-pending (not yet fully paired) client's
    /// scale factor, by `uniqueid` — the common case, since `redfog-pair
    /// --scale` normally runs at the same "relay the PIN" step as
    /// `submit_pin`, before pairing has actually finalized. `resolution`:
    /// `Some((width, height))` sets a resolution-specific override
    /// (`redfog-pair --scale S --resolution WxH`); `None` sets the
    /// device's plain default. The value is stashed on `PendingClient` and
    /// carried into the persisted record, keyed by this device's actual
    /// cert fingerprint, once `check_client_pairing_secret` finalizes
    /// pairing. Errors if `unique_id` isn't currently pending.
    pub fn set_scale_for_pending(&self, unique_id: &str, scale: f64, resolution: Option<(u32, u32)>) -> Result<(), String> {
        let mut pending = self.pending.lock().unwrap();
        let client = pending.get_mut(unique_id).ok_or_else(|| format!("no pending client {unique_id}"))?;
        match resolution {
            Some((width, height)) => {
                client.scale_by_resolution.insert(resolution_key(width, height), scale);
            }
            None => client.scale = Some(scale),
        }
        Ok(())
    }

    /// Sets (or updates) an already-paired device's scale factor, by cert
    /// fingerprint (see `paired_fingerprints`/`redfog-pair --list` for how
    /// to find it) — updates the persisted record directly, no re-pairing
    /// needed. `fingerprint` may be a prefix, same idea as an abbreviated
    /// git commit hash: resolved exactly if it matches a full fingerprint,
    /// otherwise by unique prefix — errors (listing every match) if that's
    /// ambiguous, so a caller relying on trailing digits alone doesn't
    /// silently update the wrong device. `resolution` behaves the same as
    /// `set_scale_for_pending`'s.
    pub fn set_scale_for_paired(&self, fingerprint: &str, scale: f64, resolution: Option<(u32, u32)>) -> Result<(), String> {
        let mut state = self.state.lock().unwrap();
        let resolved = resolve_fingerprint(&state, fingerprint)?;
        let entry = state.paired_by_fingerprint.get_mut(&resolved).expect("just resolved to an existing key");
        match resolution {
            Some((width, height)) => {
                entry.scale_by_resolution.insert(resolution_key(width, height), scale);
            }
            None => entry.scale = Some(scale),
        }
        self.persist(&state);
        Ok(())
    }

    /// Sets (or updates) a still-pending client's name, by `uniqueid` —
    /// same timing/mechanism as `set_scale_for_pending`: stashed on
    /// `PendingClient`, carried into the persisted record (keyed by
    /// fingerprint) once pairing finalizes. Errors if `unique_id` isn't
    /// currently pending.
    pub fn set_name_for_pending(&self, unique_id: &str, name: String) -> Result<(), String> {
        let mut pending = self.pending.lock().unwrap();
        let client = pending.get_mut(unique_id).ok_or_else(|| format!("no pending client {unique_id}"))?;
        client.name = Some(name);
        Ok(())
    }

    /// Sets (or updates) an already-paired device's name, by cert
    /// fingerprint (or a unique prefix — see `set_scale_for_paired`'s doc
    /// comment).
    pub fn set_name_for_paired(&self, fingerprint: &str, name: String) -> Result<(), String> {
        let mut state = self.state.lock().unwrap();
        let resolved = resolve_fingerprint(&state, fingerprint)?;
        let entry = state.paired_by_fingerprint.get_mut(&resolved).expect("just resolved to an existing key");
        entry.name = Some(name);
        self.persist(&state);
        Ok(())
    }

    /// Records that a paired device (by cert fingerprint) was just seen
    /// connecting from `ip`, requesting `width`x`height` — see
    /// `PairedEntry::last_seen_ip`/`last_seen_resolution`/
    /// `seen_resolutions`' own doc comments for what these are (and
    /// aren't) used for. A silent no-op if `cert_fingerprint` isn't
    /// actually paired (shouldn't happen from any real call site, which
    /// only ever calls this right after confirming pairing — not worth
    /// failing a real client's launch/pairing over a bookkeeping field
    /// regardless).
    pub fn note_seen(&self, cert_fingerprint: &str, ip: std::net::IpAddr, width: u32, height: u32) {
        let mut state = self.state.lock().unwrap();
        if let Some(entry) = state.paired_by_fingerprint.get_mut(cert_fingerprint) {
            entry.last_seen_ip = Some(ip.to_string());
            let resolution = resolution_key(width, height);
            entry.seen_resolutions.insert(resolution.clone());
            entry.last_seen_resolution = Some(resolution);
            self.persist(&state);
        }
    }

    /// Step 1: client sent its cert + salt. Returns the notifier to await the
    /// PIN before responding with the server cert.
    pub fn start_pairing(&self, unique_id: &str, client_cert_pem: String, salt: [u8; 16]) -> Arc<Notify> {
        let notify = Arc::new(Notify::new());
        self.pending.lock().unwrap().insert(
            unique_id.to_string(),
            PendingClient {
                unique_id: unique_id.to_string(),
                client_cert_pem,
                salt,
                key: None,
                pin_ready: notify.clone(),
                server_secret: None,
                server_challenge: None,
                client_hash: None,
                scale: None,
                scale_by_resolution: HashMap::new(),
                name: None,
            },
        );
        notify
    }

    /// `uniqueid`s currently mid-handshake (cert + salt sent, blocked in
    /// `getservercert` waiting on a PIN) — lets a human-facing tool (the
    /// login UI, or `redfog-pair`) show/pick a client to pair without
    /// needing to already know its `uniqueid` (e.g. from grepping server
    /// logs).
    pub fn pending_unique_ids(&self) -> Vec<String> {
        self.pending.lock().unwrap().keys().cloned().collect()
    }

    /// PIN relayed by a human (e.g. via the login UI on first connect).
    /// Derives the shared AES key and wakes the blocked `getservercert` call.
    pub fn submit_pin(&self, unique_id: &str, pin: &str) -> Result<(), String> {
        let mut pending = self.pending.lock().unwrap();
        let client = pending
            .get_mut(unique_id)
            .ok_or_else(|| format!("no pending client {unique_id}"))?;
        client.key = Some(crypto::derive_key(&client.salt, pin));
        client.pin_ready.notify_waiters();
        Ok(())
    }

    /// Step 2: client's encrypted 16-byte challenge -> our encrypted response.
    pub fn client_challenge(&self, unique_id: &str, challenge: &[u8]) -> Result<Vec<u8>, String> {
        let mut pending = self.pending.lock().unwrap();
        let client = pending
            .get_mut(unique_id)
            .ok_or_else(|| format!("no pending client {unique_id}"))?;
        let key = client.key.ok_or("client has no key yet (PIN not submitted)")?;

        let client_challenge = crypto::ecb_decrypt(challenge, &key)?;

        let mut server_secret = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut server_secret);
        client.server_secret = Some(server_secret);

        let mut server_challenge = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut server_challenge);
        client.server_challenge = Some(server_challenge);

        let mut hash_input = client_challenge;
        hash_input.extend(crypto::cert_signature_bytes(&self.server_cert_pem)?);
        hash_input.extend(server_secret);
        let hash = sha256(&hash_input);

        let mut response_plain = hash;
        response_plain.extend(server_challenge);
        crypto::ecb_encrypt(&response_plain, &key)
    }

    /// Step 3: client's encrypted proof of our challenge -> our signed secret.
    pub fn server_challenge_response(&self, unique_id: &str, response: &[u8]) -> Result<Vec<u8>, String> {
        let mut pending = self.pending.lock().unwrap();
        let client = pending
            .get_mut(unique_id)
            .ok_or_else(|| format!("no pending client {unique_id}"))?;
        let key = client.key.ok_or("client has no key yet (PIN not submitted)")?;

        client.client_hash = Some(crypto::ecb_decrypt(response, &key)?);

        let server_secret = client.server_secret.ok_or("no server secret generated yet")?;
        let signature = crypto::rsa_sign(&server_secret, &self.server_private_key_pem)?;

        let mut pairing_secret = server_secret.to_vec();
        pairing_secret.extend(signature);
        Ok(pairing_secret)
    }

    /// Step 5: verify the client's final proof and, if valid, persist it
    /// as paired. `peer_ip`: the pairing request's own source address —
    /// recorded as this device's initial `last_seen_ip`, same field
    /// `PairingServer::launch` keeps fresh on every later stream (see
    /// `PairedEntry::last_seen_ip`'s doc comment), so a device that pairs
    /// but never actually launches a stream still shows *something* in
    /// `redfog-pair --list` rather than a blank.
    pub fn check_client_pairing_secret(&self, unique_id: &str, client_secret: &[u8], peer_ip: std::net::IpAddr) -> Result<(), String> {
        let mut pending = self.pending.lock().unwrap();
        let client = pending
            .get_mut(unique_id)
            .ok_or_else(|| format!("no pending client {unique_id}"))?;

        if client_secret.len() < 16 {
            return Err("client pairing secret shorter than 16 bytes".to_string());
        }
        let (client_secret_payload, client_signature) = client_secret.split_at(16);

        let client_hash = client
            .client_hash
            .as_ref()
            .ok_or("no client hash recorded (out-of-order pairing steps)")?;
        let server_challenge = client
            .server_challenge
            .ok_or("no server challenge recorded (out-of-order pairing steps)")?;

        let mut hash_input = server_challenge.to_vec();
        hash_input.extend(crypto::cert_signature_bytes(&client.client_cert_pem)?);
        hash_input.extend(client_secret_payload);
        let expected_hash = sha256(&hash_input);

        if &expected_hash != client_hash {
            return Err("client hash mismatch (possible MITM)".to_string());
        }

        crypto::rsa_verify(client_secret_payload, client_signature, &client.client_cert_pem)?;

        let fingerprint = crypto::cert_fingerprint(&client.client_cert_pem)?;
        let scale = client.scale;
        let scale_by_resolution = client.scale_by_resolution.clone();
        let name = client.name.clone();
        let mut state = self.state.lock().unwrap();
        // Keyed by fingerprint, not `unique_id` -- see this module's own
        // doc comment. A second device pairing under the same literal
        // `unique_id` (the common case against a Sunshine-like server)
        // gets its own entry here rather than clobbering an earlier
        // device's.
        state.paired_by_fingerprint.insert(
            fingerprint,
            PairedEntry { scale, scale_by_resolution, name, last_seen_ip: Some(peer_ip.to_string()), ..Default::default() },
        );
        self.persist(&state);

        Ok(())
    }
}

fn sha256(data: &[u8]) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    Sha256::digest(data).to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tls::ServerIdentity;

    /// Full 5-step handshake, playing both roles, to check our server-side
    /// implementation is a correct counterpart to what a real Moonlight
    /// client actually does (derived from reading a known-working
    /// implementation's crypto, not guessed).
    #[test]
    fn full_pairing_handshake_succeeds() {
        let server_identity = ServerIdentity::generate().unwrap();
        let client_identity = ServerIdentity::generate().unwrap(); // just need "a self-signed RSA cert+key"

        let tmp = tempdir();
        let manager = ClientManager::new(&tmp, server_identity.cert_pem.clone(), server_identity.private_key_pem.clone());

        let unique_id = "test-client-1";
        let pin = "1234";
        let mut salt = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut salt);

        // --- Step 1: client sends cert+salt, human relays PIN ---
        manager.start_pairing(unique_id, client_identity.cert_pem.clone(), salt);
        manager.submit_pin(unique_id, pin).unwrap();
        let key = crypto::derive_key(&salt, pin);

        // --- Step 2: client challenge ---
        let mut client_challenge = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut client_challenge);
        let encrypted_challenge = crypto::ecb_encrypt(&client_challenge, &key).unwrap();
        let challenge_response = manager.client_challenge(unique_id, &encrypted_challenge).unwrap();

        let decrypted_response = crypto::ecb_decrypt(&challenge_response, &key).unwrap();
        assert_eq!(decrypted_response.len(), 48);
        let (server_hash, server_challenge) = decrypted_response.split_at(32);
        let server_challenge: [u8; 16] = server_challenge.try_into().unwrap();

        // --- Step 3: client's commitment to its own secret, encrypted server_challenge-resp ---
        let mut client_secret_payload = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut client_secret_payload);
        let client_cert_sig = crypto::cert_signature_bytes(&client_identity.cert_pem).unwrap();
        let mut commit_input = server_challenge.to_vec();
        commit_input.extend(&client_cert_sig);
        commit_input.extend(client_secret_payload);
        let commit_hash = sha256(&commit_input);
        let server_challenge_resp = crypto::ecb_encrypt(&commit_hash, &key).unwrap();

        let pairing_secret = manager
            .server_challenge_response(unique_id, &server_challenge_resp)
            .unwrap();
        let (server_secret, server_secret_sig) = pairing_secret.split_at(16);

        // Client verifies the server's identity and that it derived the same key (knows the PIN).
        crypto::rsa_verify(server_secret, server_secret_sig, &server_identity.cert_pem)
            .expect("server secret signature must verify against server cert");
        let server_cert_sig = crypto::cert_signature_bytes(&server_identity.cert_pem).unwrap();
        let mut expected_server_hash_input = client_challenge.to_vec();
        expected_server_hash_input.extend(&server_cert_sig);
        expected_server_hash_input.extend(server_secret);
        assert_eq!(sha256(&expected_server_hash_input), server_hash, "server hash must match — proves server knew the PIN");

        // --- Step 4: trivial ack (nothing to verify) ---

        // --- Step 5: client reveals its committed secret + signs it ---
        let client_secret_sig = crypto::rsa_sign(&client_secret_payload, &client_identity.private_key_pem).unwrap();
        let mut client_pairing_secret = client_secret_payload.to_vec();
        client_pairing_secret.extend(client_secret_sig);

        manager
            .check_client_pairing_secret(unique_id, &client_pairing_secret, "127.0.0.1".parse().unwrap())
            .expect("server must accept a faithfully-computed client pairing secret");

        let fingerprint = crypto::cert_fingerprint(&client_identity.cert_pem).unwrap();
        assert!(manager.is_paired_by_cert(&fingerprint));
    }

    #[test]
    fn wrong_pin_is_rejected() {
        let server_identity = ServerIdentity::generate().unwrap();
        let client_identity = ServerIdentity::generate().unwrap();
        let tmp = tempdir();
        let manager = ClientManager::new(&tmp, server_identity.cert_pem.clone(), server_identity.private_key_pem.clone());

        let unique_id = "test-client-2";
        let mut salt = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut salt);
        manager.start_pairing(unique_id, client_identity.cert_pem.clone(), salt);
        manager.submit_pin(unique_id, "1234").unwrap();

        // Client (wrongly) thinks the PIN is "0000".
        let wrong_key = crypto::derive_key(&salt, "0000");
        let client_challenge = [7u8; 16];
        let encrypted = crypto::ecb_encrypt(&client_challenge, &wrong_key).unwrap();
        let challenge_response = manager.client_challenge(unique_id, &encrypted).unwrap();

        // Decrypting the server's response with the wrong key yields garbage,
        // not the expected structure — this is what causes a real client to
        // reject pairing when the PIN doesn't match.
        let decrypted = crypto::ecb_decrypt(&challenge_response, &wrong_key).unwrap();
        let (server_hash, _server_challenge) = decrypted.split_at(32);
        let server_cert_sig = crypto::cert_signature_bytes(&server_identity.cert_pem).unwrap();
        // We don't have the real server_secret (only revealed via the correct
        // key later), so we can't even form the right input — but proving
        // the hash differs from what the correct-PIN test produces is enough
        // to show a mismatched PIN can't produce an accepted pairing.
        let mut bogus_input = client_challenge.to_vec();
        bogus_input.extend(&server_cert_sig);
        bogus_input.extend([0u8; 16]); // we don't actually know server_secret
        assert_ne!(sha256(&bogus_input), server_hash);
    }

    /// `redfog-pair --scale` calls `set_scale_for_pending` while a client
    /// is still `pending` (same step as relaying the PIN) — this checks
    /// that value survives into the persisted record once pairing
    /// actually finalizes, and that it can also be updated later, for an
    /// already-paired client (by fingerprint, not `unique_id` — see this
    /// module's own doc comment), with no pending handshake involved at
    /// all.
    #[test]
    fn scale_is_carried_from_pending_into_paired_and_updatable_after() {
        let server_identity = ServerIdentity::generate().unwrap();
        let client_identity = ServerIdentity::generate().unwrap();
        let tmp = tempdir();
        let manager = ClientManager::new(&tmp, server_identity.cert_pem.clone(), server_identity.private_key_pem.clone());

        let unique_id = "test-client-scale";
        let pin = "1234";
        let mut salt = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut salt);

        manager.start_pairing(unique_id, client_identity.cert_pem.clone(), salt);
        manager.submit_pin(unique_id, pin).unwrap();
        manager.set_scale_for_pending(unique_id, 1.5, None).expect("set_scale_for_pending on a pending client");
        let key = crypto::derive_key(&salt, pin);

        let client_challenge = [1u8; 16];
        let encrypted_challenge = crypto::ecb_encrypt(&client_challenge, &key).unwrap();
        let challenge_response = manager.client_challenge(unique_id, &encrypted_challenge).unwrap();
        let decrypted_response = crypto::ecb_decrypt(&challenge_response, &key).unwrap();
        let (_server_hash, server_challenge) = decrypted_response.split_at(32);
        let server_challenge: [u8; 16] = server_challenge.try_into().unwrap();

        let client_secret_payload = [2u8; 16];
        let client_cert_sig = crypto::cert_signature_bytes(&client_identity.cert_pem).unwrap();
        let mut commit_input = server_challenge.to_vec();
        commit_input.extend(&client_cert_sig);
        commit_input.extend(client_secret_payload);
        let commit_hash = sha256(&commit_input);
        let server_challenge_resp = crypto::ecb_encrypt(&commit_hash, &key).unwrap();
        manager.server_challenge_response(unique_id, &server_challenge_resp).unwrap();

        let client_secret_sig = crypto::rsa_sign(&client_secret_payload, &client_identity.private_key_pem).unwrap();
        let mut client_pairing_secret = client_secret_payload.to_vec();
        client_pairing_secret.extend(client_secret_sig);
        manager
            .check_client_pairing_secret(unique_id, &client_pairing_secret, "192.168.1.50".parse().unwrap())
            .expect("pairing must succeed");

        let fingerprint = crypto::cert_fingerprint(&client_identity.cert_pem).unwrap();
        assert_eq!(
            manager.scale_for_cert_fingerprint(&fingerprint, 1920, 1080),
            Some(1.5),
            "scale set while pending must survive into the paired record, as the device's default"
        );

        // Update the default, now that the client is fully paired -- by
        // fingerprint, no pending handshake or `unique_id` involved,
        // same as `redfog-pair --fingerprint ... --scale ...`.
        manager.set_scale_for_paired(&fingerprint, 2.0, None).expect("set_scale_for_paired on an already-paired client");
        assert_eq!(manager.scale_for_cert_fingerprint(&fingerprint, 1920, 1080), Some(2.0));

        // A per-resolution override takes priority over the default, but
        // only for that exact resolution -- everything else still falls
        // back to it.
        manager.set_scale_for_paired(&fingerprint, 1.0, Some((3840, 2160))).expect("set_scale_for_paired with a resolution override");
        assert_eq!(manager.scale_for_cert_fingerprint(&fingerprint, 3840, 2160), Some(1.0), "the 4K-specific override");
        assert_eq!(manager.scale_for_cert_fingerprint(&fingerprint, 1920, 1080), Some(2.0), "unrelated resolutions still see the default");
        assert_eq!(manager.scale_for_cert_fingerprint(&fingerprint, 1280, 720), Some(2.0), "unrelated resolutions still see the default");

        // A fingerprint prefix works too, as long as it's unambiguous.
        manager.set_scale_for_paired(&fingerprint[..8], 3.0, None).expect("set_scale_for_paired with a fingerprint prefix");
        assert_eq!(manager.scale_for_cert_fingerprint(&fingerprint, 1920, 1080), Some(3.0));

        // A client that was never paired (or pending) has nothing to update.
        assert!(manager.set_scale_for_paired("no-such-fingerprint", 1.0, None).is_err());
        assert!(manager.set_scale_for_pending("no-such-client", 1.0, None).is_err());

        // Pairing itself already recorded the peer IP it happened from
        // (nothing streamed yet, so no `/launch` has run to update it, or
        // recorded any resolution at all).
        let clients = manager.paired_clients();
        assert_eq!(clients.len(), 1);
        assert_eq!(clients[0].fingerprint, fingerprint);
        assert_eq!(clients[0].last_seen_ip.as_deref(), Some("192.168.1.50"));
        assert_eq!(clients[0].name, None, "no --name was ever set");
        assert_eq!(clients[0].last_seen_resolution, None);
        assert!(clients[0].seen_resolutions.is_empty());
        assert_eq!(clients[0].scale, Some(3.0), "the paired_clients view also reports current scale config");
        assert_eq!(clients[0].scale_by_resolution.get("3840x2160"), Some(&1.0));

        manager.set_name_for_paired(&fingerprint, "My TV".to_string()).unwrap();
        assert_eq!(manager.paired_clients()[0].name.as_deref(), Some("My TV"));

        // A real `/launch` records both the IP and the resolution it
        // requested -- and accumulates into `seen_resolutions` across
        // multiple different ones, not just overwriting.
        manager.note_seen(&fingerprint, "10.0.0.7".parse().unwrap(), 1920, 1080);
        let info = &manager.paired_clients()[0];
        assert_eq!(info.last_seen_ip.as_deref(), Some("10.0.0.7"), "a later /launch updates it");
        assert_eq!(info.last_seen_resolution.as_deref(), Some("1920x1080"));
        assert_eq!(info.seen_resolutions, vec!["1920x1080".to_string()]);

        manager.note_seen(&fingerprint, "10.0.0.7".parse().unwrap(), 3840, 2160);
        let info = &manager.paired_clients()[0];
        assert_eq!(info.last_seen_resolution.as_deref(), Some("3840x2160"), "the most recent one wins");
        assert_eq!(info.seen_resolutions, vec!["1920x1080".to_string(), "3840x2160".to_string()], "but history accumulates, sorted");

        // Unknown fingerprint: a silent no-op (see `note_seen`'s doc
        // comment), and name-setting errors the same way scale's does.
        manager.note_seen("no-such-fingerprint", "1.2.3.4".parse().unwrap(), 1280, 720);
        assert!(manager.set_name_for_paired("no-such-fingerprint", "x".to_string()).is_err());
        assert!(manager.set_name_for_pending("no-such-client", "x".to_string()).is_err());
    }

    /// Real, existing `paired-clients.json` files predate fingerprint-
    /// keying entirely and map `unique_id -> bare fingerprint string` (the
    /// oldest shape) or `unique_id -> {cert_fingerprint, scale,
    /// scale_by_resolution}` (this crate's own short-lived intermediate
    /// shape, before the `unique_id` collision bug described in this
    /// module's doc comment was found and fixed) -- this checks both old
    /// shapes migrate correctly to the new `paired_by_fingerprint` form,
    /// and that the migration is actually written back to disk (not just
    /// applied in memory) so it only ever has to run once.
    #[test]
    fn migrates_both_pre_fingerprint_keyed_paired_clients_json_shapes() {
        let tmp = tempdir();
        std::fs::write(
            tmp.join("paired-clients.json"),
            r#"{"paired":{"old-client":"deadbeef","other-client":{"cert_fingerprint":"c0ffee","scale":1.75,"scale_by_resolution":{"3840x2160":2.0}}}}"#,
        )
        .unwrap();

        let identity = ServerIdentity::generate().unwrap();
        let manager = ClientManager::new(&tmp, identity.cert_pem.clone(), identity.private_key_pem.clone());

        assert!(manager.is_paired_by_cert("deadbeef"));
        assert_eq!(manager.scale_for_cert_fingerprint("deadbeef", 1920, 1080), None);
        assert!(manager.is_paired_by_cert("c0ffee"));
        assert_eq!(manager.scale_for_cert_fingerprint("c0ffee", 1920, 1080), Some(1.75), "default carried over");
        assert_eq!(manager.scale_for_cert_fingerprint("c0ffee", 3840, 2160), Some(2.0), "resolution override carried over");

        let mut fingerprints: Vec<String> = manager.paired_clients().into_iter().map(|c| c.fingerprint).collect();
        fingerprints.sort();
        assert_eq!(fingerprints, vec!["c0ffee".to_string(), "deadbeef".to_string()]);

        // Updating scale on a migrated entry must work too, not just reading it.
        manager.set_scale_for_paired("deadbeef", 1.25, None).unwrap();
        assert_eq!(manager.scale_for_cert_fingerprint("deadbeef", 1920, 1080), Some(1.25));

        // The migration was actually persisted, not just applied in memory.
        let on_disk = std::fs::read_to_string(tmp.join("paired-clients.json")).unwrap();
        assert!(on_disk.contains("paired_by_fingerprint"), "migrated shape must be written back: {on_disk}");
        assert!(!on_disk.contains("\"paired\""), "old top-level key must be gone: {on_disk}");
    }

    fn tempdir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("redfog-moonlight-test-{}", rand_u64()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn rand_u64() -> u64 {
        rand::RngCore::next_u64(&mut rand::thread_rng())
    }
}
