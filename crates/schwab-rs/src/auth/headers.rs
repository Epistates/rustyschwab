//! HTTP Authorization header utilities
//!
//! Provides functions to create properly formatted authorization headers
//! for different authentication schemes (Bearer, Basic).

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};

/// Create a Bearer token authorization header value
///
/// # Arguments
/// * `access_token` - The OAuth2 access token
///
/// # Returns
/// The complete header value ready to use with `Authorization` header
///
/// # Example
/// ```ignore
/// let header = create_bearer_header("my_token");
/// assert_eq!(header, "Bearer my_token");
/// ```
pub fn create_bearer_header(access_token: &str) -> String {
    format!("Bearer {}", access_token)
}

/// Create a Basic authentication header value
///
/// Encodes credentials in the format required by HTTP Basic Authentication (RFC 7617).
/// The credentials are combined as "username:password" and base64-encoded.
///
/// # Arguments
/// * `app_key` - The application key (username)
/// * `app_secret` - The application secret (password)
///
/// # Returns
/// The complete header value ready to use with `Authorization` header
///
/// # Example
/// ```ignore
/// let header = create_basic_header("my_app_key", "my_secret");
/// assert_eq!(header, "Basic bXlfYXBwX2tleTpteV9zZWNyZXQ=");
/// ```
pub fn create_basic_header(app_key: &str, app_secret: &str) -> String {
    let auth_string = format!("{}:{}", app_key, app_secret);
    let auth_bytes = auth_string.as_bytes();
    format!("Basic {}", BASE64.encode(auth_bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_bearer_header() {
        let header = create_bearer_header("test_token");
        assert_eq!(header, "Bearer test_token");
    }

    #[test]
    fn test_create_bearer_header_with_special_chars() {
        let header = create_bearer_header("token.with.dots");
        assert_eq!(header, "Bearer token.with.dots");
    }

    #[test]
    fn test_create_basic_header() {
        let header = create_basic_header("user", "pass");
        // "user:pass" base64 encoded is "dXNlcjpwYXNz"
        assert_eq!(header, "Basic dXNlcjpwYXNz");
    }

    #[test]
    fn test_create_basic_header_with_special_chars() {
        let header = create_basic_header("user@domain", "pass:word");
        // Verify it's properly base64 encoded
        assert!(header.starts_with("Basic "));
        assert!(header.len() > 6);
    }
}
