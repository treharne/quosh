//! One-time enrolment nonces.
//!
//! `quosh enroll` runs over SSH, so the SSH login has already authenticated the
//! Unix user. The helper hands back a random nonce bound to that uid; the URL
//! carries only the nonce, never the uid. The browser spends it once, within
//! [`NONCE_TTL_SECS`], to register a passkey.
//!
//! Nonces are in memory only: a server restart invalidates outstanding enrol
//! links, which is cheap to re-run.

use rand::RngCore;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Enrolment link lifetime.
pub const NONCE_TTL_SECS: i64 = 10 * 60;

struct Nonce {
    uid: u32,
    expires: i64,
}

pub struct NonceStore {
    inner: Mutex<HashMap<[u8; 16], Nonce>>,
}

impl NonceStore {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(HashMap::new()),
        })
    }

    /// Mint a single-use nonce for `uid`, dropping any expired entries.
    pub fn issue(&self, uid: u32, now: i64) -> [u8; 16] {
        let mut nonce = [0u8; 16];
        rand::rng().fill_bytes(&mut nonce);
        let mut g = self.inner.lock().expect("nonces poisoned");
        g.retain(|_, n| n.expires > now);
        g.insert(
            nonce,
            Nonce {
                uid,
                expires: now + NONCE_TTL_SECS,
            },
        );
        nonce
    }

    /// Spend a nonce, returning its uid exactly once.
    pub fn consume(&self, nonce: &[u8; 16], now: i64) -> Option<u32> {
        let mut g = self.inner.lock().expect("nonces poisoned");
        let n = g.remove(nonce)?;
        (n.expires > now).then_some(n.uid)
    }

    pub fn len(&self) -> usize {
        self.inner.lock().expect("nonces poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for NonceStore {
    fn default() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consume_is_single_use() {
        let s = NonceStore::default();
        let n = s.issue(42, 1000);
        assert_eq!(s.consume(&n, 1000), Some(42));
        assert_eq!(s.consume(&n, 1000), None);
    }

    #[test]
    fn expired_nonce_is_rejected() {
        let s = NonceStore::default();
        let n = s.issue(42, 1000);
        assert_eq!(s.consume(&n, 1000 + NONCE_TTL_SECS), None);
        assert_eq!(s.consume(&n, 1001 + NONCE_TTL_SECS), None);
    }

    #[test]
    fn issue_prunes_expired_nonces() {
        let s = NonceStore::default();
        s.issue(1, 0);
        s.issue(2, 0);
        assert_eq!(s.len(), 2);
        s.issue(3, NONCE_TTL_SECS + 1);
        assert_eq!(s.len(), 1);
    }
}
