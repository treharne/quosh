//! Server identity and the rotating certificate chain.
//!
//! Browsers cannot use the Web PKI to trust a server with no public name or
//! public CA, so the PWA pins certificates by hash (the WebTransport
//! `serverCertificateHashes` option). That API only accepts leaf certificates
//! with a validity period of at most 14 days, which forces short-lived certs
//! and therefore rotation.
//!
//! To make rotation survivable for an already-enrolled client we keep a single
//! long-lived identity key under the data directory and derive a chain of
//! certificates from it: epoch `k` covers `[anchor + k*STRIDE, ...]` with a
//! `VALIDITY` that is slightly longer than `STRIDE`, so there is a one-day
//! overlap. An enrolled client pins the hashes of the whole current window (see
//! [`CertChain::forward_hashes`]) and can therefore reconnect across rotations
//! without re-enrolling. The chain is topped up on every authenticated connect
//! (and lazily on the first handshake of a new epoch).
//!
//! Certificate bytes are persisted, not regenerated: ECDSA signatures are
//! randomised, so regenerating a certificate would change its hash and break
//! every client that pinned it.

use anyhow::{Context, Result};
use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair, PKCS_ECDSA_P256_SHA256};
use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::{CertifiedKey, SigningKey};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use time::{Duration, OffsetDateTime};
use wtransport::tls::WEBTRANSPORT_ALPN;

/// Time between successive certificates (the epoch length).
const STRIDE: Duration = Duration::days(13);
/// Total validity of each certificate. Must be `<= 14 days` for the browser
/// `serverCertificateHashes` API. Longer than `STRIDE`, giving a one-day
/// overlap so a valid certificate is always available to serve.
const VALIDITY: Duration = Duration::days(14);
/// Backdate `notBefore` so a freshly minted certificate is valid immediately
/// even when the client's clock lags the server's. Counts against `VALIDITY`,
/// which is why it must be small.
const SLACK: Duration = Duration::minutes(5);
/// Default number of certificates in the rotation window.
pub const DEFAULT_CHAIN: usize = 7;

struct Entry {
    key: Arc<CertifiedKey>,
    hash: [u8; 32],
}

/// A single long-lived identity key plus a window of short-lived certificates
/// derived from it, and the rustls resolver that serves the current one.
pub struct CertChain {
    dir: PathBuf,
    key: KeyPair,
    signing: Arc<dyn SigningKey>,
    anchor: i64,
    len: usize,
    entries: Mutex<BTreeMap<u64, Entry>>,
}

impl CertChain {
    /// Load (or create) the identity key and anchor in `dir`, then pre-generate
    /// the `len` certificates covering the current epoch and those after it.
    pub fn load(dir: &Path, len: usize) -> Result<Arc<Self>> {
        std::fs::create_dir_all(dir).context("tls dir")?;
        // Sanity bound: a window of more than 64 certificates (~2.2 years) is
        // almost certainly a misconfiguration, and we pre-generate all of them.
        let len = len.clamp(1, 64);
        migrate_legacy_key(dir);
        let key = load_or_generate_key(&dir.join("identity.pem"))?;
        let private_key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der()));
        let signing = rustls::crypto::ring::default_provider()
            .key_provider
            .load_private_key(private_key)
            .context("load identity key")?;
        let anchor = load_or_create_anchor(&dir.join("chain-anchor"))?;
        let chain = Arc::new(Self {
            dir: dir.to_path_buf(),
            key,
            signing,
            anchor,
            len,
            entries: Mutex::new(BTreeMap::new()),
        });
        let base = chain.epoch_at(now_unix());
        for epoch in base..base + chain.len as u64 {
            chain.entry(epoch)?;
        }
        Ok(chain)
    }

    /// Epoch index covering `now`, clamped at zero if the clock runs backwards.
    fn epoch_at(&self, now: i64) -> u64 {
        if now <= self.anchor {
            return 0;
        }
        ((now - self.anchor) / STRIDE.whole_seconds()) as u64
    }

    /// Inclusive `notBefore`, exclusive `notAfter` for `epoch`, as Unix seconds.
    fn window(&self, epoch: u64) -> (i64, i64) {
        let start = self.anchor + epoch as i64 * STRIDE.whole_seconds() - SLACK.whole_seconds();
        (start, start + VALIDITY.whole_seconds())
    }

    /// Return the cached certificate for `epoch`, generating and persisting it
    /// on first use. Holds the lock across generation so two handshakes cannot
    /// mint two different (randomised-signature) certificates for one epoch.
    fn entry(&self, epoch: u64) -> Result<Entry> {
        let mut map = self.entries.lock().expect("cert cache poisoned");
        if let Some(e) = map.get(&epoch) {
            return Ok(Entry {
                key: e.key.clone(),
                hash: e.hash,
            });
        }
        let (not_before, not_after) = self.window(epoch);
        let der = self.load_or_generate_cert(epoch, not_before, not_after)?;
        let hash: [u8; 32] = Sha256::digest(der.as_ref()).into();
        let key = Arc::new(CertifiedKey::new(vec![der], self.signing.clone()));
        map.insert(
            epoch,
            Entry {
                key: key.clone(),
                hash,
            },
        );
        Ok(Entry { key, hash })
    }

    fn load_or_generate_cert(
        &self,
        epoch: u64,
        not_before: i64,
        not_after: i64,
    ) -> Result<CertificateDer<'static>> {
        let path = self.dir.join(format!("cert-{epoch:08}.der"));
        if let Ok(bytes) = std::fs::read(&path) {
            return Ok(CertificateDer::from(bytes));
        }
        let mut params =
            CertificateParams::new(vec!["localhost".into(), "127.0.0.1".into(), "::1".into()])?;
        params.not_before =
            OffsetDateTime::from_unix_timestamp(not_before).context("not_before")?;
        params.not_after = OffsetDateTime::from_unix_timestamp(not_after).context("not_after")?;
        params.distinguished_name = DistinguishedName::new();
        params.distinguished_name.push(DnType::CommonName, "quosh");
        let cert = params.self_signed(&self.key)?;
        let der = cert.der().clone();
        write_atomic(&path, der.as_ref(), 0o644)?;
        // Prune relative to the epoch being served now, not the (possibly
        // future) epoch we just minted.
        self.prune(self.epoch_at(now_unix()));
        Ok(der)
    }

    /// Remove the DER files for epochs that can no longer be served, keeping
    /// the previous one so a server clock that briefly lags still has it.
    fn prune(&self, keep_from: u64) {
        let Ok(rd) = std::fs::read_dir(&self.dir) else {
            return;
        };
        for e in rd.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            let Some(k) = name
                .strip_prefix("cert-")
                .and_then(|r| r.strip_suffix(".der"))
                .and_then(|r| r.parse::<u64>().ok())
            else {
                continue;
            };
            if k + 1 < keep_from {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }

    /// The certificate to serve now.
    pub fn current(&self) -> Result<Arc<CertifiedKey>> {
        Ok(self.entry(self.epoch_at(now_unix()))?.key)
    }

    /// SHA-256 of the DER of the certificate served now.
    pub fn current_hash(&self) -> Result<[u8; 32]> {
        Ok(self.entry(self.epoch_at(now_unix()))?.hash)
    }

    /// Hashes of the current certificate and the next `len - 1`, in order. An
    /// enrolled client pins all of them so it survives rotation.
    pub fn forward_hashes(&self) -> Result<Vec<[u8; 32]>> {
        let base = self.epoch_at(now_unix());
        (base..base + self.len as u64)
            .map(|k| Ok(self.entry(k)?.hash))
            .collect()
    }

    /// rustls configuration that serves the certificate for the current epoch,
    /// chosen per handshake by [`Resolver`]. This lets the endpoint rotate the
    /// certificate without a restart.
    pub fn tls_config(self: &Arc<Self>) -> Result<ServerConfig> {
        let builder =
            ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_protocol_versions(&[&rustls::version::TLS13])?;
        let mut config = builder
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(Resolver {
                chain: self.clone(),
            }));
        config.alpn_protocols = vec![WEBTRANSPORT_ALPN.to_vec()];
        Ok(config)
    }
}

/// Resolves the certificate for the current epoch on each handshake.
struct Resolver {
    chain: Arc<CertChain>,
}

impl ResolvesServerCert for Resolver {
    fn resolve(&self, _hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        match self.chain.current() {
            Ok(key) => Some(key),
            Err(e) => {
                tracing::error!("certificate resolution failed: {e:#}");
                None
            }
        }
    }
}

impl fmt::Debug for Resolver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("quosh::cert::Resolver")
    }
}

fn now_unix() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp()
}

/// The pre-chain server stored a single `key.pem`. Adopt it as the long-lived
/// identity key so upgrading does not change the identity material.
fn migrate_legacy_key(dir: &Path) {
    let identity = dir.join("identity.pem");
    let legacy = dir.join("key.pem");
    if !identity.exists() && legacy.exists() {
        let _ = std::fs::rename(&legacy, &identity);
    }
}

fn load_or_generate_key(path: &Path) -> Result<KeyPair> {
    if let Ok(pem) = std::fs::read_to_string(path) {
        return KeyPair::from_pem(&pem).context("parse identity key");
    }
    let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)?;
    write_atomic(path, key.serialize_pem().as_bytes(), 0o600).context("persist identity key")?;
    Ok(key)
}

fn load_or_create_anchor(path: &Path) -> Result<i64> {
    if let Ok(s) = std::fs::read_to_string(path)
        && let Ok(v) = s.trim().parse::<i64>()
    {
        return Ok(v);
    }
    let now = now_unix();
    write_atomic(path, now.to_string().as_bytes(), 0o644).context("persist chain anchor")?;
    Ok(now)
}

/// Write `data` to `path` via a temporary file and rename, so a crash cannot
/// leave a half-written certificate or key behind.
fn write_atomic(path: &Path, data: &[u8], mode: u32) -> Result<()> {
    let tmp = path.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&tmp)
            .with_context(|| format!("create {}", tmp.display()))?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("publish {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "quosh-cert-{}-{}-{tag}",
            std::process::id(),
            rand::random::<u32>()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn chain_pre_generates_the_window_and_persists_der() {
        let dir = tmpdir("window");
        let chain = CertChain::load(&dir, 3).unwrap();
        let hashes = chain.forward_hashes().unwrap();
        assert_eq!(hashes.len(), 3);
        assert_eq!(hashes[0], chain.current_hash().unwrap());
        // Every epoch in the window is on disk.
        let count = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("cert-"))
            .count();
        assert_eq!(count, 3);
        // ... and the identity key is present with restrictive permissions.
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.join("identity.pem"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn hashes_are_stable_across_reloads() {
        let dir = tmpdir("stable");
        let before = CertChain::load(&dir, 3).unwrap().forward_hashes().unwrap();
        let after = CertChain::load(&dir, 3).unwrap().forward_hashes().unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn windows_step_by_stride_and_overlap_by_a_day() {
        let dir = tmpdir("windows");
        let chain = CertChain::load(&dir, 2).unwrap();
        let (nb0, na0) = chain.window(0);
        let (nb1, na1) = chain.window(1);
        assert_eq!(nb1 - nb0, STRIDE.whole_seconds());
        assert_eq!(na0 - nb0, VALIDITY.whole_seconds());
        // Epoch 0 is still valid when epoch 1 starts: the overlap.
        assert!(nb1 < na0);
        assert_eq!(na0 - nb1, (VALIDITY - STRIDE).whole_seconds());
        assert_eq!(na1 - nb1, VALIDITY.whole_seconds());
    }

    #[test]
    fn epoch_rollover_generates_and_prunes() {
        let dir = tmpdir("rollover");
        let chain = CertChain::load(&dir, 2).unwrap();
        let base = chain.epoch_at(now_unix());
        // Pretend a whole window has elapsed: later epochs are minted on demand
        // and the long-expired ones are pruned.
        let far = now_unix() + (chain.len as i64 + 2) * STRIDE.whole_seconds();
        let epoch = chain.epoch_at(far);
        assert!(epoch > base);
        let entry = chain.entry(epoch).unwrap();
        assert_eq!(entry.hash, chain.entry(epoch).unwrap().hash);
        assert!(dir.join(format!("cert-{epoch:08}.der")).exists());
        // Simulate the clock reaching `far`: the long-expired epochs go away.
        chain.prune(chain.epoch_at(far));
        assert!(dir.join(format!("cert-{epoch:08}.der")).exists());
        assert!(!dir.join(format!("cert-{base:08}.der")).exists());
    }

    #[test]
    fn current_certificate_is_valid_now() {
        let dir = tmpdir("valid");
        let chain = CertChain::load(&dir, 1).unwrap();
        let epoch = chain.epoch_at(now_unix());
        let (not_before, not_after) = chain.window(epoch);
        assert!(not_before <= now_unix());
        assert!(now_unix() < not_after);
    }

    #[test]
    fn tls_config_has_webtransport_alpn() {
        let dir = tmpdir("alpn");
        let chain = CertChain::load(&dir, 1).unwrap();
        let config = chain.tls_config().unwrap();
        assert_eq!(config.alpn_protocols, vec![WEBTRANSPORT_ALPN.to_vec()]);
    }
}
