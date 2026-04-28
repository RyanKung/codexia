use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::RngCore;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq)]
/// PKCE verifier and derived S256 challenge pair.
pub struct Pkce {
    /// Opaque high-entropy code verifier sent during token exchange.
    pub verifier: String,
    /// Base64url-encoded SHA-256 challenge derived from the verifier.
    pub challenge: String,
}

/// Generates a random PKCE verifier and its matching S256 code challenge.
#[must_use]
pub fn generate_pkce(rng: &mut impl RngCore) -> Pkce {
    let mut bytes = [0_u8; 32];
    rng.fill_bytes(&mut bytes);
    let verifier = URL_SAFE_NO_PAD.encode(bytes);
    let challenge = code_challenge(&verifier);

    Pkce {
        verifier,
        challenge,
    }
}

/// Computes the RFC 7636 S256 code challenge for a verifier.
#[must_use]
pub fn code_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(digest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{SeedableRng, rngs::StdRng};

    #[test]
    fn creates_url_safe_verifier_and_challenge() {
        let pkce = generate_pkce(&mut StdRng::seed_from_u64(7));

        assert_eq!(pkce.verifier.len(), 43);
        assert_eq!(pkce.challenge.len(), 43);
        assert!(!pkce.verifier.contains('='));
        assert!(!pkce.challenge.contains('='));
    }

    #[test]
    fn challenge_matches_rfc7636_example() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";

        assert_eq!(
            code_challenge(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }
}
