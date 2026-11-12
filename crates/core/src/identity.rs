//! This device's long-lived TLS identity and certificate fingerprints.
//!
//! There is no certificate authority: each device has a self-signed
//! certificate, and peers trust each other by SHA-256 fingerprint, which the
//! user compares or copies across once when pairing.

use std::path::Path;

use anyhow::Context as _;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use sha2::{Digest, Sha256};

pub type Fingerprint = [u8; 32];

pub struct Identity {
    pub cert: CertificateDer<'static>,
    key: Vec<u8>,
    pub fingerprint: Fingerprint,
}

impl Identity {
    pub fn key(&self) -> PrivateKeyDer<'static> {
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key.clone()))
    }

    /// Loads the identity from `dir`, creating one on first run.
    pub fn load_or_create(dir: &Path) -> anyhow::Result<Self> {
        let (cert_path, key_path) = (dir.join("cert.der"), dir.join("key.der"));
        let (cert, key) = if cert_path.exists() && key_path.exists() {
            (std::fs::read(&cert_path)?, std::fs::read(&key_path)?)
        } else {
            let generated = rcgen::generate_simple_self_signed(vec!["dextr".to_string()])
                .context("generating the device certificate")?;
            let cert = generated.cert.der().to_vec();
            let key = generated.signing_key.serialize_der();
            write_private(&key_path, &key)?;
            std::fs::write(&cert_path, &cert)?;
            (cert, key)
        };
        let fingerprint = fingerprint(&cert);
        Ok(Self { cert: CertificateDer::from(cert), key, fingerprint })
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(bytes)?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        Ok(std::fs::write(path, bytes)?)
    }
}

pub fn fingerprint(cert_der: &[u8]) -> Fingerprint {
    Sha256::digest(cert_der).into()
}

/// `AB12 CD34 …`: 16 groups of 4 hex digits.
pub fn format(fp: &Fingerprint) -> String {
    let hex = hex::encode_upper(fp);
    hex.as_bytes()
        .chunks(4)
        .map(|c| std::str::from_utf8(c).unwrap())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Accepts any spacing, dashes, colons or case.
pub fn parse(text: &str) -> Option<Fingerprint> {
    let hex: String = text.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    hex::decode(hex).ok()?.try_into().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_parse_round_trip() {
        let fp = fingerprint(b"hello");
        let text = format(&fp);
        assert_eq!(text.len(), 64 + 15);
        assert_eq!(parse(&text), Some(fp));
        assert_eq!(parse(&text.to_lowercase().replace(' ', ":")), Some(fp));
        assert_eq!(parse("ABCD"), None);
    }
}
