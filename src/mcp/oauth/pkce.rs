//! PKCE (RFC 7636) with `S256`, and the random `state`.
//!
//! MCP 2026-07-28, authorization/security-considerations.md,
//! "Authorization Code Protection": clients MUST implement PKCE and use
//! `S256`.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::rand::{SecureRandom, SystemRandom};

/// `bytes` random bytes as unpadded base64url text (RFC 7636 section 4.1:
/// 32 bytes make a 43-character verifier of unreserved characters).
pub fn random_text(bytes: usize) -> Result<String, String> {
    let mut buffer = vec![0u8; bytes];
    SystemRandom::new()
        .fill(&mut buffer)
        .map_err(|_| "the system's random number source failed".to_string())?;
    Ok(URL_SAFE_NO_PAD.encode(buffer))
}

/// A code verifier and its `S256` challenge. Not `Debug`: the verifier
/// redeems the code.
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    pub fn new() -> Result<Self, String> {
        let verifier = random_text(32)?;
        Ok(Self {
            challenge: challenge(&verifier),
            verifier,
        })
    }
}

/// `BASE64URL(SHA256(verifier))`.
pub fn challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(ring::digest::digest(
        &ring::digest::SHA256,
        verifier.as_bytes(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_rfc_example() {
        // RFC 7636 appendix B.
        assert_eq!(
            challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn verifiers_are_long_unreserved_and_new_each_time() {
        let one = Pkce::new().unwrap();
        let two = Pkce::new().unwrap();
        assert_eq!(one.verifier.len(), 43);
        assert!(
            one.verifier
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        );
        assert_ne!(one.verifier, two.verifier);
        assert_eq!(one.challenge, challenge(&one.verifier));
        assert_ne!(one.challenge, one.verifier);
    }
}
