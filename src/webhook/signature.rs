//! Request signatures: `sha256=<hex>` of the HMAC-SHA256 of the body with
//! the route's secret, in `X-Hub-Signature-256` (GitHub) or
//! `X-Junees-Signature` (generic). The HMAC is `ring`'s (already in the
//! build through rustls), whose `verify` compares in constant time.

use ring::hmac;

/// The header GitHub signs with.
pub const GITHUB_HEADER: &str = "x-hub-signature-256";
/// The header of generic routes.
pub const GENERIC_HEADER: &str = "x-junees-signature";

/// `sha256=<hex>` of `body` with `secret`.
pub fn sign(secret: &[u8], body: &[u8]) -> String {
    let key = hmac::Key::new(hmac::HMAC_SHA256, secret);
    let tag = hmac::sign(&key, body);
    let mut out = String::from("sha256=");
    for byte in tag.as_ref() {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Why a request's signature is not accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    Missing,
    Malformed,
    Wrong,
}

impl Refused {
    pub fn message(self) -> &'static str {
        match self {
            Self::Missing => "missing signature",
            Self::Malformed => "malformed signature (expected sha256=<64 hex digits>)",
            Self::Wrong => "wrong signature",
        }
    }
}

/// Checks `header` (the signature header's value, if any) against `body`
/// signed with `secret`.
pub fn verify(secret: &[u8], body: &[u8], header: Option<&str>) -> Result<(), Refused> {
    let header = header.map(str::trim).ok_or(Refused::Missing)?;
    let hex = header.strip_prefix("sha256=").ok_or(Refused::Malformed)?;
    let tag = decode_hex(hex).ok_or(Refused::Malformed)?;
    if tag.len() != 32 {
        return Err(Refused::Malformed);
    }
    let key = hmac::Key::new(hmac::HMAC_SHA256, secret);
    hmac::verify(&key, body, &tag).map_err(|_| Refused::Wrong)
}

fn decode_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    let digit = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    text.as_bytes()
        .chunks(2)
        .map(|pair| Some(digit(pair[0])? << 4 | digit(pair[1])?))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(hex: &str) -> Vec<u8> {
        decode_hex(hex).unwrap()
    }

    #[test]
    fn matches_the_rfc_4231_vectors() {
        // RFC 4231 test cases 1, 2, 3, 4, 6 and 7 (5 truncates the tag).
        let cases: [(Vec<u8>, Vec<u8>, &str); 6] = [
            (
                vec![0x0b; 20],
                b"Hi There".to_vec(),
                "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7",
            ),
            (
                b"Jefe".to_vec(),
                b"what do ya want for nothing?".to_vec(),
                "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843",
            ),
            (
                vec![0xaa; 20],
                vec![0xdd; 50],
                "773ea91e36800e46854db8ebd09181a72959098b3ef8c122d9635514ced565fe",
            ),
            (
                bytes("0102030405060708090a0b0c0d0e0f10111213141516171819"),
                vec![0xcd; 50],
                "82558a389a443c0ea4cc819899f2083a85f0faa3e578f8077a2e3ff46729665b",
            ),
            (
                vec![0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First".to_vec(),
                "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54",
            ),
            (
                vec![0xaa; 131],
                b"This is a test using a larger than block-size key and a larger than \
                  block-size data. The key needs to be hashed before being used by the HMAC \
                  algorithm."
                    .to_vec(),
                "9b09ffa71b942fcb27635fbcd5b0e944bfdc63644f0713938a7f51535c3a35e2",
            ),
        ];
        for (key, data, expected) in cases {
            assert_eq!(sign(&key, &data), format!("sha256={expected}"));
            assert_eq!(
                verify(&key, &data, Some(&format!("sha256={expected}"))),
                Ok(())
            );
        }
    }

    #[test]
    fn matches_githubs_documented_example() {
        // docs.github.com, "Validating webhook deliveries", test values.
        assert_eq!(
            sign(b"It's a Secret to Everybody", b"Hello, World!"),
            "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17"
        );
    }

    #[test]
    fn refuses_missing_malformed_and_wrong_signatures() {
        let body = br#"{"action":"opened"}"#;
        let good = sign(b"secret", body);
        assert_eq!(verify(b"secret", body, Some(&good)), Ok(()));
        // Upper-case hex and surrounding blanks are fine.
        let upper = format!("sha256={}", good[7..].to_uppercase());
        assert_eq!(verify(b"secret", body, Some(&format!(" {upper} "))), Ok(()));
        assert_eq!(verify(b"secret", body, None), Err(Refused::Missing));
        assert_eq!(
            verify(b"other", body, Some(&good)),
            Err(Refused::Wrong),
            "another secret"
        );
        assert_eq!(
            verify(b"secret", br#"{"action":"closed"}"#, Some(&good)),
            Err(Refused::Wrong),
            "another body"
        );
        for bad in [
            "",
            "sha1=abcd",
            &good[7..],
            &good[..good.len() - 2],
            &format!("{good}00"),
            "sha256=zz",
            "sha256=7571",
            "sha256=é5",
        ] {
            assert_eq!(
                verify(b"secret", body, Some(bad)),
                Err(Refused::Malformed),
                "{bad}"
            );
        }
    }
}
