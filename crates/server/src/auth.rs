use anyhow::{Context, Result};
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, TokenData, Validation};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

/// JWT claims for authenticated sessions.
#[derive(Debug, Serialize, Deserialize)]
pub struct Claims {
    /// Subject (username)
    pub sub: String,
    /// Expiration time (Unix timestamp)
    pub exp: u64,
    /// Issued at (Unix timestamp)
    pub iat: u64,
}

const TOKEN_EXPIRY_SECS: u64 = 24 * 60 * 60; // 24 hours

/// Authenticate a user via Linux PAM.
///
/// Returns `Ok(())` if credentials are valid, or an error describing the failure.
/// NOTE: This is a blocking call. Wrap in `tokio::task::spawn_blocking`.
pub fn authenticate_pam(username: &str, password: &str) -> Result<()> {
    let mut client =
        pam::Client::with_password("beam").map_err(|e| anyhow::anyhow!("PAM init failed: {e}"))?;

    client
        .conversation_mut()
        .set_credentials(username, password);

    client
        .authenticate()
        .map_err(|e| anyhow::anyhow!("Authentication failed: {e}"))?;

    Ok(())
}

/// Generate a JWT token for an authenticated user.
pub fn generate_jwt(username: &str, secret: &str) -> Result<String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("System clock error")?
        .as_secs();

    let claims = Claims {
        sub: username.to_string(),
        iat: now,
        exp: now + TOKEN_EXPIRY_SECS,
    };

    let token = jsonwebtoken::encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .context("Failed to encode JWT")?;

    Ok(token)
}

/// Validate a JWT token and return the claims.
pub fn validate_jwt(token: &str, secret: &str) -> Result<Claims> {
    let validation = Validation::new(Algorithm::HS256);

    let token_data: TokenData<Claims> = jsonwebtoken::decode(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &validation,
    )
    .context("Invalid or expired token")?;

    Ok(token_data.claims)
}

/// Grace period for token refresh (5 minutes after expiry).
const REFRESH_GRACE_SECS: u64 = 5 * 60;

/// Validate a JWT for refresh purposes, allowing recently-expired tokens.
/// Returns claims if the token is valid OR expired within the grace window.
pub fn validate_jwt_for_refresh(token: &str, secret: &str) -> Result<Claims> {
    // Try normal validation first (token not yet expired)
    if let Ok(claims) = validate_jwt(token, secret) {
        return Ok(claims);
    }

    // Token is invalid — check if it's just expired (vs tampered/wrong secret)
    let mut validation = Validation::new(Algorithm::HS256);
    validation.validate_exp = false;

    let token_data: TokenData<Claims> = jsonwebtoken::decode(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &validation,
    )
    .context("Invalid token")?;

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("System clock error")?
        .as_secs();

    if now > token_data.claims.exp + REFRESH_GRACE_SECS {
        anyhow::bail!("Token expired beyond grace period");
    }

    Ok(token_data.claims)
}

/// Generate a cryptographically secure random JWT secret.
///
/// Uses `/dev/urandom` for CSPRNG on Linux.
pub fn generate_secret() -> String {
    use std::fmt::Write;
    // Read from /dev/urandom for CSPRNG
    let mut bytes = [0u8; 32];
    let f = std::fs::File::open("/dev/urandom").expect("Failed to open /dev/urandom");
    use std::io::Read;
    (&f).read_exact(&mut bytes)
        .expect("Failed to read random bytes");
    let mut hex = String::with_capacity(64);
    for b in &bytes {
        write!(hex, "{b:02x}").unwrap();
    }
    hex
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jwt_roundtrip() {
        let secret = "test-secret-for-jwt";
        let token = generate_jwt("testuser", secret).unwrap();
        let claims = validate_jwt(&token, secret).unwrap();
        assert_eq!(claims.sub, "testuser");
        assert!(claims.exp > claims.iat);
        assert_eq!(claims.exp - claims.iat, TOKEN_EXPIRY_SECS);
    }

    #[test]
    fn jwt_rejects_wrong_secret() {
        let token = generate_jwt("testuser", "correct-secret").unwrap();
        let result = validate_jwt(&token, "wrong-secret");
        assert!(result.is_err());
    }

    #[test]
    fn jwt_rejects_garbage() {
        let result = validate_jwt("not.a.token", "secret");
        assert!(result.is_err());
    }

    #[test]
    fn jwt_refresh_accepts_valid_token() {
        let secret = "test-secret";
        let token = generate_jwt("testuser", secret).unwrap();
        let claims = validate_jwt_for_refresh(&token, secret).unwrap();
        assert_eq!(claims.sub, "testuser");
    }

    #[test]
    fn jwt_refresh_rejects_wrong_secret() {
        let token = generate_jwt("testuser", "correct-secret").unwrap();
        assert!(validate_jwt_for_refresh(&token, "wrong-secret").is_err());
    }

    #[test]
    fn jwt_refresh_rejects_long_expired_token() {
        let secret = "test-secret";
        // Create a token that expired long ago
        let old_claims = Claims {
            sub: "testuser".to_string(),
            iat: 1000,
            exp: 1000 + TOKEN_EXPIRY_SECS,
        };
        let token = jsonwebtoken::encode(
            &Header::default(),
            &old_claims,
            &EncodingKey::from_secret(secret.as_bytes()),
        )
        .unwrap();
        assert!(validate_jwt_for_refresh(&token, secret).is_err());
    }

    #[test]
    fn generate_secret_is_64_hex_chars() {
        let secret = generate_secret();
        assert_eq!(secret.len(), 64);
        assert!(secret.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn generate_secret_is_unique() {
        let s1 = generate_secret();
        let s2 = generate_secret();
        assert_ne!(s1, s2);
    }
}
