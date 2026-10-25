//! A byte-compatible port of itsdangerous' `URLSafeTimedSerializer`.
//!
//! Tokens issued by the earlier Python server verify here, and tokens issued
//! here are valid itsdangerous tokens too, so the switch kept Claude's
//! connection (and the browser session cookie) alive. Format: `payload.timestamp.signature`,
//! each part unpadded URL-safe base64:
//!   * payload — compact JSON, optionally zlib-compressed and then prefixed
//!     with `.` (we read both, and write uncompressed, which Python accepts);
//!   * timestamp — big-endian seconds since the epoch, leading zeros stripped;
//!   * signature — HMAC-SHA1 over `payload.timestamp` with the key
//!     `SHA1(salt + "signer" + secret)` (itsdangerous' "django-concat").

use std::time::{SystemTime, UNIX_EPOCH};

use base64::{
    Engine,
    engine::{GeneralPurpose, GeneralPurposeConfig, DecodePaddingMode, general_purpose::URL_SAFE_NO_PAD},
};
use ring::{digest, hmac};
use serde_json::Value;

/// Decoder that, like Python's `urlsafe_b64decode` after itsdangerous re-pads,
/// tolerates leftover non-zero trailing bits.
const URL_SAFE_LENIENT: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::URL_SAFE,
    GeneralPurposeConfig::new()
        .with_decode_padding_mode(DecodePaddingMode::Indifferent)
        .with_decode_allow_trailing_bits(true),
);

pub struct Serializer {
    key: hmac::Key,
}

impl Serializer {
    pub fn new(secret: &str, salt: &str) -> Self {
        let mut material = Vec::with_capacity(salt.len() + 6 + secret.len());
        material.extend_from_slice(salt.as_bytes());
        material.extend_from_slice(b"signer");
        material.extend_from_slice(secret.as_bytes());
        let derived = digest::digest(&digest::SHA1_FOR_LEGACY_USE_ONLY, &material);
        Self {
            key: hmac::Key::new(hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, derived.as_ref()),
        }
    }

    pub fn dumps(&self, value: &Value) -> String {
        self.dumps_at(value, now())
    }

    fn dumps_at(&self, value: &Value, timestamp: u64) -> String {
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(value).expect("JSON value"));
        let ts_bytes = timestamp.to_be_bytes();
        let first = ts_bytes.iter().position(|&b| b != 0).unwrap_or(ts_bytes.len());
        let signed = format!("{payload}.{}", URL_SAFE_NO_PAD.encode(&ts_bytes[first..]));
        let sig = hmac::sign(&self.key, signed.as_bytes());
        format!("{signed}.{}", URL_SAFE_NO_PAD.encode(sig.as_ref()))
    }

    /// Verify signature and age, then decode the payload. Any failure — bad
    /// signature, malformed or future timestamp, expiry, bad payload — is `None`.
    pub fn loads(&self, token: &str, max_age: u64) -> Option<Value> {
        self.loads_at(token, max_age, now())
    }

    fn loads_at(&self, token: &str, max_age: u64, now: u64) -> Option<Value> {
        let (signed, sig) = token.rsplit_once('.')?;
        let sig = URL_SAFE_LENIENT.decode(sig).ok()?;
        hmac::verify(&self.key, signed.as_bytes(), &sig).ok()?;

        let (payload, ts) = signed.rsplit_once('.')?;
        let ts = URL_SAFE_LENIENT.decode(ts).ok()?;
        if ts.len() > 8 {
            return None;
        }
        let mut be = [0u8; 8];
        be[8 - ts.len()..].copy_from_slice(&ts);
        let ts = u64::from_be_bytes(be);
        let age = now as i128 - ts as i128;
        if age > max_age as i128 || age < 0 {
            return None;
        }

        let json = match payload.strip_prefix('.') {
            Some(compressed) => {
                let raw = URL_SAFE_LENIENT.decode(compressed).ok()?;
                miniz_oxide::inflate::decompress_to_vec_zlib(&raw).ok()?
            }
            None => URL_SAFE_LENIENT.decode(payload).ok()?,
        };
        serde_json::from_slice(&json).ok()
    }
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Vectors from itsdangerous 2.2.0:
    //   URLSafeTimedSerializer("secret", salt=...).dumps(...) at t=1790000000
    const T: u64 = 1_790_000_000;

    #[test]
    fn reads_python_tokens() {
        let token = Serializer::new("secret", "hevy-mcp-token");
        let python = "eyJjbGllbnRfaWQiOiJoZXZ5LWNsYXVkZS1jb25uZWN0b3IiLCJ0eXBlIjoibWNwX2FjY2Vzc190b2tlbiJ9.arE7gA.vyIvD5Kxo5NzZzqnUUuY00GqA9E";
        assert_eq!(
            token.loads_at(python, 3600, T + 10),
            Some(json!({"client_id": "hevy-claude-connector", "type": "mcp_access_token"}))
        );
        // zlib-compressed payload (itsdangerous compresses when it saves bytes).
        let compressed = ".eJyrVkrOyUzNK4nPTFGyUqogEijpKJVUFqQCdeQmF8QnJienFhfHl-Rnp-Yp1QIAfZQhZw.arE7gA.Qedpp6x8C7rxpyK1Nw8CKXPfN5c";
        assert_eq!(
            token.loads_at(compressed, 3600, T),
            Some(json!({"client_id": "x".repeat(40), "type": "mcp_access_token"}))
        );
        let session = Serializer::new("secret", "hevy-mcp-session");
        assert_eq!(
            session.loads_at("Im93bmVyIg.arE7gA.uh-VBAvh-N-zqma2rHD588mir94", 60, T),
            Some(json!("owner"))
        );
    }

    #[test]
    fn writes_python_tokens() {
        let token = Serializer::new("secret", "hevy-mcp-token");
        assert_eq!(
            token.dumps_at(&json!({"client_id": "hevy-claude-connector", "type": "mcp_access_token"}), T),
            "eyJjbGllbnRfaWQiOiJoZXZ5LWNsYXVkZS1jb25uZWN0b3IiLCJ0eXBlIjoibWNwX2FjY2Vzc190b2tlbiJ9.arE7gA.vyIvD5Kxo5NzZzqnUUuY00GqA9E"
        );
    }

    #[test]
    fn rejects_tampering_expiry_and_cross_salt() {
        let token = Serializer::new("secret", "hevy-mcp-token");
        let session = Serializer::new("secret", "hevy-mcp-session");
        let t = token.dumps_at(&json!({"a": 1}), T);
        assert!(token.loads_at(&t, 100, T + 100).is_some());
        assert!(token.loads_at(&t, 100, T + 101).is_none(), "expired");
        assert!(token.loads_at(&t, 100, T - 1).is_none(), "future timestamp");
        assert!(session.loads_at(&t, 100, T).is_none(), "other salt");
        assert!(Serializer::new("other", "hevy-mcp-token").loads_at(&t, 100, T).is_none());
        let mut forged = t.clone();
        forged.replace_range(2..3, if &t[2..3] == "A" { "B" } else { "A" });
        assert!(token.loads_at(&forged, 100, T).is_none());
        assert!(token.loads_at("garbage", 100, T).is_none());
        assert!(token.loads_at("", 100, T).is_none());
    }
}
