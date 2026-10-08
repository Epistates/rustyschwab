//! # Token Storage Backends
//!
//! Provides secure, pluggable token storage with multiple backend options:
//! - **File**: Plain JSON files with 0600 permissions (basic)
//! - **EncryptedFile**: ChaCha20Poly1305 encrypted files (recommended)
//! - **Keychain**: the OS credential store (the macOS default)
//!
//! ## Comparison
//!
//! | Backend | Encryption | Access control | Survives reboot | Notes |
//! |---------|------------|----------------|-----------------|-------|
//! | File | none | 0600 file (Unix), owner-only ACL (Windows) | yes | basic |
//! | EncryptedFile | ChaCha20-Poly1305 | as File, key in a 0600 file beside it | yes | default off macOS |
//! | Keychain, macOS | OS keychain | login keychain | yes | default on macOS |
//! | Keychain, Linux | kernel keyutils | the user's keyrings | **no** | lost at logout or reboot |
//! | Keychain, Windows | Credential Manager | the user's vault | yes | a token set can exceed its 2,560-byte limit; prefer EncryptedFile |

use crate::error::{AuthError, Error};
use crate::security::{EncryptedToken, generate_random_key, read_private_file, secure_file_write};
use chrono::{DateTime, Utc};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::path::PathBuf;
use zeroize::Zeroizing;

pub type AuthResult<T> = std::result::Result<T, AuthError>;

/// Helper to convert `Result<T, Error>` to `Result<T, AuthError>`.
fn to_auth_result<T>(result: Result<T, Error>) -> AuthResult<T> {
    result.map_err(|e| match e {
        Error::Auth(auth_err) => auth_err,
        // Absent is not corrupt: keep the kind so callers can tell them apart.
        Error::Io(io) if io.kind() == std::io::ErrorKind::NotFound => AuthError::TokenStorage(io),
        other => AuthError::TokenFileError(other.to_string()),
    })
}

/// Whether `error` only says no tokens are stored yet (a first run), as
/// opposed to tokens that exist but cannot be read.
pub(crate) fn is_absent(error: &AuthError) -> bool {
    matches!(error, AuthError::TokenStorage(io) if io.kind() == std::io::ErrorKind::NotFound)
}

/// Clock skew tolerance in seconds for token validation.
/// Allows for minor time drift between client and server.
const CLOCK_SKEW_TOLERANCE_SECS: i64 = 5;

/// Minimum recommended refresh buffer in seconds.
const MIN_REFRESH_BUFFER_SECS: i64 = 30;

/// Room for a serialized token set without reallocating: a reallocation
/// would free the old buffer without wiping it. Schwab's tokens serialize to
/// about 2 KiB.
pub(crate) const TOKEN_JSON_CAPACITY: usize = 16 * 1024;

/// Serialize `tokens` into a buffer that is wiped when dropped. Every store
/// writes its plaintext through this, so the only plaintext copies of the
/// secrets are this buffer and the `SecretString`s themselves.
fn token_json(tokens: &TokenSet) -> serde_json::Result<Zeroizing<Vec<u8>>> {
    let mut json = Zeroizing::new(Vec::with_capacity(TOKEN_JSON_CAPACITY));
    serde_json::to_writer(&mut *json, tokens)?;
    Ok(json)
}

fn serialize_secret<S>(secret: &Option<SecretString>, ser: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    match secret {
        Some(s) => ser.serialize_some(s.expose_secret()),
        None => ser.serialize_none(),
    }
}

fn deserialize_secret<'de, D>(de: D) -> Result<Option<SecretString>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Option::<String>::deserialize(de)?.map(SecretString::from))
}

/// Token set with secure memory handling.
///
/// Tokens are wrapped in [`SecretString`] which zeroizes its contents on drop.
/// JSON serialization round-trips the secret values directly without storing
/// duplicate plaintext copies.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenSet {
    /// Access token (30-minute lifetime).
    #[serde(
        serialize_with = "serialize_secret",
        deserialize_with = "deserialize_secret",
        default
    )]
    pub access_token: Option<SecretString>,

    /// Refresh token (7-day lifetime).
    #[serde(
        serialize_with = "serialize_secret",
        deserialize_with = "deserialize_secret",
        default
    )]
    pub refresh_token: Option<SecretString>,

    /// ID token (captured for OpenID Connect compatibility).
    #[serde(
        serialize_with = "serialize_secret",
        deserialize_with = "deserialize_secret",
        default
    )]
    pub id_token: Option<SecretString>,

    pub token_type: String,
    pub expires_in: i64,
    pub scope: String,

    #[serde(with = "chrono::serde::ts_seconds")]
    pub expires_at: DateTime<Utc>,

    #[serde(with = "chrono::serde::ts_seconds")]
    pub issued_at: DateTime<Utc>,

    #[serde(with = "chrono::serde::ts_seconds_option", default)]
    pub refresh_token_expires_at: Option<DateTime<Utc>>,
}

impl TokenSet {
    /// Creates a new TokenSet with secret-wrapped tokens.
    pub fn new(
        access_token: String,
        refresh_token: String,
        id_token: String,
        token_type: String,
        expires_in: i64,
        scope: String,
        issued_at: DateTime<Utc>,
    ) -> Self {
        Self::from_secrets(
            SecretString::from(access_token),
            SecretString::from(refresh_token),
            SecretString::from(id_token),
            token_type,
            expires_in,
            scope,
            issued_at,
        )
    }

    /// [`TokenSet::new`] for tokens that are already secret-wrapped, so a
    /// fresh grant never passes through a plain `String`.
    pub(crate) fn from_secrets(
        access_token: SecretString,
        refresh_token: SecretString,
        id_token: SecretString,
        token_type: String,
        expires_in: i64,
        scope: String,
        issued_at: DateTime<Utc>,
    ) -> Self {
        Self {
            access_token: Some(access_token),
            refresh_token: Some(refresh_token),
            id_token: Some(id_token),
            token_type,
            expires_in,
            scope,
            expires_at: issued_at + chrono::Duration::seconds(expires_in),
            issued_at,
            refresh_token_expires_at: Some(issued_at + chrono::Duration::days(7)),
        }
    }

    /// Returns the access token as an owned `String`.
    ///
    /// The returned `String` is **not** zeroized; treat it as the boundary
    /// between the secret-wrapped storage and the consumer (typically the HTTP
    /// header). Drop it as soon as possible.
    pub fn access_token(&self) -> AuthResult<String> {
        self.access_token
            .as_ref()
            .map(|s| s.expose_secret().to_string())
            .ok_or_else(|| AuthError::MissingConfig("Access token not available".into()))
    }

    /// Returns the refresh token as an owned `String`.
    pub fn refresh_token(&self) -> AuthResult<String> {
        self.refresh_token
            .as_ref()
            .map(|s| s.expose_secret().to_string())
            .ok_or(AuthError::MissingRefreshToken)
    }

    /// Returns the ID token as an owned `String` if available.
    pub fn id_token(&self) -> Option<String> {
        self.id_token
            .as_ref()
            .map(|s| s.expose_secret().to_string())
    }

    pub fn is_valid(&self) -> bool {
        self.is_valid_at(Utc::now())
    }

    /// Check if token is valid at a specific time, with clock skew tolerance.
    pub fn is_valid_at(&self, now: DateTime<Utc>) -> bool {
        let tolerance = chrono::Duration::seconds(CLOCK_SKEW_TOLERANCE_SECS);
        now < (self.expires_at + tolerance)
    }

    pub fn needs_refresh(&self, buffer_seconds: i64) -> bool {
        if buffer_seconds < MIN_REFRESH_BUFFER_SECS {
            tracing::warn!(
                "Refresh buffer very small: {}s (minimum {}s recommended)",
                buffer_seconds,
                MIN_REFRESH_BUFFER_SECS
            );
        }

        if buffer_seconds >= self.expires_in - 10 {
            tracing::warn!(
                "Refresh buffer too large: {}s (token expires in {}s, buffer should be smaller)",
                buffer_seconds,
                self.expires_in
            );
        }

        let refresh_time = self.expires_at - chrono::Duration::seconds(buffer_seconds);
        Utc::now() >= refresh_time
    }

    /// When the refresh token expires: the recorded time, or seven days after
    /// issue for token sets written before it was recorded.
    pub fn refresh_expiry(&self) -> DateTime<Utc> {
        self.refresh_token_expires_at
            .unwrap_or(self.issued_at + chrono::Duration::days(7))
    }

    pub fn refresh_token_expired(&self) -> bool {
        Utc::now() >= self.refresh_expiry()
    }
}

/// Token storage backend trait.
pub trait TokenStore: Send + Sync {
    fn save(&self, tokens: &TokenSet) -> AuthResult<()>;
    fn load(&self) -> AuthResult<TokenSet>;
}

/// Plain file storage with permission checks (Unix: 0600).
///
/// # Security
/// - ✅ Atomic writes (temp file + rename)
/// - ✅ Unix: 0600 permissions enforced
/// - ❌ No encryption at rest
/// - ❌ Plaintext JSON
///
/// **Recommendation:** Use [`EncryptedFileTokenStore`] or [`KeychainTokenStore`] instead.
#[derive(Debug)]
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
        let json = token_json(tokens).map_err(|e| {
            AuthError::TokenStorage(std::io::Error::other(format!(
                "Serialization failed: {}",
                e
            )))
        })?;

        prepare_secret_directory(&self.path)?;
        to_auth_result(secure_file_write(&self.path, &json))?;

        tracing::debug!("Tokens saved to file: {:?}", self.path);
        Ok(())
    }

    fn load(&self) -> AuthResult<TokenSet> {
        let json = to_auth_result(read_private_file(&self.path))?;

        let tokens: TokenSet = serde_json::from_slice(&json).map_err(|e| {
            AuthError::TokenStorage(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Deserialization failed: {}", e),
            ))
        })?;

        tracing::debug!("Tokens loaded from file: {:?}", self.path);
        Ok(tokens)
    }
}

/// Encrypted file storage with ChaCha20Poly1305 AEAD.
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
#[derive(Debug)]
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
        let key_lock = open_lock_file(&self.key_path.with_extension("key.lock"))?;
        // Holders only read or write one small file, so one still holding
        // the lock after this is wedged; see REFRESH_LOCK_WAIT.
        let deadline = std::time::Instant::now() + KEY_LOCK_WAIT;
        loop {
            match key_lock.try_lock() {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(10))
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    return Err(AuthError::TokenFileError(format!(
                        "another process has held the encryption key lock for over {}s",
                        KEY_LOCK_WAIT.as_secs()
                    )));
                }
                Err(std::fs::TryLockError::Error(e)) => return Err(AuthError::TokenStorage(e)),
            }
        }
        // Read, or generate when there is none: no separate existence check
        // for the file to change under.
        let read = match read_private_file(&self.key_path) {
            Err(crate::Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => None,
            other => Some(to_auth_result(other)?),
        };
        if let Some(key_bytes) = read {
            if key_bytes.len() != 32 {
                return Err(AuthError::TokenFileError(format!(
                    "Invalid key length: expected 32, got {}",
                    key_bytes.len()
                )));
            }

            let mut key = Zeroizing::new([0u8; 32]);
            key.copy_from_slice(&key_bytes);
            tracing::debug!("Loaded encryption key from: {:?}", self.key_path);
            Ok(key)
        } else {
            let key = to_auth_result(generate_random_key())?;
            prepare_secret_directory(&self.key_path)?;
            to_auth_result(secure_file_write(&self.key_path, &*key))?;
            tracing::info!("Generated new encryption key: {:?}", self.key_path);
            Ok(key)
        }
    }
}

impl TokenStore for EncryptedFileTokenStore {
    fn save(&self, tokens: &TokenSet) -> AuthResult<()> {
        let key = self.load_or_generate_key()?;

        let json = token_json(tokens)
            .map_err(|e| AuthError::EncryptionFailed(format!("Serialization failed: {}", e)))?;

        let encrypted = to_auth_result(EncryptedToken::encrypt(&json, &key))?;

        let encrypted_json = serde_json::to_vec(&encrypted).map_err(|e| {
            AuthError::EncryptionFailed(format!("Failed to serialize encrypted data: {}", e))
        })?;

        to_auth_result(secure_file_write(&self.path, &encrypted_json))?;

        tracing::debug!("Encrypted tokens saved to: {:?}", self.path);
        Ok(())
    }

    fn load(&self) -> AuthResult<TokenSet> {
        let key = self.load_or_generate_key()?;

        let encrypted_json = to_auth_result(read_private_file(&self.path))?;

        let encrypted: EncryptedToken = serde_json::from_slice(&encrypted_json).map_err(|e| {
            AuthError::DecryptionFailed(format!("Failed to parse encrypted data: {}", e))
        })?;

        // Parse straight out of the zeroizing decryption buffer.
        let decrypted = to_auth_result(encrypted.decrypt(&key))?;

        let tokens: TokenSet = serde_json::from_slice(decrypted.expose_secret())
            .map_err(|e| AuthError::DecryptionFailed(format!("Failed to parse JSON: {}", e)))?;

        tracing::debug!("Decrypted tokens loaded from: {:?}", self.path);
        Ok(tokens)
    }
}

/// The OS credential store, through the `keyring` crate.
///
/// - macOS: the login Keychain (the default store there).
/// - Linux: the kernel keyutils keyrings, which do not survive logout or
///   reboot; use [`EncryptedFileTokenStore`] for a lasting store.
/// - Windows: Credential Manager, whose 2,560-byte blob limit a token set
///   can exceed; [`EncryptedFileTokenStore`] is the safer choice.
#[derive(Debug)]
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
        let json = token_json(tokens)
            .map_err(|e| AuthError::KeyringError(format!("Serialization failed: {}", e)))?;
        let json = std::str::from_utf8(&json)
            .map_err(|e| AuthError::KeyringError(format!("Serialization failed: {}", e)))?;

        let entry = keyring::Entry::new(&self.service, &self.account)
            .map_err(|e| AuthError::KeyringError(format!("Failed to create entry: {}", e)))?;

        entry
            .set_password(json)
            .map_err(|e| AuthError::KeyringError(format!("Failed to save: {}", e)))?;

        tracing::debug!(
            "Tokens saved to keychain: service={}, account={}",
            self.service,
            self.account
        );
        Ok(())
    }

    fn load(&self) -> AuthResult<TokenSet> {
        let entry = keyring::Entry::new(&self.service, &self.account)
            .map_err(|e| AuthError::KeyringError(format!("Failed to create entry: {}", e)))?;

        let json = Zeroizing::new(entry.get_password().map_err(|e| match e {
            keyring::Error::NoEntry => AuthError::TokenStorage(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no tokens in the keychain",
            )),
            e => AuthError::KeyringError(format!("Failed to load: {}", e)),
        })?);

        let tokens: TokenSet = serde_json::from_str(&json)
            .map_err(|e| AuthError::KeyringError(format!("Failed to parse: {}", e)))?;

        tracing::debug!(
            "Tokens loaded from keychain: service={}, account={}",
            self.service,
            self.account
        );
        Ok(tokens)
    }
}

/// How long to wait for another process's refresh. A holder needs at most
/// three 30-second token requests plus backoff; one still holding the lock
/// after this is wedged (stopped, or stuck in I/O), and waiting longer would
/// hang every caller that needs a token.
const REFRESH_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(120);

/// How long to wait for another process's encryption key read or write. Runs
/// on the blocking pool (see `AuthManager::store_load`).
const KEY_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// The guard is held through reload, grant exchange, and persistence. OS locks
/// release on crash; lock files contain no credentials and are never unlinked.
pub(crate) async fn refresh_lock(path: &std::path::Path) -> AuthResult<std::fs::File> {
    refresh_lock_within(path, REFRESH_LOCK_WAIT).await
}

async fn refresh_lock_within(
    path: &std::path::Path,
    wait: std::time::Duration,
) -> AuthResult<std::fs::File> {
    let lock_path = path.with_extension("refresh.lock");
    let file = open_lock_file(&lock_path)?;
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(std::fs::TryLockError::WouldBlock) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(AuthError::RefreshFailed(format!(
                    "another process has held the token refresh lock for over {}s",
                    wait.as_secs()
                )));
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(AuthError::TokenStorage(e)),
        }
    }
}

/// Make sure the directory for a token, key or lock file exists, creating
/// it owner-only if not, and warn when an existing one lets other accounts
/// replace or delete files in it (group/world-writable without the sticky
/// bit). An existing directory is never re-permissioned: it may be a home
/// or project directory the caller chose.
pub(crate) fn prepare_secret_directory(file: &std::path::Path) -> AuthResult<()> {
    let Some(dir) = file.parent().filter(|d| !d.as_os_str().is_empty()) else {
        return Ok(());
    };
    if !dir.exists() {
        // Created owner-only, then checked without following links, so a
        // symlink planted between the existence check and the creation is
        // refused rather than written through.
        return to_auth_result(crate::security::ensure_private_directory(dir));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(dir) {
            let mode = meta.permissions().mode();
            if mode & 0o022 != 0 && mode & 0o1000 == 0 {
                static WARNED: std::sync::Once = std::sync::Once::new();
                WARNED.call_once(|| {
                    tracing::warn!(
                        "The token directory is writable by other accounts; keep token, \
                         key and lock files in a directory only this account can write"
                    );
                });
            }
        }
    }
    Ok(())
}

pub(crate) fn open_lock_file(path: &std::path::Path) -> AuthResult<std::fs::File> {
    prepare_secret_directory(path)?;
    // Lock files hold no secret, but another account able to open one could
    // hold it and stall refreshes: create it owner-only on Windows as well.
    #[cfg(windows)]
    {
        if let Ok(file) = crate::security_windows::secure_create_new(path) {
            return Ok(file);
        }
        // It exists: only an owner-only, non-reparse-point file will do.
        return to_auth_result(crate::security_windows::open_private_file_rw(path));
    }
    #[cfg(unix)]
    {
        // Never through a symlink, and never anything but a regular file (a
        // planted FIFO would otherwise hang the open).
        use rustix::fs::{Mode, OFlags};
        let flags =
            OFlags::CREATE | OFlags::RDWR | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
        let file = match rustix::fs::open(path, flags, Mode::from_raw_mode(0o600)) {
            Ok(fd) => std::fs::File::from(fd),
            Err(rustix::io::Errno::LOOP | rustix::io::Errno::MLINK) => {
                return to_auth_result(Err(crate::security::insecure(path, "is a symbolic link")));
            }
            Err(errno) => return Err(AuthError::TokenStorage(errno.into())),
        };
        if !file.metadata().map_err(AuthError::TokenStorage)?.is_file() {
            return to_auth_result(Err(crate::security::insecure(
                path,
                "is not a regular file",
            )));
        }
        Ok(file)
    }
    #[cfg(not(any(unix, windows)))]
    std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(path)
        .map_err(AuthError::TokenStorage)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    /// A missing token directory is created owner-only.
    #[cfg(unix)]
    #[test]
    fn missing_token_directories_are_created_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("secrets").join("schwab");
        let store = EncryptedFileTokenStore::new(nested.join("tokens.json"));
        store.save(&create_test_tokens()).unwrap();
        let mode = std::fs::metadata(&nested).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
        assert_eq!(
            store.load().unwrap().access_token().unwrap(),
            "access_token_12345"
        );
    }

    /// A lock file is never opened through a symlink, which could point it
    /// at any file the account can write.
    #[cfg(unix)]
    #[test]
    fn lock_files_refuse_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("elsewhere");
        std::fs::write(&target, b"").unwrap();
        let lock = dir.path().join("tokens.refresh.lock");
        std::os::unix::fs::symlink(&target, &lock).unwrap();
        assert!(matches!(
            open_lock_file(&lock),
            Err(AuthError::TokenFileInsecure(_))
        ));
        open_lock_file(&dir.path().join("fresh.lock")).unwrap();
    }

    /// A holder that never lets go makes waiters fail, not hang.
    #[tokio::test]
    async fn refresh_lock_wait_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let tokens = dir.path().join("tokens.json");
        let held = refresh_lock_within(&tokens, std::time::Duration::from_secs(1))
            .await
            .unwrap();
        let waited = refresh_lock_within(&tokens, std::time::Duration::from_millis(50)).await;
        assert!(
            matches!(waited, Err(AuthError::RefreshFailed(_))),
            "{waited:?}"
        );
        drop(held);
        refresh_lock_within(&tokens, std::time::Duration::from_millis(50))
            .await
            .unwrap();
    }

    fn create_test_tokens() -> TokenSet {
        TokenSet::new(
            "access_token_12345".to_string(),
            "refresh_token_67890".to_string(),
            "id_token_abcde".to_string(),
            "Bearer".to_string(),
            1800,
            "api".to_string(),
            Utc::now(),
        )
    }

    #[test]
    fn test_file_store_roundtrip() {
        let temp = NamedTempFile::new().unwrap();
        let store = FileTokenStore::new(temp.path().to_path_buf());
        let tokens = create_test_tokens();

        store.save(&tokens).unwrap();
        let loaded = store.load().unwrap();

        assert_eq!(
            tokens.access_token().unwrap(),
            loaded.access_token().unwrap()
        );
        assert_eq!(
            tokens.refresh_token().unwrap(),
            loaded.refresh_token().unwrap()
        );
    }

    #[test]
    fn test_encrypted_file_store_roundtrip() {
        let temp = NamedTempFile::new().unwrap();
        let store = EncryptedFileTokenStore::new(temp.path().to_path_buf());
        let tokens = create_test_tokens();

        store.save(&tokens).unwrap();
        let loaded = store.load().unwrap();

        assert_eq!(
            tokens.access_token().unwrap(),
            loaded.access_token().unwrap()
        );
        assert_eq!(
            tokens.refresh_token().unwrap(),
            loaded.refresh_token().unwrap()
        );
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

        let mut data = std::fs::read(temp.path()).unwrap();
        if data.len() > 50 {
            data[50] ^= 0xFF;
            std::fs::write(temp.path(), data).unwrap();
        }

        let result = store.load();
        assert!(result.is_err(), "Tampered data should fail to decrypt");
    }
}
