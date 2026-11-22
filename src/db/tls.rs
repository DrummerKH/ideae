//! What an encrypted connection trusts, for the drivers that take a rustls
//! configuration. The mode is the connection's own: encrypted only, or with
//! the server's certificate checked.

use std::{path::Path, sync::Arc};

use anyhow::{Context as _, Result, anyhow};
use rustls::{
    ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme,
    client::{
        WebPkiServerVerifier,
        danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    },
    crypto::{CryptoProvider, ring, verify_tls12_signature, verify_tls13_signature},
    pki_types::{CertificateDer, ServerName, UnixTime, pem::PemObject as _},
};

fn provider() -> Arc<CryptoProvider> {
    Arc::new(ring::default_provider())
}

/// Every crate that picks the process's provider finds this one.
pub fn install_provider() {
    let _ = ring::default_provider().install_default();
}

/// Encrypted, whoever answers: what `Require` asks for.
#[derive(Debug)]
struct AnyCertificate(Arc<CryptoProvider>);

impl ServerCertVerifier for AnyCertificate {
    fn verify_server_cert(&self, _: &CertificateDer<'_>, _: &[CertificateDer<'_>], _: &ServerName<'_>, _: &[u8], _: UnixTime) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn verify_tls13_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// Checks the certificate against the database's own name: through a tunnel
/// the driver only knows it is talking to this machine.
#[derive(Debug)]
struct NamedServer {
    inner: Arc<WebPkiServerVerifier>,
    name: ServerName<'static>,
}

impl ServerCertVerifier for NamedServer {
    fn verify_server_cert(&self, end: &CertificateDer<'_>, intermediates: &[CertificateDer<'_>], _: &ServerName<'_>, ocsp: &[u8], now: UnixTime) -> Result<ServerCertVerified, rustls::Error> {
        self.inner.verify_server_cert(end, intermediates, &self.name, ocsp, now)
    }

    fn verify_tls12_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

fn roots(ca: Option<&Path>) -> Result<RootCertStore> {
    let mut store = RootCertStore::empty();
    match ca {
        Some(file) => {
            let certificates: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(file)
                .with_context(|| format!("The CA file {}", file.display()))?
                .collect::<Result<_, _>>()
                .with_context(|| format!("The CA file {}", file.display()))?;
            anyhow::ensure!(!certificates.is_empty(), "No certificate in {}", file.display());
            for certificate in certificates {
                store.add(certificate)?;
            }
        }
        None => store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
    }
    Ok(store)
}

/// A configuration that encrypts, and with `verify` checks the certificate
/// against `ca` (else the public roots) for the name `host`.
pub fn client_config(verify: bool, ca: Option<&Path>, host: &str) -> Result<ClientConfig> {
    let provider = provider();
    let builder = ClientConfig::builder_with_provider(provider.clone()).with_safe_default_protocol_versions()?;
    let verifier: Arc<dyn ServerCertVerifier> = if verify {
        let inner = WebPkiServerVerifier::builder_with_provider(Arc::new(roots(ca)?), provider).build()?;
        let name = ServerName::try_from(host.to_string()).map_err(|_| anyhow!("{host} is not a name a certificate can be for"))?;
        Arc::new(NamedServer { inner, name })
    } else {
        Arc::new(AnyCertificate(provider))
    };
    Ok(builder.dangerous().with_custom_certificate_verifier(verifier).with_no_client_auth())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_configuration_is_made_for_each_mode() {
        assert!(client_config(false, None, "db.example").is_ok());
        assert!(client_config(true, None, "db.example").is_ok());
        assert!(client_config(true, None, "10.0.0.1").is_ok());
    }

    #[test]
    fn a_ca_file_that_is_not_one_is_said_to_be() {
        let file = std::env::temp_dir().join(format!("ideae-ca-{}.pem", crate::db::config::new_id()));
        std::fs::write(&file, "not a certificate").unwrap();
        let error = client_config(true, Some(&file), "db.example").unwrap_err().to_string();
        assert!(error.contains("No certificate"), "{error}");
        std::fs::remove_file(file).unwrap();
    }
}
