//! The box identity the integration tests provision and the credentials the edge signs for it.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::LazyLock;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{Signer, SigningKey};
use serde_json::json;
use sha2::{Digest, Sha256};

const OWNER_TTL_SECS: i64 = 24 * 3600;

static OWNER: LazyLock<String> = LazyLock::new(|| {
    let key = edge_key();
    let exp = cocoonbox_agent::now_ms() / 1000 + OWNER_TTL_SECS;
    sign_credential(&key, "v1", &format!("{}|cb-box|w1|{exp}", key_id(&key)))
});

/// Writes the identity of box `cb-box` at wake `wake_id`, signed by `key`, to `path` and returns its key id.
pub fn write_identity(path: &Path, key: &SigningKey, wake_id: &str) -> String {
    let key_id = key_id(key);
    let dir = path.parent().unwrap();
    std::fs::create_dir_all(dir).unwrap();
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    let identity = json!({
        "box_id": "cb-box",
        "wake_id": wake_id,
        "key_id": key_id,
        "public_key": URL_SAFE_NO_PAD.encode(key.verifying_key().to_bytes()),
    });
    std::fs::write(dir.join("tmp"), identity.to_string()).unwrap();
    std::fs::set_permissions(dir.join("tmp"), std::fs::Permissions::from_mode(0o644)).unwrap();
    std::fs::rename(dir.join("tmp"), path).unwrap();
    key_id
}

/// Signs `fields` as a credential of `version`.
pub fn sign_credential(key: &SigningKey, version: &str, fields: &str) -> String {
    let signed = format!("{version}.{}", URL_SAFE_NO_PAD.encode(fields));
    let sig = URL_SAFE_NO_PAD.encode(key.sign(signed.as_bytes()).to_bytes());
    format!("{signed}.{sig}")
}

/// Provisions the box at `path` with the edge key at wake `w1`.
pub fn provision(path: &Path) {
    write_identity(path, &edge_key(), "w1");
}

/// The owner credential of a box that [`provision`] wrote.
pub fn owner() -> &'static str {
    &OWNER
}

fn edge_key() -> SigningKey {
    SigningKey::from_bytes(&[5; 32])
}

fn key_id(key: &SigningKey) -> String {
    hex::encode(Sha256::digest(key.verifying_key().to_bytes()))[..16].to_owned()
}
