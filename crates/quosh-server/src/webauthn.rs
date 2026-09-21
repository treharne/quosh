//! Minimal WebAuthn registration and assertion verification.
//!
//! The PWA uses passkeys as its durable user credential. The browser performs
//! the ceremony; the server has to check the result. This module does exactly
//! that and nothing else:
//!
//! * We require `attestation: "none"`, so registration only needs the public
//!   key out of the authenticator data — there is no attestation statement to
//!   verify.
//! * We require user presence **and** user verification, because a passkey is
//!   meant to be gated by the device's biometric/PIN.
//! * Only ES256 (COSE alg `-7`) over P-256 (crv `1`) is accepted; that is what
//!   every passkey provider emits and what `ring` verifies without extra
//!   dependencies.
//!
//! Everything is validated against a server-generated challenge, the expected
//! origin, and the Relying Party ID; none of it is taken from the network.

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::signature::{ECDSA_P256_SHA256_ASN1, UnparsedPublicKey};
use serde::Deserialize;
use sha2::{Digest, Sha256};

const FLAG_UP: u8 = 0x01;
const FLAG_UV: u8 = 0x04;
const FLAG_AT: u8 = 0x40;
const FLAG_ED: u8 = 0x80;

const COSE_KTY_EC2: u64 = 2;
const COSE_ALG_ES256: i64 = -7;
const COSE_CRV_P256: i64 = 1;

/// A credential as extracted from a registration ceremony.
#[derive(Debug, Clone)]
pub struct Registered {
    pub credential_id: Vec<u8>,
    /// Uncompressed SEC1 point (`0x04 || x || y`), the form `ring` verifies.
    pub public_key: Vec<u8>,
    pub sign_count: u32,
}

#[derive(Deserialize)]
struct ClientData {
    #[serde(rename = "type")]
    typ: String,
    challenge: String,
    origin: String,
    #[serde(rename = "crossOrigin", default)]
    cross_origin: bool,
}

/// Validate the parts of `clientDataJSON` that are common to both ceremonies.
fn check_client_data(
    raw: &[u8],
    expected_type: &str,
    origin: &str,
    challenge: &[u8],
) -> Result<()> {
    let data: ClientData = serde_json::from_slice(raw).context("clientDataJSON")?;
    if data.typ != expected_type {
        bail!("clientDataJSON type {:?} != {expected_type}", data.typ);
    }
    if data.origin != origin {
        bail!("origin {:?} != {origin}", data.origin);
    }
    if data.cross_origin {
        bail!("crossOrigin must not be set");
    }
    if data.challenge != URL_SAFE_NO_PAD.encode(challenge) {
        bail!("challenge mismatch");
    }
    Ok(())
}

struct AuthData<'a> {
    rp_id_hash: &'a [u8],
    flags: u8,
    sign_count: u32,
    /// `(credential_id, COSE public key bytes)` when the attested-credential bit
    /// is set (registration only).
    credential: Option<(Vec<u8>, Vec<u8>)>,
}

fn parse_auth_data(data: &[u8]) -> Result<AuthData<'_>> {
    if data.len() < 37 {
        bail!("authenticator data too short");
    }
    let rp_id_hash = &data[0..32];
    let flags = data[32];
    let sign_count = u32::from_be_bytes(data[33..37].try_into().unwrap());
    let mut credential = None;
    if flags & FLAG_AT != 0 {
        let mut p = &data[37..];
        if p.len() < 18 {
            bail!("attested credential data truncated");
        }
        let cred_len = u16::from_be_bytes(p[16..18].try_into().unwrap()) as usize;
        p = &p[18..];
        if p.len() < cred_len {
            bail!("credential id truncated");
        }
        let credential_id = p[..cred_len].to_vec();
        p = &p[cred_len..];
        let (cose, rest) = read_cbor(p).context("COSE public key")?;
        if flags & FLAG_ED != 0 && !rest.is_empty() {
            // Extensions, if any, are not interpreted.
        }
        credential = Some((credential_id, cose_p256(&cose)?));
    }
    Ok(AuthData {
        rp_id_hash,
        flags,
        sign_count,
        credential,
    })
}

fn check_auth_data_flags(flags: u8) -> Result<()> {
    if flags & FLAG_UP == 0 {
        bail!("user presence flag not set");
    }
    if flags & FLAG_UV == 0 {
        bail!("user verification flag not set");
    }
    Ok(())
}

/// Verify a `navigator.credentials.create()` result and extract the credential.
///
/// `attestation_object` is the raw CBOR from the browser; `client_data_json`
/// is the raw UTF-8 JSON. The attestation statement itself is not verified
/// because the server requests `attestation: "none"`.
pub fn verify_registration(
    rp_id: &str,
    origin: &str,
    challenge: &[u8],
    client_data_json: &[u8],
    attestation_object: &[u8],
) -> Result<Registered> {
    check_client_data(client_data_json, "webauthn.create", origin, challenge)?;

    let (att, _) = read_cbor(attestation_object).context("attestationObject")?;
    let Cbor::Map(entries) = &att else {
        bail!("attestationObject is not a CBOR map");
    };
    let fmt = map_get(entries, "fmt").and_then(Cbor::as_text);
    if fmt != Some("none") {
        bail!("unsupported attestation format {fmt:?}");
    }
    let auth_data = map_get(entries, "authData")
        .and_then(Cbor::as_bytes)
        .context("attestationObject.authData")?;
    let ad = parse_auth_data(auth_data)?;
    check_auth_data_flags(ad.flags)?;
    if ad.rp_id_hash != Sha256::digest(rp_id.as_bytes()).as_slice() {
        bail!("rpIdHash does not match rp_id");
    }
    let (credential_id, public_key) = ad
        .credential
        .context("registration without credential data")?;
    verify_p256_point(&public_key)?;
    Ok(Registered {
        credential_id,
        public_key,
        sign_count: ad.sign_count,
    })
}

/// Verify a `navigator.credentials.get()` result against a stored public key.
///
/// Returns the authenticator's new signature counter, which the caller should
/// persist.
pub fn verify_assertion(
    rp_id: &str,
    origin: &str,
    challenge: &[u8],
    client_data_json: &[u8],
    authenticator_data: &[u8],
    signature: &[u8],
    public_key: &[u8],
) -> Result<u32> {
    check_client_data(client_data_json, "webauthn.get", origin, challenge)?;
    let ad = parse_auth_data(authenticator_data)?;
    check_auth_data_flags(ad.flags)?;
    if ad.rp_id_hash != Sha256::digest(rp_id.as_bytes()).as_slice() {
        bail!("rpIdHash does not match rp_id");
    }
    let mut signed = authenticator_data.to_vec();
    signed.extend_from_slice(&Sha256::digest(client_data_json));
    UnparsedPublicKey::new(&ECDSA_P256_SHA256_ASN1, public_key)
        .verify(&signed, signature)
        .map_err(|_| anyhow::anyhow!("assertion signature"))?;
    Ok(ad.sign_count)
}

/// Reject a public key that is not a valid uncompressed P-256 point early, so
/// a bad registration fails at enrolment rather than at first login.
fn verify_p256_point(point: &[u8]) -> Result<()> {
    if point.len() != 65 || point[0] != 0x04 {
        bail!("public key is not an uncompressed P-256 point");
    }
    Ok(())
}

/// Pull the uncompressed point out of a COSE `EC2`/`ES256`/`P-256` key.
fn cose_p256(cbor: &Cbor) -> Result<Vec<u8>> {
    let Cbor::Map(entries) = cbor else {
        bail!("COSE key is not a map");
    };
    let kty = map_get_int(entries, 1).and_then(Cbor::as_uint);
    if kty != Some(COSE_KTY_EC2) {
        bail!("COSE kty {kty:?} != EC2");
    }
    let alg = map_get_int(entries, 3).and_then(Cbor::as_i64);
    if alg != Some(COSE_ALG_ES256) {
        bail!("COSE alg {alg:?} != ES256");
    }
    let crv = map_get_int(entries, -1).and_then(Cbor::as_i64);
    if crv != Some(COSE_CRV_P256) {
        bail!("COSE crv {crv:?} != P-256");
    }
    let x = map_get_int(entries, -2).and_then(Cbor::as_bytes);
    let y = map_get_int(entries, -3).and_then(Cbor::as_bytes);
    let (x, y) = match (x, y) {
        (Some(x), Some(y)) if x.len() == 32 && y.len() == 32 => (x, y),
        _ => bail!("COSE x/y missing or not 32 bytes"),
    };
    let mut point = Vec::with_capacity(65);
    point.push(0x04);
    point.extend_from_slice(x);
    point.extend_from_slice(y);
    Ok(point)
}

// -- Minimal CBOR reader ----------------------------------------------------
//
// WebAuthn only ever hands us small CBOR documents (the attestation object and
// one COSE key), and we only need a subset of the data model. A full CBOR
// dependency would be more code surface than this.

#[derive(Debug, Clone, PartialEq)]
enum Cbor {
    Uint(u64),
    Nint(i64),
    Bytes(Vec<u8>),
    Text(String),
    Array(Vec<Cbor>),
    Map(Vec<(Cbor, Cbor)>),
    Bool(bool),
    Null,
    /// A tag or a float/simple value we do not interpret.
    Other,
}

impl Cbor {
    fn as_uint(&self) -> Option<u64> {
        match self {
            Cbor::Uint(v) => Some(*v),
            _ => None,
        }
    }

    fn as_i64(&self) -> Option<i64> {
        match self {
            Cbor::Uint(v) => i64::try_from(*v).ok(),
            Cbor::Nint(v) => Some(*v),
            _ => None,
        }
    }

    fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Cbor::Bytes(b) => Some(b),
            _ => None,
        }
    }

    fn as_text(&self) -> Option<&str> {
        match self {
            Cbor::Text(s) => Some(s),
            _ => None,
        }
    }
}

fn map_get<'a>(entries: &'a [(Cbor, Cbor)], key: &str) -> Option<&'a Cbor> {
    entries
        .iter()
        .find(|(k, _)| k.as_text() == Some(key))
        .map(|(_, v)| v)
}

fn map_get_int(entries: &[(Cbor, Cbor)], key: i64) -> Option<&Cbor> {
    entries
        .iter()
        .find(|(k, _)| k.as_i64() == Some(key))
        .map(|(_, v)| v)
}

fn read_len(info: u8, buf: &[u8]) -> Result<(u64, &[u8])> {
    Ok(match info {
        0..=23 => (info as u64, buf),
        24 => {
            let (&v, rest) = buf.split_first().context("cbor byte")?;
            (v as u64, rest)
        }
        25 => {
            let v = buf.get(..2).context("cbor u16")?;
            (u16::from_be_bytes(v.try_into().unwrap()) as u64, &buf[2..])
        }
        26 => {
            let v = buf.get(..4).context("cbor u32")?;
            (u32::from_be_bytes(v.try_into().unwrap()) as u64, &buf[4..])
        }
        27 => {
            let v = buf.get(..8).context("cbor u64")?;
            (u64::from_be_bytes(v.try_into().unwrap()), &buf[8..])
        }
        _ => bail!("reserved CBOR additional info {info}"),
    })
}

fn read_cbor(buf: &[u8]) -> Result<(Cbor, &[u8])> {
    let (&first, rest) = buf.split_first().context("cbor empty")?;
    let major = first >> 5;
    let info = first & 0x1f;
    let (val, mut rest) = read_len(info, rest)?;
    let item = match major {
        0 => Cbor::Uint(val),
        1 => Cbor::Nint(-1 - val as i64),
        2 => {
            let n = val as usize;
            let b = rest.get(..n).context("cbor bytes")?;
            rest = &rest[n..];
            Cbor::Bytes(b.to_vec())
        }
        3 => {
            let n = val as usize;
            let b = rest.get(..n).context("cbor text")?;
            rest = &rest[n..];
            Cbor::Text(std::str::from_utf8(b).context("cbor utf8")?.to_string())
        }
        4 => {
            let mut items = Vec::new();
            for _ in 0..val {
                let (item, r) = read_cbor(rest)?;
                items.push(item);
                rest = r;
            }
            Cbor::Array(items)
        }
        5 => {
            let mut items = Vec::new();
            for _ in 0..val {
                let (k, r) = read_cbor(rest)?;
                let (v, r) = read_cbor(r)?;
                items.push((k, v));
                rest = r;
            }
            Cbor::Map(items)
        }
        6 => {
            let (inner, r) = read_cbor(rest)?;
            rest = r;
            let _ = inner;
            Cbor::Other
        }
        _ => match info {
            20 => Cbor::Bool(false),
            21 => Cbor::Bool(true),
            22 | 23 => Cbor::Null,
            _ => Cbor::Other,
        },
    };
    Ok((item, rest))
}

#[cfg(test)]
pub(crate) mod sim {
    //! A software authenticator + the tiny CBOR writer it needs, shared by
    //! the unit tests here and the transport handshake test.
    use super::*;
    use ring::rand::SystemRandom;
    use ring::signature::{EcdsaKeyPair, ECDSA_P256_SHA256_ASN1_SIGNING, KeyPair};

    pub(crate) const RP: &str = "quosh.jtcs.dev";
    pub(crate) const ORIGIN: &str = "https://quosh.jtcs.dev";

    pub(crate) struct FakeAuthenticator {
        pub(crate) key: EcdsaKeyPair,
        pub(crate) credential_id: Vec<u8>,
        pub(crate) rng: SystemRandom,
    }

    impl FakeAuthenticator {
        pub(crate) fn new() -> Self {
            let rng = SystemRandom::new();
            let pkcs8 =
                EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng).unwrap();
            let key =
                EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8.as_ref(), &rng)
                    .unwrap();
            Self {
                key,
                credential_id: b"cred-1234".to_vec(),
                rng,
            }
        }

        /// The uncompressed point, as it comes out of the authenticator.
        pub(crate) fn point(&self) -> Vec<u8> {
            self.key.public_key().as_ref().to_vec()
        }

        fn cose(&self) -> Vec<u8> {
            let p = self.point();
            cbor_map(&[
                (cbor_uint(1), cbor_uint(COSE_KTY_EC2)),
                (cbor_uint(3), cbor_nint(COSE_ALG_ES256)),
                (cbor_nint(-1), cbor_uint(COSE_CRV_P256 as u64)),
                (cbor_nint(-2), cbor_bytes(&p[1..33])),
                (cbor_nint(-3), cbor_bytes(&p[33..65])),
            ])
        }

        pub(crate) fn registration(&self, challenge: &[u8], sign_count: u32) -> (Vec<u8>, Vec<u8>) {
            let client = client_json("webauthn.create", challenge);
            let auth_data = reg_auth_data(self.credential_id.clone(), &self.cose(), sign_count);
            let att = cbor_map(&[
                (cbor_text("fmt"), cbor_text("none")),
                (cbor_text("attStmt"), cbor_map(&[])),
                (cbor_text("authData"), cbor_bytes(&auth_data)),
            ]);
            (client, att)
        }

        pub(crate) fn assertion(&self, challenge: &[u8], sign_count: u32) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
            let client = client_json("webauthn.get", challenge);
            let mut auth_data = Vec::new();
            auth_data.extend_from_slice(&Sha256::digest(RP.as_bytes()));
            auth_data.push(FLAG_UP | FLAG_UV);
            auth_data.extend_from_slice(&sign_count.to_be_bytes());
            let mut signed = auth_data.clone();
            signed.extend_from_slice(&Sha256::digest(&client));
            let sig = self.key.sign(&self.rng, &signed).unwrap().as_ref().to_vec();
            (client, auth_data, sig)
        }
    }

    pub(crate) fn client_json(typ: &str, challenge: &[u8]) -> Vec<u8> {
        format!(
            r#"{{"type":"{typ}","challenge":"{}","origin":"{ORIGIN}"}}"#,
            URL_SAFE_NO_PAD.encode(challenge)
        )
        .into_bytes()
    }

    fn reg_auth_data(credential_id: Vec<u8>, cose: &[u8], sign_count: u32) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&Sha256::digest(RP.as_bytes()));
        out.push(FLAG_UP | FLAG_UV | FLAG_AT);
        out.extend_from_slice(&sign_count.to_be_bytes());
        out.extend_from_slice(&[0u8; 16]); // AAGUID
        out.extend_from_slice(&(credential_id.len() as u16).to_be_bytes());
        out.extend_from_slice(&credential_id);
        out.extend_from_slice(cose);
        out
    }

    fn cbor_head(major: u8, val: u64) -> Vec<u8> {
        let m = major << 5;
        if val < 24 {
            vec![m | val as u8]
        } else if val < 256 {
            vec![m | 24, val as u8]
        } else if val < 65536 {
            let mut v = vec![m | 25];
            v.extend_from_slice(&(val as u16).to_be_bytes());
            v
        } else {
            let mut v = vec![m | 26];
            v.extend_from_slice(&(val as u32).to_be_bytes());
            v
        }
    }

    fn cbor_uint(v: u64) -> Vec<u8> {
        cbor_head(0, v)
    }

    fn cbor_nint(v: i64) -> Vec<u8> {
        assert!(v < 0);
        cbor_head(1, (-1 - v) as u64)
    }

    fn cbor_bytes(b: &[u8]) -> Vec<u8> {
        let mut v = cbor_head(2, b.len() as u64);
        v.extend_from_slice(b);
        v
    }

    fn cbor_text(s: &str) -> Vec<u8> {
        let mut v = cbor_head(3, s.len() as u64);
        v.extend_from_slice(s.as_bytes());
        v
    }

    fn cbor_map(pairs: &[(Vec<u8>, Vec<u8>)]) -> Vec<u8> {
        let mut v = cbor_head(5, pairs.len() as u64);
        for (k, val) in pairs {
            v.extend_from_slice(k);
            v.extend_from_slice(val);
        }
        v
    }

}

#[cfg(test)]
mod tests {
    use super::sim::*;
    use super::*;

    #[test]
    fn registration_round_trip() {
        let a = FakeAuthenticator::new();
        let challenge = [7u8; 32];
        let (client, att) = a.registration(&challenge, 0);
        let reg = verify_registration(RP, ORIGIN, &challenge, &client, &att).unwrap();
        assert_eq!(reg.credential_id, a.credential_id);
        assert_eq!(reg.public_key, a.point());
    }

    #[test]
    fn assertion_round_trip_and_counter() {
        let a = FakeAuthenticator::new();
        let challenge = [9u8; 32];
        let (client, auth_data, sig) = a.assertion(&challenge, 5);
        let count = verify_assertion(
            RP,
            ORIGIN,
            &challenge,
            &client,
            &auth_data,
            &sig,
            &a.point(),
        )
        .unwrap();
        assert_eq!(count, 5);
    }

    #[test]
    fn wrong_challenge_is_rejected() {
        let a = FakeAuthenticator::new();
        let (client, att) = a.registration(&[1u8; 32], 0);
        assert!(verify_registration(RP, ORIGIN, &[2u8; 32], &client, &att).is_err());
    }

    #[test]
    fn wrong_origin_is_rejected() {
        let a = FakeAuthenticator::new();
        let challenge = [3u8; 32];
        let (client, att) = a.registration(&challenge, 0);
        assert!(
            verify_registration(RP, "https://evil.example", &challenge, &client, &att).is_err()
        );
    }

    #[test]
    fn wrong_rp_id_is_rejected() {
        let a = FakeAuthenticator::new();
        let challenge = [4u8; 32];
        let (client, att) = a.registration(&challenge, 0);
        assert!(verify_registration("other.example", ORIGIN, &challenge, &client, &att).is_err());
    }

    #[test]
    fn tampered_signature_is_rejected() {
        let a = FakeAuthenticator::new();
        let challenge = [5u8; 32];
        let (client, auth_data, mut sig) = a.assertion(&challenge, 1);
        let last = sig.len() - 1;
        sig[last] ^= 0xff;
        assert!(
            verify_assertion(
                RP,
                ORIGIN,
                &challenge,
                &client,
                &auth_data,
                &sig,
                &a.point()
            )
            .is_err()
        );
    }

    #[test]
    fn tampered_authenticator_data_is_rejected() {
        let a = FakeAuthenticator::new();
        let challenge = [6u8; 32];
        let (client, mut auth_data, sig) = a.assertion(&challenge, 1);
        auth_data[32] &= !FLAG_UV;
        assert!(
            verify_assertion(
                RP,
                ORIGIN,
                &challenge,
                &client,
                &auth_data,
                &sig,
                &a.point()
            )
            .is_err()
        );
    }

    #[test]
    fn assertion_type_mismatch_is_rejected() {
        let a = FakeAuthenticator::new();
        let challenge = [8u8; 32];
        // Sign a registration-shaped clientData; it must not satisfy a get().
        let client = client_json("webauthn.create", &challenge);
        let mut auth_data = Vec::new();
        auth_data.extend_from_slice(&Sha256::digest(RP.as_bytes()));
        auth_data.push(FLAG_UP | FLAG_UV);
        auth_data.extend_from_slice(&0u32.to_be_bytes());
        let mut signed = auth_data.clone();
        signed.extend_from_slice(&Sha256::digest(&client));
        let sig = a.key.sign(&a.rng, &signed).unwrap().as_ref().to_vec();
        assert!(
            verify_assertion(
                RP,
                ORIGIN,
                &challenge,
                &client,
                &auth_data,
                &sig,
                &a.point()
            )
            .is_err()
        );
    }
}
