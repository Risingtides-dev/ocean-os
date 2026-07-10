//! PKCE (RFC 7636) code verifier/challenge and CSRF state generation.
//!
//! Mirrors OMP's `registry/oauth/pkce.ts`: 96 random bytes → base64url verifier
//! (128 chars), SHA-256 challenge base64url, 16 random bytes → lowercase-hex
//! state (32 chars).

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use sha2::{Digest, Sha256};

pub(crate) struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

const VERIFIER_BYTES: usize = 96;
const STATE_BYTES: usize = 16;

/// Generate a fresh verifier/challenge pair.
pub(crate) fn generate() -> Pkce {
    let verifier = random_base64url(VERIFIER_BYTES);
    let challenge = challenge(&verifier);
    Pkce {
        verifier,
        challenge,
    }
}

/// Compute the S256 challenge for an existing verifier (exposed for the known
/// test vector).
pub(crate) fn challenge(verifier: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(hasher.finalize())
}

/// Generate a 32-char lowercase-hex CSRF state token.
pub(crate) fn generate_state() -> String {
    let mut bytes = [0u8; STATE_BYTES];
    fill_random(&mut bytes);
    let mut out = String::with_capacity(STATE_BYTES * 2);
    for byte in &bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn random_base64url(n: usize) -> String {
    let mut bytes = vec![0u8; n];
    fill_random(&mut bytes);
    URL_SAFE_NO_PAD.encode(&bytes)
}

fn fill_random(buf: &mut [u8]) {
    // getrandom 0.3 exposes `fill`; panicking here is correct — the OS CSPRNG
    // being unavailable is unrecoverable for an OAuth login.
    getrandom::fill(buf).expect("getrandom: failed to obtain random bytes");
}

#[cfg(test)]
mod tests {
    use super::{challenge, generate, generate_state};
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    use sha2::{Digest, Sha256};

    #[test]
    fn verifier_is_128_char_base64url() {
        // 96 random bytes -> base64url (URL_SAFE_NO_PAD) is exactly 128 chars
        // and never contains '+', '/', or '='.
        let pkce = generate();
        let verifier = &pkce.verifier;
        assert_eq!(verifier.len(), 128, "verifier must be 128 base64url chars");
        assert!(
            verifier
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "verifier must contain only base64url chars (no +/=): {verifier}"
        );
        assert!(
            URL_SAFE_NO_PAD.decode(verifier).is_ok(),
            "not valid base64url"
        );
    }

    #[test]
    fn challenge_matches_hand_computed_s256_vector() {
        // FIXED verifier -> deterministic challenge, recomputed here with the
        // same crates but an independent code path from production.
        let verifier = "fixed-verifier-for-the-known-s256-test-vector-0000000000000000000000";
        let mut hasher = Sha256::new();
        hasher.update(verifier.as_bytes());
        let expected = URL_SAFE_NO_PAD.encode(hasher.finalize());
        assert_eq!(challenge(verifier), expected);
        // sha256 is 32 bytes -> 43 base64url chars (no padding).
        assert_eq!(expected.len(), 43);
    }

    #[test]
    fn generated_challenge_is_consistent_with_its_verifier() {
        // generate()'s challenge must equal an independent sha256(verifier).
        for _ in 0..16 {
            let pkce = generate();
            let mut hasher = Sha256::new();
            hasher.update(pkce.verifier.as_bytes());
            assert_eq!(
                pkce.challenge,
                URL_SAFE_NO_PAD.encode(hasher.finalize()),
                "challenge inconsistent with verifier {}",
                pkce.verifier
            );
        }
    }

    #[test]
    fn generated_pairs_are_unique() {
        let a = generate();
        let b = generate();
        assert_ne!(a.verifier, b.verifier);
        assert_ne!(a.challenge, b.challenge);
    }

    #[test]
    fn state_is_32_char_lowercase_hex() {
        for _ in 0..32 {
            let state = generate_state();
            assert_eq!(state.len(), 32, "state must be 32 hex chars: {state}");
            assert!(
                state
                    .bytes()
                    .all(|b| (b'a'..=b'f').contains(&b) || b.is_ascii_digit()),
                "state must be lowercase hex: {state}"
            );
        }
    }

    #[test]
    fn states_are_unique() {
        assert_ne!(generate_state(), generate_state());
    }
}
