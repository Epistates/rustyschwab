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
    aead::{Aead, Generate, KeyInit},
};
use secrecy::{SecretBox, SecretString};
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
    /// - Uses the OS-provided CSPRNG for nonce generation (`getrandom`)
    /// - Each encryption uses a unique random nonce (never reused)
    /// - Provides authenticated encryption (detects tampering)
    ///
    /// # Arguments
    ///
    /// * `plaintext` - Data to encrypt. It is only borrowed: keep secrets in a
    ///   zeroizing buffer (`Zeroizing<Vec<u8>>`) on the caller's side.
    /// * `key` - 32-byte encryption key
    ///
    /// # Errors
    ///
    /// Returns `AuthError::EncryptionFailed` if encryption fails
    pub fn encrypt(plaintext: &[u8], key: &[u8; KEY_SIZE]) -> Result<Self> {
        // Create cipher from key
        let cipher = ChaCha20Poly1305::new(key.into());

        // Random nonce from the OS CSPRNG (CRITICAL: unique per encryption)
        let nonce = Nonce::generate();

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
        let nonce = Nonce::try_from(nonce_bytes).map_err(|_| {
            Error::Auth(AuthError::DecryptionFailed(
                "Encrypted data has a malformed nonce".to_string(),
            ))
        })?;

        // Create cipher from key
        let cipher = ChaCha20Poly1305::new(key.into());

        // Decrypt with AEAD authentication
        let plaintext = cipher.decrypt(&nonce, ciphertext).map_err(|e| {
            Error::Auth(AuthError::DecryptionFailed(format!(
                "ChaCha20Poly1305 decrypt failed: {}",
                e
            )))
        })?;

        // Wrap in SecretBox<Vec<u8>> for automatic zeroization (requires Box)
        Ok(SecretBox::new(Box::new(plaintext)))
    }
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

    let written = (|| {
        file.write_all(data)
            .map_err(|e| AuthError::TokenFileError(format!("Failed to write data: {}", e)))?;
        // Sync to disk before rename (ensures data is written; on Apple
        // platforms `sync_all` is `F_FULLFSYNC`).
        file.sync_all()
            .map_err(|e| AuthError::TokenFileError(format!("Failed to sync file: {}", e)))?;
        drop(file);
        // Atomic rename
        std::fs::rename(&temp_path, path)
            .map_err(|e| AuthError::TokenFileError(format!("Failed to rename temp file: {}", e)))
    })();
    if let Err(error) = written {
        // The temp file holds the secret too: never leave it behind.
        let _ = std::fs::remove_file(&temp_path);
        return Err(error.into());
    }

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

/// Open an existing private file for reading, checking the open descriptor
/// rather than the path, so the file read is the file checked: a path
/// swapped for a symlink between a check and a read cannot slip through.
///
/// - **Unix**: opened with `O_NOFOLLOW` (a symlink is refused) and
///   `O_NONBLOCK` (a planted FIFO cannot hang the open), then required to
///   be a regular file owned by this user (or root) with mode exactly 0600.
/// - **Windows**: an owner-only protected DACL, opened without following a
///   reparse point, and refused if the handle is one.
///
/// A missing file is [`Error::Io`] with [`std::io::ErrorKind::NotFound`].
#[cfg(unix)]
pub fn open_private_file(path: &Path) -> Result<std::fs::File> {
    use rustix::fs::{Mode, OFlags};
    use std::os::unix::fs::MetadataExt;
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let file = match rustix::fs::open(path, flags, Mode::empty()) {
        Ok(fd) => std::fs::File::from(fd),
        // What O_NOFOLLOW answers for a symlink (EMLINK on the BSDs).
        Err(rustix::io::Errno::LOOP | rustix::io::Errno::MLINK) => {
            return Err(insecure(path, "is a symbolic link"));
        }
        Err(errno) => return Err(Error::Io(errno.into())),
    };
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        return Err(insecure(path, "is not a regular file"));
    }
    let owner = metadata.uid();
    if owner != rustix::process::geteuid().as_raw() && owner != 0 {
        return Err(insecure(path, "is owned by another account"));
    }
    let perms = metadata.permissions().mode() & 0o777;
    if perms != 0o600 {
        return Err(insecure(
            path,
            &format!("has insecure permissions {perms:o} (expected 0600)"),
        ));
    }
    Ok(file)
}

#[cfg(windows)]
pub use crate::security_windows::open_private_file;

#[cfg(not(any(unix, windows)))]
pub fn open_private_file(_path: &Path) -> Result<std::fs::File> {
    Err(Error::Config(
        "Secure file storage is unsupported on this platform".into(),
    ))
}

/// A refusal that keeps the path out of the error string: it can reach an
/// MCP tool response (and so a model), and the path discloses the home
/// directory layout. The operator log gets the path.
pub(crate) fn insecure(path: &Path, why: &str) -> Error {
    tracing::error!(?path, "secret file {why}");
    Error::Auth(AuthError::TokenFileInsecure(format!(
        "secret file {why}; see server logs for the path"
    )))
}

/// Verify that `path` is a private file, as [`open_private_file`] does.
pub fn verify_file_permissions(path: &Path) -> Result<()> {
    open_private_file(path).map(drop)
}

/// Read a private file (see [`open_private_file`]) whole, into a buffer
/// sized to it and wiped on drop.
pub fn read_private_file(path: &Path) -> Result<Zeroizing<Vec<u8>>> {
    use std::io::Read;
    let file = open_private_file(path)?;
    let len = file.metadata()?.len();
    let mut contents = Zeroizing::new(Vec::with_capacity(usize::try_from(len).unwrap_or(0)));
    // Read what was there when it was opened, so the buffer never grows.
    file.take(len).read_to_end(&mut contents)?;
    Ok(contents)
}

/// Read a configuration file that decides authorization (an MCP policy,
/// grants, identity or risk file), at most `limit` bytes.
///
/// Unlike a secret it may be world-readable, and it may be a symlink (a
/// Kubernetes ConfigMap mounts files through two of them), but on Unix the
/// file it resolves to, and that file's directory, must be owned by this
/// account or root and writable by no one else: a file another local user
/// can edit could grant that user trading. Non-blocking, so a planted FIFO
/// cannot hang the read. Elsewhere it is read as is, within `limit`.
pub fn read_trusted_file(path: &Path, limit: u64) -> Result<Vec<u8>> {
    use std::io::Read;
    #[cfg(unix)]
    let file = {
        use rustix::fs::{Mode, OFlags};
        use std::os::unix::fs::MetadataExt;
        let resolved = std::fs::canonicalize(path)?;
        let trusted = |metadata: &std::fs::Metadata| {
            let owner = metadata.uid();
            (owner == rustix::process::geteuid().as_raw() || owner == 0)
                && metadata.permissions().mode() & 0o022 == 0
        };
        if let Some(parent) = resolved.parent()
            && !trusted(&std::fs::metadata(parent)?)
        {
            return Err(untrusted(
                path,
                "is in a directory another account can write",
            ));
        }
        let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
        let file = std::fs::File::from(
            rustix::fs::open(&resolved, flags, Mode::empty()).map_err(std::io::Error::from)?,
        );
        let metadata = file.metadata()?;
        if !metadata.file_type().is_file() {
            return Err(untrusted(path, "is not a regular file"));
        }
        if !trusted(&metadata) {
            return Err(untrusted(
                path,
                "must be owned by this account or root and writable by no one else",
            ));
        }
        file
    };
    #[cfg(not(unix))]
    let file = std::fs::File::open(path)?;
    let mut contents = Vec::new();
    file.take(limit + 1).read_to_end(&mut contents)?;
    if contents.len() as u64 > limit {
        return Err(Error::Config(format!(
            "Configuration file exceeds {limit} bytes"
        )));
    }
    Ok(contents)
}

#[cfg(unix)]
fn untrusted(path: &Path, why: &str) -> Error {
    tracing::error!(?path, "configuration file {why}");
    Error::Config(format!(
        "A configuration file {why}; see server logs for the path"
    ))
}

/// Create a private directory before any database or auxiliary file is opened.
///
/// A directory created here is 0700; an existing one must already be a
/// real directory (not a symlink), owned by this user and closed to others.
#[cfg(unix)]
pub fn ensure_private_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    if !path.exists() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)?;
    }
    // Checked without following links, after creation: a symlink planted
    // between the existence check and `create` is refused, not written through.
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_dir()
        || metadata.permissions().mode() & 0o077 != 0
        || metadata.uid() != rustix::process::geteuid().as_raw()
    {
        return Err(Error::Storage(
            "State directory must be private (0700), owned by this account, \
             and not a symlink"
                .into(),
        ));
    }
    Ok(())
}

/// Read a one-line secret (a bearer token, for example) from a private file
/// (see [`open_private_file`]), trimming surrounding whitespace. The read
/// buffer is wiped on drop.
pub fn read_secret_file(path: &Path) -> Result<SecretString> {
    let contents = read_private_file(path)?;
    let text = std::str::from_utf8(&contents)
        .map_err(|_| Error::Config("Secret file is not UTF-8".into()))?;
    Ok(SecretString::from(text.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret;

    /// A token file written by an earlier release still opens: this blob
    /// was encrypted by chacha20poly1305 0.10 (key: 32 bytes of 7).
    #[test]
    fn a_blob_from_an_earlier_release_decrypts() {
        let hex = "0edc4b3b89ba0424a6b3799c8d32927b83dc25ae6b83a09044c85616088951f4\
                   e44b7e1301bb30566f8d50b090be72a693d4795b8dc1";
        let data = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect();
        let plaintext = EncryptedToken { data }.decrypt(&[7u8; 32]).unwrap();
        assert_eq!(
            plaintext.expose_secret().as_slice(),
            b"rustyschwab token store v1"
        );
    }

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
    #[cfg(unix)]
    fn private_files_refuse_links_fifos_and_loose_modes() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("tokens.json");
        secure_file_write(&real, b"secret").unwrap();
        assert_eq!(&*read_private_file(&real).unwrap(), b"secret");

        let link = dir.path().join("link.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(matches!(
            read_private_file(&link),
            Err(Error::Auth(AuthError::TokenFileInsecure(_)))
        ));

        let fifo = dir.path().join("fifo");
        assert!(
            std::process::Command::new("mkfifo")
                .args(["-m", "600"])
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        // Refused without blocking on a writer.
        assert!(verify_file_permissions(&fifo).is_err());

        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(verify_file_permissions(&real).is_err());

        let missing = read_private_file(&dir.path().join("missing")).unwrap_err();
        assert!(matches!(missing, Error::Io(e) if e.kind() == std::io::ErrorKind::NotFound));
    }

    /// An authorization file may be world-readable and reached through a
    /// symlink (a ConfigMap mount), but not writable by anyone else, and not
    /// larger than its limit.
    #[cfg(unix)]
    #[test]
    fn trusted_files_refuse_other_writers_and_oversize() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let policy = dir.path().join("policy.json");
        std::fs::write(&policy, b"[]").unwrap();
        std::fs::set_permissions(&policy, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(read_trusted_file(&policy, 16).unwrap(), b"[]");
        let link = dir.path().join("current");
        std::os::unix::fs::symlink(&policy, &link).unwrap();
        assert_eq!(read_trusted_file(&link, 16).unwrap(), b"[]");
        assert!(read_trusted_file(&policy, 1).is_err());
        std::fs::set_permissions(&policy, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert!(read_trusted_file(&link, 16).is_err());
        std::fs::set_permissions(&policy, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(read_trusted_file(&policy, 16).is_err());
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn state_directories_refuse_a_symlinked_leaf() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = dir.path().join("elsewhere");
        ensure_private_directory(&elsewhere).unwrap();
        let link = dir.path().join("state");
        std::os::unix::fs::symlink(&elsewhere, &link).unwrap();
        assert!(ensure_private_directory(&link).is_err());
    }

    #[cfg(unix)]
    #[test]
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
