//! Webhook signing.
//!
//! Header: `Dodo-Signature: t=<unix seconds>,v1=<hex HMAC-SHA256>`, where the
//! MAC is over `"<t>.<raw request body>"` keyed by the endpoint's secret.
//!
//! Binding the timestamp into the MAC is the replay protection: a receiver
//! rejects signatures older than a tolerance (we recommend 5 minutes), and an
//! attacker cannot refresh `t` without the secret. The event id in the body
//! lets receivers drop duplicates, since delivery is at-least-once.

use hmac::{Hmac, Mac};
use sha2::Sha256;

pub const HEADER: &str = "dodo-signature";
pub const RECOMMENDED_TOLERANCE_SECS: i64 = 300;

type HmacSha256 = Hmac<Sha256>;

pub fn sign(secret: &str, timestamp: i64, body: &[u8]) -> String {
    format!("t={timestamp},v1={}", hex::encode(mac(secret, timestamp, body).finalize().into_bytes()))
}

/// Reference receiver-side verification, the logic we would ship in SDKs.
pub fn verify(secret: &str, header: &str, body: &[u8], now: i64, tolerance_secs: i64) -> bool {
    let mut timestamp = None;
    let mut signatures = Vec::new();
    for part in header.split(',') {
        match part.split_once('=') {
            Some(("t", value)) => timestamp = value.parse::<i64>().ok(),
            Some(("v1", value)) => signatures.extend(hex::decode(value).ok()),
            _ => {}
        }
    }
    let Some(timestamp) = timestamp else { return false };
    if (now - timestamp).abs() > tolerance_secs {
        return false;
    }
    // `verify_slice` is constant-time. Several v1 values are allowed so a
    // secret rotation can sign with old and new keys during the overlap.
    signatures.iter().any(|signature| mac(secret, timestamp, body).verify_slice(signature).is_ok())
}

fn mac(secret: &str, timestamp: i64, body: &[u8]) -> HmacSha256 {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts keys of any length");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    mac
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "whsec_test";
    const BODY: &[u8] = br#"{"id":"evt_1"}"#;

    #[test]
    fn a_fresh_signature_verifies() {
        let header = sign(SECRET, 1_700_000_000, BODY);
        assert!(verify(SECRET, &header, BODY, 1_700_000_010, RECOMMENDED_TOLERANCE_SECS));
    }

    #[test]
    fn tampered_body_wrong_secret_and_stale_timestamp_are_rejected() {
        let header = sign(SECRET, 1_700_000_000, BODY);
        assert!(!verify(SECRET, &header, br#"{"id":"evt_2"}"#, 1_700_000_000, 300));
        assert!(!verify("whsec_other", &header, BODY, 1_700_000_000, 300));
        assert!(!verify(SECRET, &header, BODY, 1_700_000_000 + 301, 300));
    }

    #[test]
    fn timestamp_cannot_be_swapped_without_the_secret() {
        let header = sign(SECRET, 1_700_000_000, BODY);
        let replayed = header.replace("t=1700000000", "t=1700009999");
        assert!(!verify(SECRET, &replayed, BODY, 1_700_009_999, 300));
    }
}
