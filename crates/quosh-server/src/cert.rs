use anyhow::{Context, Result};
use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair, PKCS_ECDSA_P256_SHA256};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use time::{Duration, OffsetDateTime};

pub struct TlsBits {
    pub cert_pem: PathBuf,
    pub key_pem: PathBuf,
    pub sha256: [u8; 32],
}

pub fn load_or_generate(dir: &Path) -> Result<TlsBits> {
    std::fs::create_dir_all(dir).context("tls dir")?;
    let cert_pem = dir.join("cert.pem");
    let key_pem = dir.join("key.pem");
    if !cert_pem.exists() || !key_pem.exists() {
        generate(&cert_pem, &key_pem)?;
    }
    let mut rdr = std::io::BufReader::new(std::fs::File::open(&cert_pem)?);
    let der = rustls_pemfile::certs(&mut rdr)
        .next()
        .context("cert pem empty")?
        .context("cert pem parse")?;
    let mut sha256 = [0u8; 32];
    sha256.copy_from_slice(&Sha256::digest(der.as_ref()));
    Ok(TlsBits {
        cert_pem,
        key_pem,
        sha256,
    })
}

fn generate(cert_pem: &Path, key_pem: &Path) -> Result<()> {
    let mut params =
        CertificateParams::new(vec!["localhost".into(), "127.0.0.1".into(), "::1".into()])?;
    params.not_before = OffsetDateTime::now_utc() - Duration::minutes(5);
    params.not_after = OffsetDateTime::now_utc() + Duration::days(13);
    params.distinguished_name = DistinguishedName::new();
    params.distinguished_name.push(DnType::CommonName, "quosh");
    let key_pair = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)?;
    let cert = params.self_signed(&key_pair)?;

    let dir = key_pem.parent().unwrap_or(Path::new("."));
    let tmp_key = dir.join(".key.pem.tmp");
    let tmp_cert = dir.join(".cert.pem.tmp");
    let _ = std::fs::remove_file(&tmp_key);
    let _ = std::fs::remove_file(&tmp_cert);

    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp_key)
            .context("create key tmp")?;
        f.write_all(key_pair.serialize_pem().as_bytes())?;
        f.sync_all()?;
    }
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .open(&tmp_cert)
            .context("create cert tmp")?;
        f.write_all(cert.pem().as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp_cert, cert_pem).context("publish cert")?;
    std::fs::rename(&tmp_key, key_pem).context("publish key")?;
    Ok(())
}
