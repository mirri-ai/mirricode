//! PKCE (RFC 7636) code verifier / code challenge generation.
//!
//! The code verifier is a cryptographically random string of 43 characters
//! drawn from the RFC 7636 alphabet (`A-Z a-z 0-9 - . _ ~`) and the code
//! challenge is `BASE64URL(SHA256(code_verifier))` without padding.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};

use crate::oauth::error::{OAuthError, OAuthResult};

/// RFC 7636 minimum code verifier length.
pub const CODE_VERIFIER_MIN_LEN: usize = 43;
/// RFC 7636 maximum code verifier length.
pub const CODE_VERIFIER_MAX_LEN: usize = 128;
/// Random bytes backing one verifier / state value: 32 bytes encode to
/// 43 base64url characters, inside the RFC 7636 bounds.
pub const RANDOM_BYTES: usize = 32;

/// A PKCE pair: the verifier kept client-side and the challenge sent to the
/// authorization server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PkcePair {
    /// The plaintext verifier — sent only in the token exchange request.
    pub code_verifier: String,
    /// `BASE64URL(SHA256(code_verifier))` — sent in the authorization request.
    pub code_challenge: String,
    /// Challenge method; always `"S256"`.
    pub method: &'static str,
}

/// Generate a cryptographically random, URL-safe string of `RANDOM_BYTES`
/// bytes, base64url-encoded without padding.
pub fn random_url_safe_string() -> OAuthResult<String> {
    let mut bytes = vec![0u8; RANDOM_BYTES];
    getrandom::getrandom(&mut bytes).map_err(|error| OAuthError::Pkce(error.to_string()))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// Compute the RFC 7636 `S256` code challenge for a verifier.
pub fn code_challenge_for(code_verifier: &str) -> String {
    let digest = Sha256::digest(code_verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(digest)
}

/// Generate a fresh code verifier. The resulting string is 43 characters
/// long and drawn from the RFC 7636 alphabet (base64url is a strict subset).
pub fn generate_code_verifier() -> OAuthResult<String> {
    let verifier = random_url_safe_string()?;
    debug_assert!(
        (CODE_VERIFIER_MIN_LEN..=CODE_VERIFIER_MAX_LEN).contains(&verifier.len()),
        "generated code verifier must satisfy RFC 7636 length bounds"
    );
    Ok(verifier)
}

/// Generate a random `state` value for CSRF protection of the callback.
pub fn generate_state() -> OAuthResult<String> {
    random_url_safe_string()
}

/// Generate a complete PKCE pair.
pub fn generate_pkce_pair() -> OAuthResult<PkcePair> {
    let code_verifier = generate_code_verifier()?;
    Ok(PkcePair {
        code_challenge: code_challenge_for(&code_verifier),
        code_verifier,
        method: "S256",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_match_the_rfc7636_appendix_b_test_vector_when_computing_pkce_challenge() {
        // Test vector from RFC 7636 Appendix B.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            code_challenge_for(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn should_generate_a_verifier_within_the_rfc7636_length_bounds_when_generating_code_verifier() {
        let verifier = generate_code_verifier().unwrap();
        assert!(verifier.len() >= CODE_VERIFIER_MIN_LEN);
        assert!(verifier.len() <= CODE_VERIFIER_MAX_LEN);
        assert!(
            verifier
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '.' | '_' | '~'))
        );
    }

    #[test]
    fn should_generate_distinct_verifiers_and_states_when_generating_multiple_times() {
        let a = generate_code_verifier().unwrap();
        let b = generate_code_verifier().unwrap();
        assert_ne!(a, b);
        let s1 = generate_state().unwrap();
        let s2 = generate_state().unwrap();
        assert_ne!(s1, s2);
    }

    #[test]
    fn should_produce_a_challenge_of_43_characters_when_generating_pkce_pair() {
        let pair = generate_pkce_pair().unwrap();
        assert_eq!(pair.code_challenge.len(), 43);
        assert_eq!(pair.method, "S256");
        assert_eq!(pair.code_challenge, code_challenge_for(&pair.code_verifier));
    }
}
