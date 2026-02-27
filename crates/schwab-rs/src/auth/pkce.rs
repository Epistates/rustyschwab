//! PKCE (Proof Key for Public Clients) implementation and validation
//!
//! Implements RFC 7636 PKCE for OAuth2 authorization flow in public clients.
//! Prevents authorization code interception attacks by using dynamically generated
//! code verifiers and challenges.

use crate::error::{AuthError, Error, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD as BASE64_URL, Engine};
use ring::{digest, rand as ring_rand};
use ring::rand::SecureRandom;
use std::collections::HashSet;
use parking_lot::RwLock;
use std::sync::Arc;
use tracing::{debug, warn};

/// PKCE session state tracker
///
/// Manages code verifiers and challenges for a single OAuth authorization flow,
/// with history tracking to prevent replay attacks.
#[derive(Clone)]
pub struct PkceSession {
    /// Current code verifier (used only once, then consumed)
    current_verifier: Arc<RwLock<Option<String>>>,
    /// History of used verifiers (prevents replays of old challenges)
    used_verifiers: Arc<RwLock<HashSet<String>>>,
}

impl PkceSession {
    /// Create a new PKCE session
    pub fn new() -> Self {
        Self {
            current_verifier: Arc::new(RwLock::new(None)),
            used_verifiers: Arc::new(RwLock::new(HashSet::new())),
        }
    }

    /// Generate a new code verifier and return the challenge
    ///
    /// # Returns
    /// Tuple of (verifier, challenge) for the authorization URL
    pub fn generate(&self) -> Result<(String, String)> {
        let verifier = Self::generate_code_verifier()?;
        let challenge = Self::code_challenge_s256(&verifier);

        // Store for later use in token exchange
        *self.current_verifier.write() = Some(verifier.clone());
        debug!("Generated new PKCE verifier for authorization");

        Ok((verifier, challenge))
    }

    /// Take (consume) the current verifier for token exchange
    ///
    /// This enforces single-use and prevents replay attacks by consuming
    /// the verifier after it's used.
    ///
    /// # Returns
    /// The verifier if available, error if none exists or already used
    pub fn take_verifier(&self) -> Result<String> {
        let verifier = self.current_verifier.write().take()
            .ok_or_else(|| Error::Auth(AuthError::OAuthFlow(
                "PKCE enabled but no code verifier found. Call authorize() first.".to_string()
            )))?;

        // Add to history to detect any attempts to reuse
        if !self.used_verifiers.write().insert(verifier.clone()) {
            // Verifier was already in history - possible replay attack
            warn!("Attempted reuse of PKCE verifier - possible replay attack detected");
            return Err(Error::Auth(AuthError::OAuthFlow(
                "PKCE verifier already used - possible replay attack".to_string()
            )));
        }

        debug!("PKCE verifier consumed (single-use enforced)");
        Ok(verifier)
    }

    /// Check if a verifier has already been used
    pub fn has_been_used(&self, verifier: &str) -> bool {
        self.used_verifiers.read().contains(verifier)
    }

    /// Get count of used verifiers (for monitoring/metrics)
    pub fn used_count(&self) -> usize {
        self.used_verifiers.read().len()
    }

    /// Generate a cryptographically secure code verifier
    ///
    /// Follows RFC 7636 §4.1: 43-128 characters from [A-Z] / [a-z] / [0-9] / "-" / "." / "_" / "~"
    fn generate_code_verifier() -> Result<String> {
        // Generate 96 bytes of random data
        let rng = ring_rand::SystemRandom::new();
        let mut bytes = [0u8; 96];
        rng.fill(&mut bytes)
            .map_err(|_| Error::Auth(AuthError::OAuthFlow(
                "Failed to generate PKCE verifier - cryptographic RNG unavailable".to_string()
            )))?;

        // Base64url encode to get 128 characters (RFC 7636 allows up to 128)
        Ok(BASE64_URL.encode(&bytes))
    }

    /// Generate code challenge using S256 method (SHA256)
    ///
    /// Follows RFC 7636 §4.2: BASE64URL(SHA256(verifier))
    fn code_challenge_s256(verifier: &str) -> String {
        let digest = digest::digest(&digest::SHA256, verifier.as_bytes());
        BASE64_URL.encode(digest.as_ref())
    }
}

impl Default for PkceSession {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pkce_session_creation() {
        let session = PkceSession::new();
        assert_eq!(session.used_count(), 0);
    }

    #[test]
    fn test_pkce_generate_and_take() {
        let session = PkceSession::new();

        let (verifier, challenge) = session.generate().expect("Failed to generate");
        assert!(!verifier.is_empty());
        assert!(!challenge.is_empty());
        assert_ne!(verifier, challenge);

        let taken = session.take_verifier().expect("Failed to take");
        assert_eq!(taken, verifier);
        assert_eq!(session.used_count(), 1);
    }

    #[test]
    fn test_pkce_single_use_enforcement() {
        let session = PkceSession::new();

        session.generate().expect("Failed to generate");
        session.take_verifier().expect("Failed to take");

        // Attempting to take again should fail
        let result = session.take_verifier();
        assert!(result.is_err());
    }

    #[test]
    fn test_pkce_replay_detection() {
        let session = PkceSession::new();

        let (verifier, _) = session.generate().expect("Failed to generate");
        session.take_verifier().expect("Failed to take");

        // Attempting to reuse by manually inserting should be detected
        assert!(session.has_been_used(&verifier));
    }

    #[test]
    fn test_pkce_multiple_sessions() {
        let session = PkceSession::new();

        // Generate and consume first verifier
        let (v1, _) = session.generate().expect("Failed to generate 1");
        session.take_verifier().expect("Failed to take 1");

        // Generate and consume second verifier
        let (v2, _) = session.generate().expect("Failed to generate 2");
        session.take_verifier().expect("Failed to take 2");

        // Both should be tracked
        assert!(session.has_been_used(&v1));
        assert!(session.has_been_used(&v2));
        assert_eq!(session.used_count(), 2);
    }

    #[test]
    fn test_pkce_challenge_deterministic() {
        let verifier = "test_verifier_string";
        let challenge1 = PkceSession::code_challenge_s256(verifier);
        let challenge2 = PkceSession::code_challenge_s256(verifier);

        // Same verifier should produce same challenge
        assert_eq!(challenge1, challenge2);
        // Challenge should be different from verifier
        assert_ne!(verifier, challenge1);
    }
}
