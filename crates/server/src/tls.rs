use std::sync::Arc;

use anyhow::{Context, Result};
use rcgen::{CertificateParams, KeyPair, SanType};
use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

/// Result of TLS configuration build, including the cert DER for agent pinning.
pub struct TlsConfigResult {
    pub config: ServerConfig,
    /// Path to the PEM certificate file the agent should pin against.
    /// For user-provided certs this is the original cert path.
    /// For self-signed certs this is a generated temp file.
    pub cert_pem_path: String,
}

/// Build a `rustls::ServerConfig` from either configured cert/key paths
/// or by generating a self-signed certificate.
pub fn build_tls_config(
    cert_path: Option<&str>,
    key_path: Option<&str>,
) -> Result<TlsConfigResult> {
    let (certs, key, cert_pem_path) = match (cert_path, key_path) {
        (Some(cert), Some(key)) => {
            let (certs, priv_key) = load_certs_from_files(cert, key)?;
            (certs, priv_key, cert.to_string())
        }
        _ => {
            let cert_pem_path = "/var/lib/beam/server-cert.pem";
            let key_pem_path = "/var/lib/beam/server-key.pem";

            std::fs::create_dir_all("/var/lib/beam").context("Failed to create /var/lib/beam")?;

            // Reuse existing self-signed cert+key if both files exist and are valid
            let loaded = if std::path::Path::new(cert_pem_path).exists()
                && std::path::Path::new(key_pem_path).exists()
            {
                match load_certs_from_files(cert_pem_path, key_pem_path) {
                    Ok((certs, key)) => {
                        tracing::info!("Loaded existing self-signed cert from {cert_pem_path}");
                        Some((certs, key))
                    }
                    Err(e) => {
                        tracing::warn!("Existing self-signed cert invalid, regenerating: {e}");
                        None
                    }
                }
            } else {
                None
            };

            let (certs, priv_key) = match loaded {
                Some(pair) => pair,
                None => {
                    let (certs, priv_key) = generate_self_signed()?;

                    // Persist cert PEM for agent pinning
                    let pem_data = pem::encode(&pem::Pem::new("CERTIFICATE", certs[0].to_vec()));
                    std::fs::write(cert_pem_path, pem_data.as_bytes())
                        .context("Failed to write self-signed cert PEM")?;

                    // Persist key PEM so cert survives restarts
                    {
                        use std::os::unix::fs::OpenOptionsExt;
                        let key_bytes = match &priv_key {
                            PrivateKeyDer::Pkcs8(k) => k.secret_pkcs8_der(),
                            _ => unreachable!("we always generate PKCS8"),
                        };
                        let key_pem_data =
                            pem::encode(&pem::Pem::new("PRIVATE KEY", key_bytes.to_vec()));
                        std::fs::OpenOptions::new()
                            .write(true)
                            .create(true)
                            .truncate(true)
                            .mode(0o600)
                            .open(key_pem_path)
                            .and_then(|mut f| {
                                use std::io::Write;
                                f.write_all(key_pem_data.as_bytes())
                            })
                            .context("Failed to write self-signed key PEM")?;
                    }

                    tracing::info!("Generated self-signed cert: {cert_pem_path} + {key_pem_path}");
                    (certs, priv_key)
                }
            };

            (certs, priv_key, cert_pem_path.to_string())
        }
    };

    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("Failed to build TLS server config")?;

    Ok(TlsConfigResult {
        config,
        cert_pem_path,
    })
}

/// Load certificate chain and private key from PEM files on disk.
fn load_certs_from_files(
    cert_path: &str,
    key_path: &str,
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    let cert_pem = std::fs::read(cert_path)
        .with_context(|| format!("Failed to read TLS cert: {cert_path}"))?;
    let key_pem =
        std::fs::read(key_path).with_context(|| format!("Failed to read TLS key: {key_path}"))?;

    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut cert_pem.as_slice())
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("Failed to parse TLS certificate PEM")?;

    let key = rustls_pemfile::private_key(&mut key_pem.as_slice())
        .context("Failed to parse TLS private key PEM")?
        .context("No private key found in PEM file")?;

    tracing::info!("Loaded TLS cert from {cert_path}");
    Ok((certs, key))
}

/// Generate a self-signed certificate for localhost development.
fn generate_self_signed() -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    tracing::info!("Generating self-signed TLS certificate for localhost");

    let mut params = CertificateParams::new(vec!["localhost".to_string()])
        .context("Failed to create certificate params")?;
    params
        .subject_alt_names
        .push(SanType::IpAddress(std::net::IpAddr::V4(
            std::net::Ipv4Addr::LOCALHOST,
        )));
    params
        .subject_alt_names
        .push(SanType::IpAddress(std::net::IpAddr::V6(
            std::net::Ipv6Addr::LOCALHOST,
        )));

    let key_pair = KeyPair::generate().context("Failed to generate key pair")?;
    let cert = params
        .self_signed(&key_pair)
        .context("Failed to generate self-signed certificate")?;

    let cert_der = CertificateDer::from(cert.der().to_vec());
    let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_pair.serialize_der()));

    Ok((vec![cert_der], key_der))
}

/// Helper to create a `tokio_rustls::TlsAcceptor` from a `rustls::ServerConfig`.
pub fn make_acceptor(config: ServerConfig) -> tokio_rustls::TlsAcceptor {
    tokio_rustls::TlsAcceptor::from(Arc::new(config))
}
