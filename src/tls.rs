// lode/tls: the shared rustls client-config builder.
//
// One place turns a TlsSpec into a rustls ClientConfig for every network driver
// (no OpenSSL anywhere). Encryption is the DEFAULT posture: `Prefer` attempts
// TLS and only falls back to plaintext when the server has none (and that is
// surfaced, never silent); `Require`/`VerifyCa`/`VerifyFull` mandate it. `ca`
// verifies the server chain; `client_cert`/`client_key` enable mutual X.509.
//
// The daemon installs a process-wide crypto provider once at startup (see
// install_crypto_provider) so rustls has algorithms without each call wiring one.

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::WebPkiServerVerifier;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{CertificateError, ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};

use crate::spec::TlsSpec;

/// Install the process-wide default crypto provider (ring) once. Idempotent.
pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Load PEM certificates from a file.
fn load_certs(path: &str) -> anyhow::Result<Vec<CertificateDer<'static>>> {
    let data = std::fs::read(path).map_err(|e| anyhow::anyhow!("cannot read cert '{path}': {e}"))?;
    let mut reader = std::io::BufReader::new(&data[..]);
    let certs: Result<Vec<_>, _> = rustls_pemfile::certs(&mut reader).collect();
    certs.map_err(|e| anyhow::anyhow!("bad certificate '{path}': {e}"))
}

/// Load a PEM private key from a file.
fn load_key(path: &str) -> anyhow::Result<PrivateKeyDer<'static>> {
    let data = std::fs::read(path).map_err(|e| anyhow::anyhow!("cannot read key '{path}': {e}"))?;
    let mut reader = std::io::BufReader::new(&data[..]);
    rustls_pemfile::private_key(&mut reader)
        .map_err(|e| anyhow::anyhow!("bad private key '{path}': {e}"))?
        .ok_or_else(|| anyhow::anyhow!("no private key found in '{path}'"))
}

/// A dangerous verifier that accepts ANY server certificate — used for the
/// encrypt-but-do-not-verify modes (`Prefer` / `Require`), where the link is
/// still fully encrypted but the peer identity is not checked (as with libpq's
/// sslmode=require). The verifying modes (`VerifyCa`/`VerifyFull`) never use it.
#[derive(Debug)]
struct AcceptAnyServerCert(Arc<rustls::crypto::CryptoProvider>);

impl ServerCertVerifier for AcceptAnyServerCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// A verifier that checks the CHAIN and not the NAME — which is what
/// `verify_ca` says, and what `verify_full` adds the name to.
///
/// Without it the two modes were the same mode: rustls's own verifier checks
/// the chain and then the name, so `verify_ca` was as strict as `verify_full`
/// everywhere except mysql_async, whose API happens to have a flag for it. A
/// choice the form offers and the engine does not keep is not a choice.
///
/// The name is also the one thing that cannot be checked through an SSH tunnel
/// or a proxy: the address dialled is then a loopback port of this machine's
/// own, and no server certificate names it. The chain still is checked, and the
/// channel to that port is the SSH channel, authenticated by the server's host
/// key — so for a tunnelled link `verify_ca` is the honest strict mode rather
/// than a loosening of one.
#[derive(Debug)]
struct VerifyChainOnly(Arc<dyn ServerCertVerifier>);

impl ServerCertVerifier for VerifyChainOnly {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        match self
            .0
            .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
        {
            // rustls verifies the chain BEFORE the name, so this error — and
            // only this error — means the chain was good and the name was not.
            // Every other error is passed through: an unknown issuer, an
            // expired certificate and a revoked one are all still refusals.
            Err(rustls::Error::InvalidCertificate(CertificateError::NotValidForName))
            | Err(rustls::Error::InvalidCertificate(CertificateError::NotValidForNameContext { .. })) => {
                Ok(ServerCertVerified::assertion())
            }
            other => other,
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_verify_schemes()
    }
}

/// Build a rustls ClientConfig for `tls`. Applies the CA / system roots and any
/// client certificate (mutual X.509); uses the accept-any verifier for the
/// encrypt-only modes. Errors if a client cert is given without a key (or vice
/// versa), or a referenced file is unreadable.
pub fn client_config(tls: &TlsSpec) -> anyhow::Result<Arc<ClientConfig>> {
    let provider = rustls::crypto::CryptoProvider::get_default()
        .cloned()
        .unwrap_or_else(|| Arc::new(rustls::crypto::ring::default_provider()));

    let builder = ClientConfig::builder();

    // Root store (used only when the mode verifies the chain).
    let config = if tls.verifies_cert() {
        let mut roots = RootCertStore::empty();
        if let Some(ca) = &tls.ca {
            for cert in load_certs(ca)? {
                roots
                    .add(cert)
                    .map_err(|e| anyhow::anyhow!("bad CA certificate: {e}"))?;
            }
        } else {
            let native = rustls_native_certs::load_native_certs();
            for cert in native.certs {
                let _ = roots.add(cert);
            }
        }
        if tls.verifies_hostname() {
            finish(builder.with_root_certificates(roots), tls)?
        } else {
            // verify_ca: the chain, not the name. Built on rustls's own
            // verifier, so the chain is checked by the same code as
            // verify_full — only the name is let go.
            let chain = WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider.clone())
                .build()
                .map_err(|e| anyhow::anyhow!("these CA certificates cannot verify anything: {e}"))?;
            let verifier = Arc::new(VerifyChainOnly(chain));
            finish(builder.dangerous().with_custom_certificate_verifier(verifier), tls)?
        }
    } else {
        // Encrypt-only (Prefer / Require): accept any server cert.
        let verifier = Arc::new(AcceptAnyServerCert(provider));
        let b = builder.dangerous().with_custom_certificate_verifier(verifier);
        finish(b, tls)?
    };

    Ok(Arc::new(config))
}

/// Apply the optional client certificate (mutual X.509), else no client auth.
fn finish(
    builder: rustls::ConfigBuilder<ClientConfig, rustls::client::WantsClientCert>,
    tls: &TlsSpec,
) -> anyhow::Result<ClientConfig> {
    match (&tls.client_cert, &tls.client_key) {
        (Some(cert), Some(key)) => {
            let certs = load_certs(cert)?;
            let key = load_key(key)?;
            builder
                .with_client_auth_cert(certs, key)
                .map_err(|e| anyhow::anyhow!("client certificate rejected: {e}"))
        }
        (None, None) => Ok(builder.with_no_client_auth()),
        _ => Err(anyhow::anyhow!("TLS client cert and key must be provided together")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::TlsMode;

    /// A verifier that answers whatever the test says, so the one thing under
    /// test is what `VerifyChainOnly` DOES with each answer. No certificate is
    /// needed for that, and a certificate chain that a test can make is a
    /// chain nobody should trust anyway.
    #[derive(Debug)]
    struct Says(Option<rustls::Error>);

    impl ServerCertVerifier for Says {
        fn verify_server_cert(
            &self,
            _: &CertificateDer<'_>,
            _: &[CertificateDer<'_>],
            _: &ServerName<'_>,
            _: &[u8],
            _: UnixTime,
        ) -> Result<ServerCertVerified, rustls::Error> {
            match &self.0 {
                Some(e) => Err(e.clone()),
                None => Ok(ServerCertVerified::assertion()),
            }
        }

        fn verify_tls12_signature(
            &self,
            _: &[u8],
            _: &CertificateDer<'_>,
            _: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _: &[u8],
            _: &CertificateDer<'_>,
            _: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            Vec::new()
        }
    }

    fn ask(said: Option<rustls::Error>) -> Result<ServerCertVerified, rustls::Error> {
        let verifier = VerifyChainOnly(Arc::new(Says(said)));
        verifier.verify_server_cert(
            &CertificateDer::from(vec![0u8; 4]),
            &[],
            &ServerName::try_from("db.example.com").expect("a name"),
            &[],
            UnixTime::now(),
        )
    }

    #[test]
    fn the_wrong_name_on_a_good_chain_is_accepted() {
        // Through a tunnel the address dialled is 127.0.0.1 and no certificate
        // names it. The chain was verified; this is what verify_ca means.
        assert!(ask(Some(rustls::Error::InvalidCertificate(CertificateError::NotValidForName))).is_ok());
        assert!(ask(Some(rustls::Error::InvalidCertificate(
            CertificateError::NotValidForNameContext {
                expected: ServerName::try_from("db.example.com").expect("a name").to_owned(),
                presented: vec!["CN=other".to_string()],
            }
        )))
        .is_ok());
    }

    #[test]
    fn a_chain_nobody_vouched_for_is_still_refused() {
        // The whole point of verify_ca: the CA is still the gate. This is the
        // error the Redis driver was failing with before it was handed the CA.
        assert!(ask(Some(rustls::Error::InvalidCertificate(CertificateError::UnknownIssuer))).is_err());
        assert!(ask(Some(rustls::Error::InvalidCertificate(CertificateError::Expired))).is_err());
        assert!(ask(Some(rustls::Error::InvalidCertificate(CertificateError::Revoked))).is_err());
    }

    #[test]
    fn a_certificate_that_passes_passes() {
        assert!(ask(None).is_ok());
    }

    #[test]
    fn each_mode_builds_a_config_of_its_own() {
        install_crypto_provider();
        // No CA given: the system roots. Every mode has to produce something —
        // a mode that cannot be built is a mode nobody can choose.
        for mode in [
            TlsMode::Prefer,
            TlsMode::Require,
            TlsMode::VerifyCa,
            TlsMode::VerifyFull,
        ] {
            let built = client_config(&TlsSpec {
                mode,
                ..Default::default()
            });
            assert!(built.is_ok(), "{} could not be built: {built:?}", mode.label());
        }
    }
}
