//! Durable device registrations and short-lived session tokens.
//!
//! The passkey is the long-lived credential; a session token only covers an
//! active session so routine transport reconnects are silent. Tokens expire
//! after [`SESSION_IDLE_SECS`] without session activity in either direction and
//! are rotated on every authenticated connect. Storage is a single JSON file
//! under the data directory, written atomically.
//!
//! A token is tied to the credential that authenticated it so
//! [`DeviceStore::revoke_credential`] can drop "that device and its tokens".

use anyhow::{Context, Result};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// A session token expires after this long without activity in either
/// direction.
pub const SESSION_IDLE_SECS: i64 = 60 * 60;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Registration {
    pub uid: u32,
    pub credential_id: String,
    /// Uncompressed SEC1 point, hex.
    pub public_key: String,
    pub rp_id: String,
    pub origin: String,
    pub user_name: String,
    pub sign_count: u32,
    pub created: i64,
    pub last_used: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionToken {
    token: String,
    uid: u32,
    /// The credential that produced this token, when it came from a passkey.
    credential_id: Option<String>,
    last_seen: i64,
}

#[derive(Default, Serialize, Deserialize)]
struct Persisted {
    #[serde(default)]
    registrations: Vec<Registration>,
    #[serde(default)]
    tokens: Vec<SessionToken>,
}

pub struct DeviceStore {
    path: PathBuf,
    inner: Mutex<Persisted>,
}

impl DeviceStore {
    pub fn load(path: &Path) -> Result<Arc<Self>> {
        let inner = match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).context("parse devices.json")?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Persisted::default(),
            Err(e) => return Err(e).context("read devices.json"),
        };
        Ok(Arc::new(Self {
            path: path.to_path_buf(),
            inner: Mutex::new(inner),
        }))
    }

    fn persist(&self, g: &Persisted) -> Result<()> {
        let tmp = self.path.with_extension("tmp");
        let _ = std::fs::remove_file(&tmp);
        {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)
                .with_context(|| format!("create {}", tmp.display()))?;
            f.write_all(&serde_json::to_vec_pretty(g)?)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, &self.path).context("publish devices.json")?;
        Ok(())
    }

    /// Insert or replace a registration by credential id.
    pub fn register(&self, reg: Registration) -> Result<()> {
        let mut g = self.inner.lock().expect("devices poisoned");
        g.registrations
            .retain(|r| r.credential_id != reg.credential_id);
        g.registrations.push(reg);
        self.persist(&g)
    }

    pub fn find(&self, credential_id: &str) -> Option<Registration> {
        let g = self.inner.lock().expect("devices poisoned");
        g.registrations
            .iter()
            .find(|r| r.credential_id == credential_id)
            .cloned()
    }

    pub fn registrations(&self, uid: u32) -> Vec<Registration> {
        let g = self.inner.lock().expect("devices poisoned");
        g.registrations
            .iter()
            .filter(|r| r.uid == uid)
            .cloned()
            .collect()
    }

    /// Record a successful assertion: bump the counter and last-used time.
    pub fn touched(&self, credential_id: &str, sign_count: u32, now: i64) -> Result<()> {
        let mut g = self.inner.lock().expect("devices poisoned");
        if let Some(r) = g
            .registrations
            .iter_mut()
            .find(|r| r.credential_id == credential_id)
        {
            r.sign_count = r.sign_count.max(sign_count);
            r.last_used = Some(now);
        }
        self.persist(&g)
    }

    /// Issue a fresh session token for `uid`.
    pub fn issue_token(&self, uid: u32, credential_id: Option<&str>, now: i64) -> Result<[u8; 32]> {
        let mut token = [0u8; 32];
        rand::rng().fill_bytes(&mut token);
        let mut g = self.inner.lock().expect("devices poisoned");
        g.tokens.push(SessionToken {
            token: hex::encode(token),
            uid,
            credential_id: credential_id.map(str::to_string),
            last_seen: now,
        });
        self.persist(&g)?;
        Ok(token)
    }

    /// Return the uid for a live token, without touching its idle timer.
    pub fn validate_token(&self, token: &[u8; 32], now: i64) -> Option<u32> {
        let g = self.inner.lock().expect("devices poisoned");
        g.tokens
            .iter()
            .find(|t| t.token == hex::encode(token) && now - t.last_seen <= SESSION_IDLE_SECS)
            .map(|t| t.uid)
    }

    /// Extend a token's idle timer; called on any session activity.
    pub fn touch_token(&self, token: &[u8; 32], now: i64) -> Result<()> {
        let mut g = self.inner.lock().expect("devices poisoned");
        if let Some(t) = g.tokens.iter_mut().find(|t| t.token == hex::encode(token)) {
            t.last_seen = now;
            self.persist(&g)?;
        }
        Ok(())
    }

    /// Rotate on authenticated connect: drop `old` and return a new token.
    pub fn rotate_token(
        &self,
        old: Option<&[u8; 32]>,
        uid: u32,
        credential_id: Option<&str>,
        now: i64,
    ) -> Result<[u8; 32]> {
        if let Some(old) = old {
            let old = hex::encode(old);
            let mut g = self.inner.lock().expect("devices poisoned");
            g.tokens.retain(|t| t.token != old);
            self.persist(&g)?;
        }
        self.issue_token(uid, credential_id, now)
    }

    /// Drop one credential and the tokens it issued. Returns true if it existed.
    pub fn revoke_credential(&self, uid: u32, credential_id: &str) -> Result<bool> {
        let mut g = self.inner.lock().expect("devices poisoned");
        let before = g.registrations.len();
        g.registrations
            .retain(|r| !(r.uid == uid && r.credential_id == credential_id));
        let existed = g.registrations.len() != before;
        g.tokens
            .retain(|t| !(t.uid == uid && t.credential_id.as_deref() == Some(credential_id)));
        self.persist(&g)?;
        Ok(existed)
    }

    /// Drop every registration and token for `uid`.
    pub fn revoke_all(&self, uid: u32) -> Result<usize> {
        let mut g = self.inner.lock().expect("devices poisoned");
        let before = g.registrations.iter().filter(|r| r.uid == uid).count();
        g.registrations.retain(|r| r.uid != uid);
        g.tokens.retain(|t| t.uid != uid);
        self.persist(&g)?;
        Ok(before)
    }

    /// Drop only live session tokens; registrations survive, forcing a passkey
    /// on the next connect.
    pub fn revoke_sessions(&self, uid: u32) -> Result<usize> {
        let mut g = self.inner.lock().expect("devices poisoned");
        let before = g.tokens.iter().filter(|t| t.uid == uid).count();
        g.tokens.retain(|t| t.uid != uid);
        self.persist(&g)?;
        Ok(before)
    }

    pub fn tokens(&self, uid: u32) -> Vec<(String, i64)> {
        let g = self.inner.lock().expect("devices poisoned");
        g.tokens
            .iter()
            .filter(|t| t.uid == uid)
            .map(|t| (t.token.clone(), t.last_seen))
            .collect()
    }
}

/// Summary for `quosh devices`.
pub fn summarize(regs: &[Registration]) -> Vec<HashMap<String, String>> {
    regs.iter()
        .map(|r| {
            let mut m = HashMap::new();
            m.insert("credential".into(), short_id(&r.credential_id));
            m.insert("user".into(), r.user_name.clone());
            m.insert("created".into(), r.created.to_string());
            m.insert(
                "last_used".into(),
                r.last_used.map(|t| t.to_string()).unwrap_or_default(),
            );
            m
        })
        .collect()
}

pub fn short_id(hex_id: &str) -> String {
    hex_id.chars().take(16).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(tag: &str) -> (Arc<DeviceStore>, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "quosh-devices-{}-{}-{tag}",
            std::process::id(),
            rand::random::<u32>()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("devices.json");
        (DeviceStore::load(&path).unwrap(), path)
    }

    fn reg(uid: u32, id: &str) -> Registration {
        Registration {
            uid,
            credential_id: id.into(),
            public_key: "04".into(),
            rp_id: "quosh.jtcs.dev".into(),
            origin: "https://quosh.jtcs.dev".into(),
            user_name: format!("u{uid}@host"),
            sign_count: 0,
            created: 100,
            last_used: None,
        }
    }

    #[test]
    fn register_and_find_persist_across_reload() {
        let (s, path) = store("persist");
        s.register(reg(1, "aa")).unwrap();
        let s2 = DeviceStore::load(&path).unwrap();
        assert_eq!(s2.find("aa").unwrap().uid, 1);
    }

    #[test]
    fn re_register_replaces_by_credential_id() {
        let (s, _) = store("replace");
        s.register(reg(1, "aa")).unwrap();
        let mut r = reg(1, "aa");
        r.sign_count = 9;
        s.register(r).unwrap();
        assert_eq!(s.registrations(1).len(), 1);
        assert_eq!(s.find("aa").unwrap().sign_count, 9);
    }

    #[test]
    fn token_lifecycle_and_expiry() {
        let (s, _) = store("token");
        let t = s.issue_token(7, Some("aa"), 1000).unwrap();
        assert_eq!(s.validate_token(&t, 1000 + SESSION_IDLE_SECS), Some(7));
        assert_eq!(s.validate_token(&t, 1001 + SESSION_IDLE_SECS), None);
        s.touch_token(&t, 2000).unwrap();
        assert_eq!(s.validate_token(&t, 2000 + SESSION_IDLE_SECS), Some(7));
    }

    #[test]
    fn rotate_invalidates_the_old_token() {
        let (s, _) = store("rotate");
        let t1 = s.issue_token(7, None, 0).unwrap();
        let t2 = s.rotate_token(Some(&t1), 7, None, 0).unwrap();
        assert_eq!(s.validate_token(&t1, 0), None);
        assert_eq!(s.validate_token(&t2, 0), Some(7));
    }

    #[test]
    fn revoke_credential_drops_its_tokens_only() {
        let (s, _) = store("revoke");
        s.register(reg(1, "aa")).unwrap();
        s.register(reg(1, "bb")).unwrap();
        let ta = s.issue_token(1, Some("aa"), 0).unwrap();
        let tb = s.issue_token(1, Some("bb"), 0).unwrap();
        assert!(s.revoke_credential(1, "aa").unwrap());
        assert!(s.find("aa").is_none());
        assert!(s.find("bb").is_some());
        assert_eq!(s.validate_token(&ta, 0), None);
        assert_eq!(s.validate_token(&tb, 0), Some(1));
    }

    #[test]
    fn revoke_all_and_sessions_scopes() {
        let (s, _) = store("scopes");
        s.register(reg(1, "aa")).unwrap();
        s.register(reg(2, "cc")).unwrap();
        let t1 = s.issue_token(1, None, 0).unwrap();
        let t2 = s.issue_token(2, None, 0).unwrap();
        assert_eq!(s.revoke_sessions(1).unwrap(), 1);
        assert_eq!(s.validate_token(&t1, 0), None);
        assert_eq!(s.validate_token(&t2, 0), Some(2));
        assert_eq!(s.revoke_all(1).unwrap(), 1);
        assert!(s.registrations(1).is_empty());
        assert_eq!(s.registrations(2).len(), 1);
    }
}
