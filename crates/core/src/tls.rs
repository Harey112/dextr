//! QUIC/TLS configuration with self-signed device certificates.
//!
//! Both sides always present their certificate and prove they hold its key.
//! Which certificates are trusted is decided by fingerprint:
//!
//! * dialing a paired device pins its fingerprint in the handshake;
//! * dialing to pair accepts any certificate (the users then compare a code
//!   derived from both fingerprints);
//! * the listener accepts any client certificate, and the engine checks the
//!   fingerprint before doing anything but pairing.

use std::{sync::Arc, time::Duration};

use dextr_proto::ALPN;
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use rustls::{
    DigitallySignedStruct, DistinguishedName, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature},
    pki_types::{CertificateDer, ServerName, UnixTime},
    server::danger::{ClientCertVerified, ClientCertVerifier},
};

use crate::identity::{self, Fingerprint, Identity};

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn transport() -> Arc<quinn::TransportConfig> {
    let mut t = quinn::TransportConfig::default();
    t.keep_alive_interval(Some(Duration::from_secs(1)));
    t.max_idle_timeout(Some(Duration::from_secs(10).try_into().unwrap()));
    Arc::new(t)
}

#[derive(Debug)]
struct Pinned {
    /// `None` accepts any certificate (signatures are still verified).
    expected: Option<Fingerprint>,
    provider: Arc<CryptoProvider>,
}

impl Pinned {
    fn check(&self, cert: &CertificateDer<'_>) -> Result<(), rustls::Error> {
        let fp = identity::fingerprint(cert);
        match self.expected {
            None => Ok(()),
            Some(expected) if expected == fp => Ok(()),
            _ => {
                tracing::warn!("rejected unpaired peer {}", identity::format(&fp));
                Err(rustls::Error::InvalidCertificate(
                    rustls::CertificateError::ApplicationVerificationFailure,
                ))
            }
        }
    }

    fn tls12(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(m, c, d, &self.provider.signature_verification_algorithms)
    }

    fn tls13(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(m, c, d, &self.provider.signature_verification_algorithms)
    }

    fn schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        self.check(end_entity).map(|()| ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.tls12(m, c, d)
    }

    fn verify_tls13_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.tls13(m, c, d)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.schemes()
    }
}

impl ClientCertVerifier for Pinned {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        self.check(end_entity).map(|()| ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.tls12(m, c, d)
    }

    fn verify_tls13_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.tls13(m, c, d)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.schemes()
    }
}

pub fn server_config(id: &Identity) -> anyhow::Result<quinn::ServerConfig> {
    let verifier = Arc::new(Pinned { expected: None, provider: provider() });
    let mut tls = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(verifier)
        .with_single_cert(vec![id.cert.clone()], id.key())?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(tls)?));
    config.transport_config(transport());
    Ok(config)
}

/// Dials `peer` only, or anyone when pairing (`None`).
pub fn client_config(id: &Identity, peer: Option<Fingerprint>) -> anyhow::Result<quinn::ClientConfig> {
    let verifier = Arc::new(Pinned { expected: peer, provider: provider() });
    let mut tls = rustls::ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(vec![id.cert.clone()], id.key())?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let mut config = quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(tls)?));
    config.transport_config(transport());
    Ok(config)
}

/// The fingerprint of the certificate the other side presented.
pub fn peer_fingerprint(conn: &quinn::Connection) -> Option<Fingerprint> {
    let certs = conn.peer_identity()?.downcast::<Vec<CertificateDer<'static>>>().ok()?;
    certs.first().map(|c| identity::fingerprint(c))
}

/// The 6-digit code both users compare when pairing. Derived from both
/// certificates, so a device in the middle (which can't present either
/// device's certificate) makes the two screens show different codes.
pub fn pairing_code(a: &Fingerprint, b: &Fingerprint) -> String {
    use sha2::{Digest, Sha256};
    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
    let hash = Sha256::new().chain_update(b"dextr pairing code").chain_update(lo).chain_update(hi).finalize();
    let n = u32::from_be_bytes([hash[0], hash[1], hash[2], hash[3]]) % 1_000_000;
    format!("{:03} {:03}", n / 1000, n % 1000)
}

#[cfg(test)]
mod tests {
    #[test]
    fn pairing_code_is_the_same_on_both_sides() {
        let (a, b, c) = ([1u8; 32], [2u8; 32], [3u8; 32]);
        let code = super::pairing_code(&a, &b);
        assert_eq!(code, super::pairing_code(&b, &a));
        assert_eq!(code.len(), 7);
        assert_ne!(code, super::pairing_code(&a, &c));
    }
}
