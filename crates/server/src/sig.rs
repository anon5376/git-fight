//! Webhook HMAC. Verify the raw body before JSON parse.

use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;

type HmacSha256 = Hmac<Sha256>;

pub fn signature_header(secret: &[u8], body: &[u8]) -> String {
    format!("sha256={}", hex::encode(hmac_sha256(secret, body)))
}

/// Constant-time check of `X-Hub-Signature-256` (`sha256=` + hex).
/// Missing, malformed, or mismatched headers return false. The HMAC is
/// always computed so a missing header is not a fast path.
pub fn verify_signature(secret: &[u8], body: &[u8], header: Option<&str>) -> bool {
    let computed = hmac_sha256(secret, body);
    let mut claimed = [0u8; 32];
    let parsed = parse_header(header, &mut claimed);
    let eq = computed.ct_eq(&claimed);
    parsed && bool::from(eq)
}

fn hmac_sha256(secret: &[u8], body: &[u8]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(secret).expect("hmac-sha256 accepts any key length");
    mac.update(body);
    let bytes = mac.finalize().into_bytes();
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    out
}

fn parse_header(header: Option<&str>, out: &mut [u8; 32]) -> bool {
    let Some(header) = header else {
        return false;
    };
    let Some(hex) = header.strip_prefix("sha256=") else {
        return false;
    };
    if hex.len() != 64 {
        return false;
    }
    hex::decode_to_slice(hex.as_bytes(), out).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_header_accepts() {
        let secret = b"it's a secret";
        let body = b"{\"ok\":true}";
        let mac = hmac_sha256(secret, body);
        let header = format!("sha256={}", hex::encode(mac));
        assert!(verify_signature(secret, body, Some(&header)));
    }

    #[test]
    fn invalid_header_rejects() {
        let secret = b"it's a secret";
        let body = b"{\"ok\":true}";
        let header = "sha256=0000000000000000000000000000000000000000000000000000000000000000";
        assert!(!verify_signature(secret, body, Some(header)));
    }

    #[test]
    fn missing_header_rejects() {
        assert!(!verify_signature(b"secret", b"body", None));
        assert!(!verify_signature(b"secret", b"body", Some("")));
        assert!(!verify_signature(b"secret", b"body", Some("sha1=abc")));
    }

    #[test]
    fn hmac_does_not_fall_back_to_a_zero_key() {
        let body = b"payload";
        let real = hmac_sha256(b"webhook-secret", body);
        let zero = hmac_sha256(&[0u8; 32], body);
        assert_ne!(real, zero);
        let header = format!("sha256={}", hex::encode(real));
        assert!(verify_signature(b"webhook-secret", body, Some(&header)));
        assert!(!verify_signature(&[0u8; 32], body, Some(&header)));
    }
}
