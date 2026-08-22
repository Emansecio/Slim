use std::fmt;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use sha2::{Digest, Sha256};

use super::OAuthError;

#[derive(Clone, Eq, PartialEq)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl fmt::Debug for Pkce {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Pkce")
            .field("verifier", &"[REDACTED]")
            .field("challenge", &self.challenge)
            .finish()
    }
}

pub fn generate_pkce() -> Result<Pkce, OAuthError> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|_| OAuthError::InvalidResponse("secure random generation failed".into()))?;
    let verifier = URL_SAFE_NO_PAD.encode(bytes);
    let challenge = challenge_for_verifier(&verifier);
    Ok(Pkce {
        verifier,
        challenge,
    })
}

pub fn generate_state() -> Result<String, OAuthError> {
    let mut bytes = [0_u8; 24];
    getrandom::fill(&mut bytes)
        .map_err(|_| OAuthError::InvalidResponse("secure random generation failed".into()))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

pub fn challenge_for_verifier(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}
