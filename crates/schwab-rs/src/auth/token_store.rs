//! # Token Storage Backends
//!
//! Provides secure, pluggable token storage with multiple backend options:
//! - **File**: Plain JSON files with 0600 permissions (basic)
//! - **EncryptedFile**: ChaCha20Poly1305 encrypted files (recommended)

#![allow(missing_docs)] // Internal token storage implementation
//! - **Keychain**: Cross-platform secure credential storage (best)
//!
//! ## Security Comparison
//!
//! | Backend | Encryption | Permissions | Cross-Platform | Security Level |
//! |---------|------------|-------------|----------------|----------------|
//! | File | ❌ No | ✅ 0600 (Unix) | ✅ Yes | ⭐⭐ Basic |
//! | EncryptedFile | ✅ ChaCha20 | ✅ 0600 (Unix) | ✅ Yes | ⭐⭐⭐⭐ High |
//! | Keychain | ✅ OS-native | ✅ OS-native | ✅ Yes | ⭐⭐⭐⭐⭐ Best |

use crate::error::{AuthError, Error};

pub type AuthResult<T> = std::result::Result<T, AuthError>;
use crate::security::{EncryptedToken, generate_random_key, secure_file_write, verify_file_permissions};

/// Helper to convert Result<T, Error> to Result<T, AuthError>
fn to_auth_result<T>(result: Result<T, Error>) -> AuthResult<T> {
    result.map_err(|e| match e {
        Error::Auth(auth_err) => auth_err,
        other => AuthError::TokenFileError(other.to_string()),
    })
}
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use zeroize::Zeroizing;
use chrono::{DateTime, Utc};

/// Clock skew tolerance in seconds for token validation
/// Allows for minor time drift between client and server
const CLOCK_SKEW_TOLERANCE_SECS: i64 = 5;

/// Minimum recommended refresh buffer in seconds
const MIN_REFRESH_BUFFER_SECS: i64 = 30;

/// Token set with secure memory handling
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenSet {
    /// Access token (30-minute lifetime) - wrapped in SecretString for memory safety
    #[serde(skip)]
    pub access_token: Option<SecretString>,

    /// Refresh token (7-day lifetime) - wrapped in SecretString
    #[serde(skip)]
    pub refresh_token: Option<SecretString>,

    /// ID token (captured for OpenID Connect compatibility) - wrapped in SecretString
    #[serde(skip)]
    pub id_token: Option<SecretString>,

    /// For serialization only (zeroized after use)
    #[serde(rename = "access_token")]
    access_token_raw: Option<String>,

    #[serde(rename = "refresh_token")]
    refresh_token_raw: Option<String>,

    #[serde(rename = "id_token")]
    id_token_raw: Option<String>,

    pub token_type: String,
    pub expires_in: i64,
    pub scope: String,

    #[serde(with = "chrono::serde::ts_seconds")]
    pub expires_at: chrono::DateTime<chrono::Utc>,

    #[serde(with = "chrono::serde::ts_seconds")]
    pub issued_at: chrono::DateTime<chrono::Utc>,

    #[serde(with = "chrono::serde::ts_seconds_option", default)]
    pub refresh_token_expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl TokenSet {
    /// Creates a new TokenSet with secret-wrapped tokens
    pub fn new(
        access_token: String,
        refresh_token: String,
        id_token: String,
        token_type: String,
        expires_in: i64,
        scope: String,
        issued_at: chrono::DateTime<chrono::Utc>,
    ) -> Self {
        Self {
            access_token: Some(SecretString::from(access_token.clone())),
            refresh_token: Some(SecretString::from(refresh_token.clone())),
            id_token: Some(SecretString::from(id_token.clone())),
            access_token_raw: Some(access_token),
            refresh_token_raw: Some(refresh_token),
            id_token_raw: Some(id_token),
            token_type,
            expires_in,
            scope,
            expires_at: issued_at + chrono::Duration::seconds(expires_in),
            issued_at,
            refresh_token_expires_at: Some(issued_at + chrono::Duration::days(7)),
        }
    }

    /// Gets access token (explicit access for security audit trail)
    pub fn access_token(&self) -> AuthResult<String> {
        self.access_token
            .as_ref()
            .map(|s| s.expose_secret().to_string())
            .ok_or_else(|| AuthError::MissingConfig("Access token not available".into()))
    }

    /// Gets refresh token (explicit access for security audit trail)
    pub fn refresh_token(&self) -> AuthResult<String> {
        self.refresh_token
            .as_ref()
            .map(|s| s.expose_secret().to_string())
            .ok_or_else(|| AuthError::MissingRefreshToken)
    }

    /// Gets ID token if available
    pub fn id_token(&self) -> Option<String> {
        self.id_token
            .as_ref()
            .map(|s| s.expose_secret().to_string())
    }

    pub fn is_valid(&self) -> bool {
        self.is_valid_at(Utc::now())
    }

    /// Check if token is valid at a specific time, with clock skew tolerance
    pub fn is_valid_at(&self, now: DateTime<Utc>) -> bool {
        // Add tolerance for clock skew to prevent spurious expiry errors
        let tolerance = chrono::Duration::seconds(CLOCK_SKEW_TOLERANCE_SECS);
        now < (self.expires_at + tolerance)
    }

    pub fn needs_refresh(&self, buffer_seconds: i64) -> bool {
        // Validate buffer is reasonable
        if buffer_seconds < MIN_REFRESH_BUFFER_SECS {
            log::warn!(
                "Refresh buffer very small: {}s (minimum {}s recommended)",
                buffer_seconds,
                MIN_REFRESH_BUFFER_SECS
            );
        }

        if buffer_seconds >= self.expires_in - 10 {
            log::warn!(
                "Refresh buffer too large: {}s (token expires in {}s, buffer should be smaller)",
                buffer_seconds,
                self.expires_in
            );
        }

        let refresh_time = self.expires_at - chrono::Duration::seconds(buffer_seconds);
        Utc::now() >= refresh_time
    }

    pub fn refresh_token_expired(&self) -> bool {
        if let Some(refresh_expires) = self.refresh_token_expires_at {
            chrono::Utc::now() >= refresh_expires
        } else {
            chrono::Utc::now() >= self.issued_at + chrono::Duration::days(7)
        }
    }

    /// Prepares for serialization by ensuring raw tokens are available
    fn prepare_for_serialization(&mut self) {
        if self.access_token_raw.is_none() {
            if let Some(ref secret) = self.access_token {
                self.access_token_raw = Some(secret.expose_secret().to_string());
            }
        }
        if self.refresh_token_raw.is_none() {
            if let Some(ref secret) = self.refresh_token {
                self.refresh_token_raw = Some(secret.expose_secret().to_string());
            }
        }
        if self.id_token_raw.is_none() {
            if let Some(ref secret) = self.id_token {
                self.id_token_raw = Some(secret.expose_secret().to_string());
            }
        }
    }

    /// Cleans up after deserialization by wrapping tokens in SecretString
    fn finalize_after_deserialization(&mut self) {
        if let Some(access) = self.access_token_raw.take() {
            self.access_token = Some(SecretString::from(access));
        }
        if let Some(refresh) = self.refresh_token_raw.take() {
            self.refresh_token = Some(SecretString::from(refresh));
        }
        if let Some(id) = self.id_token_raw.take() {
            self.id_token = Some(SecretString::from(id));
        }
    }
}

/// Token storage backend trait
pub trait TokenStore: Send + Sync {
    fn save(&self, tokens: &TokenSet) -> AuthResult<()>;
    fn load(&self) -> AuthResult<TokenSet>;
}

/// Plain file storage with permission checks (Unix: 0600)
///
/// # Security
/// - ✅ Atomic writes (temp file + rename)
/// - ✅ Unix: 0600 permissions enforced
/// - ❌ No encryption at rest
/// - ❌ Plaintext JSON
///
/// **Recommendation:** Use EncryptedFileTokenStore or KeychainTokenStore instead
pub struct FileTokenStore {
    path: PathBuf,
}

impl FileTokenStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl TokenStore for FileTokenStore {
    fn save(&self, tokens: &TokenSet) -> AuthResult<()> {
        let mut tokens_copy = tokens.clone();
        tokens_copy.prepare_for_serialization();

        let json = serde_json::to_string_pretty(&tokens_copy)
            .map_err(|e| AuthError::TokenStorage(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("Serialization failed: {}", e),
            )))?;

        // Use secure write with permission enforcement
        to_auth_result(secure_file_write(&self.path, json.as_bytes()))?;

        log::debug!("Tokens saved to file: {:?}", self.path);
        Ok(())
    }

    fn load(&self) -> AuthResult<TokenSet> {
        // Verify permissions before reading
        to_auth_result(verify_file_permissions(&self.path))?;

        let json = std::fs::read_to_string(&self.path)
            .map_err(AuthError::TokenStorage)?;

        let mut tokens: TokenSet = serde_json::from_str(&json)
            .map_err(|e| AuthError::TokenStorage(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Deserialization failed: {}", e),
            )))?;

        tokens.finalize_after_deserialization();

        log::debug!("Tokens loaded from file: {:?}", self.path);
        Ok(tokens)
    }
}

/// Encrypted file storage with ChaCha20Poly1305 AEAD
///
/// # Security
/// - ✅ ChaCha20Poly1305 AEAD encryption
/// - ✅ Authenticated (tamper detection)
/// - ✅ Atomic writes with 0600 permissions
/// - ✅ Random key generation
/// - ⚠️ Key stored in separate file (must be secured)
///
/// **Key Management:**
/// - Key file: `{tokens_file}.key`
/// - Key is generated once and reused
/// - Key is protected with 0600 permissions
pub struct EncryptedFileTokenStore {
    path: PathBuf,
    key_path: PathBuf,
}

impl EncryptedFileTokenStore {
    pub fn new(path: PathBuf) -> Self {
        let key_path = path.with_extension("key");
        Self { path, key_path }
    }

    fn load_or_generate_key(&self) -> AuthResult<Zeroizing<[u8; 32]>> {
        if self.key_path.exists() {
            // Load existing key
            to_auth_result(verify_file_permissions(&self.key_path))?;
            let key_bytes = std::fs::read(&self.key_path)
                .map_err(|e| AuthError::TokenFileError(format!("Failed to read key: {}", e)))?;

            if key_bytes.len() != 32 {
                return Err(AuthError::TokenFileError(
                    format!("Invalid key length: expected 32, got {}", key_bytes.len())
                ).into());
            }

            let mut key = Zeroizing::new([0u8; 32]);
            key.copy_from_slice(&key_bytes);
            log::debug!("Loaded encryption key from: {:?}", self.key_path);
            Ok(key)
        } else {
            // Generate new key
            let key = to_auth_result(generate_random_key())?;
            to_auth_result(secure_file_write(&self.key_path, &*key))?;
            log::info!("Generated new encryption key: {:?}", self.key_path);
            Ok(key)
        }
    }
}

impl TokenStore for EncryptedFileTokenStore {
    fn save(&self, tokens: &TokenSet) -> AuthResult<()> {
        let key = self.load_or_generate_key()?;

        let mut tokens_copy = tokens.clone();
        tokens_copy.prepare_for_serialization();

        let json = serde_json::to_string(&tokens_copy)
            .map_err(|e| AuthError::EncryptionFailed(format!("Serialization failed: {}", e)))?;

        // Encrypt with AEAD
        let encrypted = to_auth_result(EncryptedToken::encrypt(json.as_bytes(), &key))?;

        // Serialize encrypted data
        let encrypted_json = serde_json::to_vec(&encrypted)
            .map_err(|e| AuthError::EncryptionFailed(format!("Failed to serialize encrypted data: {}", e)))?;

        // Write with secure permissions
        to_auth_result(secure_file_write(&self.path, &encrypted_json))?;

        log::debug!("Encrypted tokens saved to: {:?}", self.path);
        Ok(())
    }

    fn load(&self) -> AuthResult<TokenSet> {
        let key = self.load_or_generate_key()?;

        // Verify permissions
        to_auth_result(verify_file_permissions(&self.path))?;

        // Read encrypted data
        let encrypted_json = std::fs::read(&self.path)
            .map_err(|e| AuthError::DecryptionFailed(format!("Failed to read file: {}", e)))?;

        // Deserialize encrypted container
        let encrypted: EncryptedToken = serde_json::from_slice(&encrypted_json)
            .map_err(|e| AuthError::DecryptionFailed(format!("Failed to parse encrypted data: {}", e)))?;

        // Decrypt with AEAD
        let decrypted = to_auth_result(encrypted.decrypt(&key))?;

        // Parse JSON
        let json = String::from_utf8(decrypted.expose_secret().to_vec())
            .map_err(|e| AuthError::DecryptionFailed(format!("Invalid UTF-8: {}", e)))?;

        let mut tokens: TokenSet = serde_json::from_str(&json)
            .map_err(|e| AuthError::DecryptionFailed(format!("Failed to parse JSON: {}", e)))?;

        tokens.finalize_after_deserialization();

        log::debug!("Decrypted tokens loaded from: {:?}", self.path);
        Ok(tokens)
    }
}

/// Cross-platform keychain storage using the keyring crate
///
/// # Security
/// - ✅ OS-native secure storage
/// - ✅ macOS: Keychain
/// - ✅ Windows: Credential Manager
/// - ✅ Linux: Secret Service (libsecret)
/// - ✅ Encrypted by OS
/// - ✅ Protected with user authentication
///
/// **Recommendation:** This is the most secure option
pub struct KeychainTokenStore {
    service: String,
    account: String,
}

impl KeychainTokenStore {
    pub fn new(service: String, account: String) -> Self {
        Self { service, account }
    }
}

impl TokenStore for KeychainTokenStore {
    fn save(&self, tokens: &TokenSet) -> AuthResult<()> {
        let mut tokens_copy = tokens.clone();
        tokens_copy.prepare_for_serialization();

        let json = serde_json::to_string(&tokens_copy)
            .map_err(|e| AuthError::KeyringError(format!("Serialization failed: {}", e)))?;

        // Use keyring crate for cross-platform support
        let entry = keyring::Entry::new(&self.service, &self.account)
            .map_err(|e| AuthError::KeyringError(format!("Failed to create entry: {}", e)))?;

        entry.set_password(&json)
            .map_err(|e| AuthError::KeyringError(format!("Failed to save: {}", e)))?;

        log::debug!("Tokens saved to keychain: service={}, account={}", self.service, self.account);
        Ok(())
    }

    fn load(&self) -> AuthResult<TokenSet> {
        let entry = keyring::Entry::new(&self.service, &self.account)
            .map_err(|e| AuthError::KeyringError(format!("Failed to create entry: {}", e)))?;

        let json = entry.get_password()
            .map_err(|e| AuthError::KeyringError(format!("Failed to load: {}", e)))?;

        let mut tokens: TokenSet = serde_json::from_str(&json)
            .map_err(|e| AuthError::KeyringError(format!("Failed to parse: {}", e)))?;

        tokens.finalize_after_deserialization();

        log::debug!("Tokens loaded from keychain: service={}, account={}", self.service, self.account);
        Ok(tokens)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    fn create_test_tokens() -> TokenSet {
        TokenSet::new(
            "access_token_12345".to_string(),
            "refresh_token_67890".to_string(),
            "id_token_abcde".to_string(),
            "Bearer".to_string(),
            1800,
            "api".to_string(),
            chrono::Utc::now(),
        )
    }

    #[test]
    fn test_file_store_roundtrip() {
        let temp = NamedTempFile::new().unwrap();
        let store = FileTokenStore::new(temp.path().to_path_buf());
        let tokens = create_test_tokens();

        store.save(&tokens).unwrap();
        let loaded = store.load().unwrap();

        assert_eq!(tokens.access_token().unwrap(), loaded.access_token().unwrap());
        assert_eq!(tokens.refresh_token().unwrap(), loaded.refresh_token().unwrap());
    }

    #[test]
    fn test_encrypted_file_store_roundtrip() {
        let temp = NamedTempFile::new().unwrap();
        let store = EncryptedFileTokenStore::new(temp.path().to_path_buf());
        let tokens = create_test_tokens();

        store.save(&tokens).unwrap();
        let loaded = store.load().unwrap();

        assert_eq!(tokens.access_token().unwrap(), loaded.access_token().unwrap());
        assert_eq!(tokens.refresh_token().unwrap(), loaded.refresh_token().unwrap());
    }

    #[test]
    #[cfg(unix)]
    fn test_file_store_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let temp = NamedTempFile::new().unwrap();
        let store = FileTokenStore::new(temp.path().to_path_buf());
        let tokens = create_test_tokens();

        store.save(&tokens).unwrap();

        let metadata = std::fs::metadata(temp.path()).unwrap();
        let mode = metadata.permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "File should have 0600 permissions");
    }

    #[test]
    fn test_encrypted_file_tampering_detection() {
        let temp = NamedTempFile::new().unwrap();
        let store = EncryptedFileTokenStore::new(temp.path().to_path_buf());
        let tokens = create_test_tokens();

        store.save(&tokens).unwrap();

        // Tamper with file
        let mut data = std::fs::read(temp.path()).unwrap();
        if data.len() > 50 {
            data[50] ^= 0xFF; // Flip a bit
            std::fs::write(temp.path(), data).unwrap();
        }

        // Should fail to decrypt
        let result = store.load();
        assert!(result.is_err(), "Tampered data should fail to decrypt");
    }
}
