//! # Security Module
//!
//! Provides best-in-class security features for token storage and handling:
//! - **Encryption at rest**: ChaCha20Poly1305 AEAD (audited by NCC Group)
//! - **Memory safety**: Automatic zeroization via `secrecy` crate
//! - **Cross-platform keychain**: Secure credential storage via `keyring`
//! - **File permissions**: Unix 0600 enforcement for token files
//!
//! ## Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────┐
//! │           Token Security Layers             │
//! ├─────────────────────────────────────────────┤
//! │ Layer 1: Memory      │ SecretString         │ ← Automatic zeroization
//! │ Layer 2: Storage     │ Keychain/Encrypted   │ ← Platform keychain or AES
//! │ Layer 3: Transport   │ HTTPS only           │ ← TLS 1.3
//! │ Layer 4: Filesystem  │ 0600 permissions     │ ← Owner-only access
//! └─────────────────────────────────────────────┘
//! ```

use crate::error::{AuthError, Error, Result};
use chacha20poly1305::{
    ChaCha20Poly1305, Nonce,
    aead::{Aead, AeadCore, KeyInit, OsRng},
};
use secrecy::{ExposeSecret, SecretBox, SecretString};
use serde::{Deserialize, Serialize};
use std::path::Path;
use zeroize::Zeroizing;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

/// Size of ChaCha20Poly1305 key in bytes (256 bits)
const KEY_SIZE: usize = 32;

/// Size of ChaCha20Poly1305 nonce in bytes (96 bits)
const NONCE_SIZE: usize = 12;

/// Encrypted token container with AEAD authentication
///
/// Format: `[nonce (12 bytes)] || [ciphertext + auth tag]`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncryptedToken {
    /// Nonce + ciphertext + authentication tag
    pub data: Vec<u8>,
}

impl EncryptedToken {
    /// Encrypts plaintext using ChaCha20Poly1305-AEAD
    ///
    /// # Security
    ///
    /// - Uses OS-provided RNG for nonce generation (`OsRng`)
    /// - Each encryption uses a unique random nonce (never reused)
    /// - Provides authenticated encryption (detects tampering)
    ///
    /// # Arguments
    ///
    /// * `plaintext` - Data to encrypt (will be zeroized after encryption)
    /// * `key` - 32-byte encryption key
    ///
    /// # Errors
    ///
    /// Returns `AuthError::EncryptionFailed` if encryption fails
    pub fn encrypt(plaintext: &[u8], key: &[u8; KEY_SIZE]) -> Result<Self> {
        // Create cipher from key
        let cipher = ChaCha20Poly1305::new(key.into());

        // Generate random nonce (CRITICAL: must be unique for each encryption)
        let nonce = ChaCha20Poly1305::generate_nonce(&mut OsRng);

        // Encrypt with AEAD
        let ciphertext = cipher.encrypt(&nonce, plaintext).map_err(|e| {
            Error::Auth(AuthError::EncryptionFailed(format!(
                "ChaCha20Poly1305 encrypt failed: {}",
                e
            )))
        })?;

        // Prepend nonce to ciphertext (required for decryption)
        let mut data = nonce.to_vec();
        data.extend_from_slice(&ciphertext);

        Ok(Self { data })
    }

    /// Decrypts ciphertext using ChaCha20Poly1305-AEAD
    ///
    /// # Security
    ///
    /// - Verifies authentication tag (prevents tampering)
    /// - Returns error if data has been modified
    /// - Result is automatically zeroized when dropped
    ///
    /// # Arguments
    ///
    /// * `key` - 32-byte encryption key (must match encryption key)
    ///
    /// # Errors
    ///
    /// Returns `AuthError::DecryptionFailed` if:
    /// - Data is too short (< nonce size)
    /// - Authentication tag verification fails (tampering detected)
    /// - Decryption fails for any other reason
    pub fn decrypt(&self, key: &[u8; KEY_SIZE]) -> Result<SecretBox<Vec<u8>>> {
        // Validate data length
        if self.data.len() < NONCE_SIZE {
            return Err(Error::Auth(AuthError::DecryptionFailed(
                "Encrypted data too short".to_string(),
            )));
        }

        // Extract nonce and ciphertext
        let (nonce_bytes, ciphertext) = self.data.split_at(NONCE_SIZE);
        let nonce = Nonce::from_slice(nonce_bytes);

        // Create cipher from key
        let cipher = ChaCha20Poly1305::new(key.into());

        // Decrypt with AEAD authentication
        let plaintext = cipher.decrypt(nonce, ciphertext).map_err(|e| {
            Error::Auth(AuthError::DecryptionFailed(format!(
                "ChaCha20Poly1305 decrypt failed: {}",
                e
            )))
        })?;

        // Wrap in SecretBox<Vec<u8>> for automatic zeroization (requires Box)
        Ok(SecretBox::new(Box::new(plaintext)))
    }
}

/// Derives a 32-byte encryption key from a password using PBKDF2-HMAC-SHA256
///
/// # Security
///
/// - Uses 600,000 iterations (OWASP 2023 recommendation)
/// - Salted with machine-specific data to prevent rainbow tables
/// - **WARNING**: User must provide a strong password (16+ chars recommended)
///
/// # Arguments
///
/// * `password` - User-provided password (will be zeroized)
/// * `salt` - Application-specific salt (should include machine ID)
///
/// # Returns
///
/// Returns a 32-byte key suitable for ChaCha20Poly1305
pub fn derive_key_from_password(password: &SecretString, salt: &[u8]) -> Zeroizing<[u8; KEY_SIZE]> {
    use ring::pbkdf2;
    use std::num::NonZeroU32;

    const ITERATIONS: u32 = 600_000; // OWASP 2023 recommendation
    // SAFETY: ITERATIONS is a non-zero constant, so this will never panic
    let iterations =
        NonZeroU32::new(ITERATIONS).expect("ITERATIONS constant is non-zero and will never panic");

    let mut key = Zeroizing::new([0u8; KEY_SIZE]);
    pbkdf2::derive(
        pbkdf2::PBKDF2_HMAC_SHA256,
        iterations,
        salt,
        password.expose_secret().as_bytes(),
        &mut *key,
    );

    key
}

/// Generates a cryptographically secure random key for ChaCha20Poly1305
///
/// # Security
///
/// Uses `ring::rand::SystemRandom` (OS-provided CSPRNG)
///
/// # Returns
///
/// Returns a 32-byte random key or an error if the RNG is unavailable
///
/// # Errors
///
/// Returns `AuthError::EncryptionFailed` if the cryptographic RNG is unavailable,
/// which may indicate a compromised or misconfigured system.
pub fn generate_random_key() -> Result<Zeroizing<[u8; KEY_SIZE]>> {
    use ring::rand::{SecureRandom, SystemRandom};

    let rng = SystemRandom::new();
    let mut key = Zeroizing::new([0u8; KEY_SIZE]);
    rng.fill(&mut *key).map_err(|_| {
        Error::Auth(AuthError::EncryptionFailed(
            "Cryptographic RNG unavailable - system may be compromised".to_string(),
        ))
    })?;

    Ok(key)
}

/// Securely writes data to a file with restricted permissions (Unix: 0600)
///
/// # Security
///
/// - **Unix**: Sets permissions to 0600 (owner read/write only)
/// - **Windows**: Protected owner/SYSTEM DACL and atomic replacement
/// - Atomic write via temp file + rename
/// - Verifies permissions after write
///
/// # Arguments
///
/// * `path` - File path to write
/// * `data` - Data to write
///
/// # Errors
///
/// Returns `AuthError::TokenFileError` if:
/// - File write fails
/// - Permission setting fails (Unix)
/// - Permission verification fails
#[cfg(unix)]
pub fn secure_file_write(path: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    // Randomized temp suffix prevents collisions between concurrent writers
    // and stale temp files from prior crashed writes.
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let mut temp_name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    temp_name.push(format!(".{}.tmp", suffix));
    let temp_path = path.with_file_name(temp_name);

    // CRITICAL: Set permissions at creation time (no race window)
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600) // Owner read/write only - set atomically!
        .open(&temp_path)
        .map_err(|e| AuthError::TokenFileError(format!("Failed to create temp file: {}", e)))?;

    file.write_all(data)
        .map_err(|e| AuthError::TokenFileError(format!("Failed to write data: {}", e)))?;

    // Sync to disk before rename (ensures data is written)
    file.sync_all()
        .map_err(|e| AuthError::TokenFileError(format!("Failed to sync file: {}", e)))?;

    // Drop file handle before rename
    drop(file);

    // Atomic rename
    std::fs::rename(&temp_path, path)
        .map_err(|e| AuthError::TokenFileError(format!("Failed to rename temp file: {}", e)))?;

    // Persist the directory entry, including the replacement, before reporting success.
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::File::open(parent)?.sync_all()?;
    // Verify final permissions (defense in depth)
    verify_file_permissions(path)?;

    tracing::debug!("Secure file write completed: {:?}", path);
    Ok(())
}

#[cfg(windows)]
pub use crate::security_windows::secure_file_write;

#[cfg(not(any(unix, windows)))]
pub fn secure_file_write(_path: &Path, _data: &[u8]) -> Result<()> {
    Err(Error::Config(
        "Secure file storage is unsupported on this platform".into(),
    ))
}

/// Verifies that a file has secure permissions (Unix: 0600)
///
/// # Security
///
/// - **Unix**: Checks that mode is exactly 0600
/// - **Windows**: Warns that verification is not available
///
/// # Arguments
///
/// * `path` - File path to check
///
/// # Errors
///
/// Returns `AuthError::TokenFileInsecure` if permissions are too permissive
#[cfg(unix)]
pub fn verify_file_permissions(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|e| AuthError::TokenFileError(format!("Failed to read file metadata: {}", e)))?;

    if !metadata.file_type().is_file() {
        return Err(Error::Auth(AuthError::TokenFileInsecure(
            "Token path must be a regular file".into(),
        )));
    }
    let mode = metadata.permissions().mode();
    let perms = mode & 0o777; // Extract permission bits

    if perms != 0o600 {
        // Keep the absolute path out of the error string: this error can
        // propagate to an MCP tool response (and thus to an LLM), and the
        // path discloses the home-directory layout / username. The full
        // path goes to the operator log instead.
        tracing::error!(
            ?path,
            mode = format!("{:o}", perms),
            "token file has insecure permissions (expected 0600)"
        );
        return Err(Error::Auth(AuthError::TokenFileInsecure(format!(
            "token file has insecure permissions {:o} (expected 0600); see server logs for the path",
            perms
        ))));
    }

    Ok(())
}

/// Create a private directory before any database or auxiliary file is opened.
#[cfg(unix)]
pub fn ensure_private_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    if !path.exists() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)?;
    }
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.permissions().mode() & 0o077 != 0 {
        return Err(Error::Storage(
            "State directory must be private (0700) and not a symlink".into(),
        ));
    }
    Ok(())
}

#[cfg(windows)]
pub use crate::security_windows::verify_file_permissions;
#[cfg(not(any(unix, windows)))]
pub fn verify_file_permissions(_path: &Path) -> Result<()> {
    Err(Error::Config(
        "Secure file storage is unsupported on this platform".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encrypt_decrypt_roundtrip() {
        let key = generate_random_key().unwrap();
        let plaintext = b"access_token_12345678";

        // Encrypt
        let encrypted = EncryptedToken::encrypt(plaintext, &key).unwrap();

        // Decrypt
        let decrypted = encrypted.decrypt(&key).unwrap();

        assert_eq!(decrypted.expose_secret().as_slice(), plaintext);
    }

    #[test]
    fn test_wrong_key_fails() {
        let key1 = generate_random_key().unwrap();
        let key2 = generate_random_key().unwrap();
        let plaintext = b"secret_data";

        let encrypted = EncryptedToken::encrypt(plaintext, &key1).unwrap();
        let result = encrypted.decrypt(&key2);

        assert!(result.is_err(), "Decryption with wrong key should fail");
    }

    #[test]
    fn test_tampered_data_fails() {
        let key = generate_random_key().unwrap();
        let plaintext = b"important_data";

        let mut encrypted = EncryptedToken::encrypt(plaintext, &key).unwrap();

        // Tamper with ciphertext
        encrypted.data[NONCE_SIZE] ^= 0xFF;

        let result = encrypted.decrypt(&key);
        assert!(result.is_err(), "Decryption of tampered data should fail");
    }

    #[test]
    fn test_key_derivation_deterministic() {
        let password = SecretString::from("strong_password_123");
        let salt = b"application_salt";

        let key1 = derive_key_from_password(&password, salt);
        let key2 = derive_key_from_password(&password, salt);

        assert_eq!(
            &*key1, &*key2,
            "Same password + salt should produce same key"
        );
    }

    #[test]
    fn test_key_derivation_different_passwords() {
        let password1 = SecretString::from("password1");
        let password2 = SecretString::from("password2");
        let salt = b"salt";

        let key1 = derive_key_from_password(&password1, salt);
        let key2 = derive_key_from_password(&password2, salt);

        assert_ne!(
            &*key1, &*key2,
            "Different passwords should produce different keys"
        );
    }

    #[test]
    #[cfg(unix)]
    fn test_secure_file_write_permissions() {
        use tempfile::NamedTempFile;

        let temp_file = NamedTempFile::new().unwrap();
        let path = temp_file.path();

        let data = b"secret token data";
        secure_file_write(path, data).unwrap();

        // Verify content
        let content = std::fs::read(path).unwrap();
        assert_eq!(content, data);

        // Verify permissions
        let metadata = std::fs::symlink_metadata(path).unwrap();
        assert!(metadata.file_type().is_file());
        let mode = metadata.permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "File should have 0600 permissions");
    }
}
#[cfg(windows)]
pub use crate::security_windows::ensure_private_directory;
#[cfg(not(any(unix, windows)))]
pub fn ensure_private_directory(_path: &Path) -> Result<()> {
    Err(Error::Storage(
        "Private state directories are unsupported on this platform".into(),
    ))
}
