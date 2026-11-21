//! Single-use, expiring WebSocket tokens.
//!
//! The whole store is pure: every mutation takes an explicit `now`, which
//! makes expiry deterministic in tests.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use base64::Engine as _;

/// Length of the random secret in bytes.
pub const TOKEN_BYTES: usize = 32;

/// A minted token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    /// Opaque, URL-safe, unguessable value.
    pub secret: String,
    /// `host:port` of the RDP listener to splice towards.
    pub target: String,
    /// Authenticated user, for logging.
    pub user: String,
    /// Absolute expiry.
    pub expires_at: Instant,
}

/// In-memory token store, guarded by a single lock in [`crate::state::AppState`].
#[derive(Debug, Default)]
pub struct TokenStore {
    tokens: HashMap<String, Token>,
    ttl: Duration,
}

impl TokenStore {
    /// Store with the TTL taken from `web.token_ttl_minutes`.
    pub fn new(ttl_minutes: u32) -> Self {
        Self {
            tokens: HashMap::new(),
            ttl: Duration::from_secs(u64::from(ttl_minutes.max(1)) * 60),
        }
    }

    /// Configured lifetime of a token.
    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    /// Number of live (unexpired, unconsumed) tokens.
    pub fn len(&self) -> usize {
        self.tokens.len()
    }

    /// Whether the store is empty.
    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }

    /// Issue a token for `user` targeting `target`.
    pub fn issue(&mut self, user: &str, target: &str, now: Instant) -> Token {
        let token = Token {
            secret: random_secret(),
            target: target.to_string(),
            user: user.to_string(),
            expires_at: now + self.ttl,
        };
        self.tokens.insert(token.secret.clone(), token.clone());
        token
    }

    /// Redeem `presented`: the token must exist, match in constant time, not be
    /// expired, and is removed whether or not it was still valid (single use).
    ///
    /// Returns the token that was consumed.
    pub fn consume(&mut self, presented: &str, now: Instant) -> Option<Token> {
        self.consume_detailed(presented, now).ok()
    }

    /// [`Self::consume`] with a reason, so the gateway can answer an expired
    /// token with a different RDCleanPath error code than an unknown one.
    pub fn consume_detailed(&mut self, presented: &str, now: Instant) -> Result<Token, TokenError> {
        let matching = self
            .tokens
            .keys()
            .find(|secret| constant_time_eq(secret.as_bytes(), presented.as_bytes()))
            .ok_or(TokenError::Unknown)?
            .clone();
        // Single use: the token is gone no matter what happens next.
        let token = self.tokens.remove(&matching);
        match token {
            Some(token) if token.expires_at > now => Ok(token),
            Some(_) => Err(TokenError::Expired),
            None => Err(TokenError::Unknown),
        }
    }

    /// Drop expired tokens; returns how many were removed.
    pub fn reap(&mut self, now: Instant) -> usize {
        let before = self.tokens.len();
        self.tokens.retain(|_, token| token.expires_at > now);
        before - self.tokens.len()
    }

    /// Replace the TTL (used when the config is reloaded).
    pub fn set_ttl(&mut self, ttl: Duration) {
        self.ttl = ttl;
    }
}

/// Why a presented token was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenError {
    /// Never issued, or already used.
    Unknown,
    /// Issued, but past `web.token_ttl_minutes`.
    Expired,
}

impl std::fmt::Display for TokenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown => f.write_str("unknown or already-used token"),
            Self::Expired => f.write_str("expired token"),
        }
    }
}

impl std::error::Error for TokenError {}

/// 32 random bytes, URL-safe base64 without padding (64 chars, no `+`/`/`).
pub fn random_secret() -> String {
    let mut buf = [0u8; TOKEN_BYTES];
    rand::fill(&mut buf);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
}

/// Length-preserving, branch-minimising comparison.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_is_unguessable_and_url_safe() {
        let a = random_secret();
        let b = random_secret();
        assert_eq!(a.len(), 43, "32 bytes of base64url without padding");
        assert_ne!(a, b);
        assert!(
            a.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
    }

    #[test]
    fn token_is_single_use() {
        let t0 = Instant::now();
        let mut store = TokenStore::new(5);
        let token = store.issue("alice", "127.0.0.1:3389", t0);
        assert_eq!(store.len(), 1);

        let consumed = store.consume(&token.secret, t0).expect("first use works");
        assert_eq!(consumed.target, "127.0.0.1:3389");
        assert_eq!(consumed.user, "alice");
        assert!(store.is_empty());
        assert!(
            store.consume(&token.secret, t0).is_none(),
            "second use fails"
        );
    }

    #[test]
    fn wrong_secret_is_rejected() {
        let t0 = Instant::now();
        let mut store = TokenStore::new(5);
        store.issue("alice", "127.0.0.1:3389", t0);
        assert!(store.consume("not-the-token", t0).is_none());
        assert_eq!(store.len(), 1, "a miss must not consume anything");
    }

    #[test]
    fn token_expires() {
        let t0 = Instant::now();
        let mut store = TokenStore::new(1); // 60s TTL
        let token = store.issue("alice", "127.0.0.1:3389", t0);
        assert!(
            store
                .consume(&token.secret, t0 + Duration::from_secs(59))
                .is_some()
        );

        let token = store.issue("bob", "127.0.0.1:3389", t0);
        assert_eq!(store.ttl(), Duration::from_secs(60));
        assert!(
            store
                .consume(&token.secret, t0 + Duration::from_secs(61))
                .is_none(),
            "expired token is rejected"
        );
        assert!(store.is_empty(), "an expired token is still consumed");
    }

    #[test]
    fn reap_removes_only_expired() {
        let t0 = Instant::now();
        let mut store = TokenStore::new(1);
        let short = store.issue("a", "127.0.0.1:3389", t0);
        store.set_ttl(Duration::from_secs(600));
        let long = store.issue("b", "127.0.0.1:3389", t0);

        assert_eq!(store.reap(t0), 0);
        assert_eq!(store.reap(t0 + Duration::from_secs(61)), 1);
        assert!(
            store
                .consume(&long.secret, t0 + Duration::from_secs(61))
                .is_some()
        );
        assert!(store.consume(&short.secret, t0).is_none());
    }

    #[test]
    fn zero_ttl_is_clamped_to_one_minute() {
        let store = TokenStore::new(0);
        assert_eq!(store.ttl(), Duration::from_secs(60));
    }

    #[test]
    fn constant_time_comparison() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(constant_time_eq(b"", b""));
    }
}
