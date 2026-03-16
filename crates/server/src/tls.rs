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
            // Override UMask=0077: agents need to traverse this directory
            // to read the public cert after dropping root privileges.
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions("/var/lib/beam", std::fs::Permissions::from_mode(0o755))
                    .context("Failed to set /var/lib/beam permissions")?;
            }

            // Reuse existing self-signed cert+key if both files exist, are valid,
            // and haven't expired (check file age as a proxy for cert expiry).
            let loaded = if std::path::Path::new(cert_pem_path).exists()
                && std::path::Path::new(key_pem_path).exists()
            {
                // Check cert file age — regenerate if older than 365 days
                let cert_too_old = std::fs::metadata(cert_pem_path)
                    .and_then(|m| m.modified())
                    .map(|mtime| {
                        mtime.elapsed().unwrap_or_default()
                            > std::time::Duration::from_secs(365 * 24 * 3600)
                    })
                    .unwrap_or(false);

                if cert_too_old {
                    tracing::warn!(
                        "Self-signed cert is older than 365 days, regenerating: {cert_pem_path}"
                    );
                    None
                } else {
                    match load_certs_from_files(cert_pem_path, key_pem_path) {
                        Ok((certs, key)) => {
                            // Log cert file age for monitoring
                            if let Ok(age) = std::fs::metadata(cert_pem_path)
                                .and_then(|m| m.modified())
                                .map(|mtime| mtime.elapsed().unwrap_or_default())
                            {
                                let days = age.as_secs() / 86400;
                                if days > 300 {
                                    tracing::warn!(
                                        "Self-signed cert is {days} days old, will regenerate at 365 days"
                                    );
                                } else {
                                    tracing::info!(
                                        "Loaded self-signed cert from {cert_pem_path} (age: {days} days)"
                                    );
                                }
                            }
                            Some((certs, key))
                        }
                        Err(e) => {
                            tracing::warn!("Existing self-signed cert invalid, regenerating: {e}");
                            None
                        }
                    }
                }
            } else {
                None
            };

            let (certs, priv_key) = match loaded {
                Some(pair) => pair,
                None => {
                    let (certs, priv_key) = generate_self_signed()?;

                    // Persist cert PEM for agent pinning (fsync for crash safety).
                    // Mode 0644: the cert is public; agents need to read it after
                    // dropping privileges. UMask=0077 in systemd would make it 0600
                    // if we used File::create() without explicit mode.
                    let pem_data = pem::encode(&pem::Pem::new("CERTIFICATE", certs[0].to_vec()));
                    {
                        use std::os::unix::fs::OpenOptionsExt;
                        use std::os::unix::fs::PermissionsExt;
                        let cert_file = std::fs::OpenOptions::new()
                            .write(true)
                            .create(true)
                            .truncate(true)
                            .mode(0o644)
                            .open(cert_pem_path)
                            .context("Failed to create self-signed cert PEM")?;
                        use std::io::Write;
                        let mut writer = std::io::BufWriter::new(&cert_file);
                        writer
                            .write_all(pem_data.as_bytes())
                            .context("Failed to write self-signed cert PEM")?;
                        writer.flush()?;
                        cert_file.sync_all().context("Failed to fsync cert PEM")?;
                        // Override UMask=0077 from systemd: the cert is public and
                        // agents need to read it after dropping root privileges.
                        // OpenOptions::mode() is masked by umask, so set explicitly.
                        cert_file
                            .set_permissions(std::fs::Permissions::from_mode(0o644))
                            .context("Failed to set cert permissions")?;
                    }

                    // Persist key PEM so cert survives restarts
                    {
                        use std::os::unix::fs::OpenOptionsExt;
                        let key_bytes = match &priv_key {
                            PrivateKeyDer::Pkcs8(k) => k.secret_pkcs8_der(),
                            _ => unreachable!("we always generate PKCS8"),
                        };
                        let key_pem_data =
                            pem::encode(&pem::Pem::new("PRIVATE KEY", key_bytes.to_vec()));
                        let key_file = std::fs::OpenOptions::new()
                            .write(true)
                            .create(true)
                            .truncate(true)
                            .mode(0o600)
                            .open(key_pem_path)
                            .context("Failed to create self-signed key PEM")?;
                        {
                            use std::io::Write;
                            let mut writer = std::io::BufWriter::new(&key_file);
                            writer
                                .write_all(key_pem_data.as_bytes())
                                .context("Failed to write self-signed key PEM")?;
                            writer.flush()?;
                        }
                        key_file.sync_all().context("Failed to fsync key PEM")?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn init_crypto() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }

    #[test]
    fn generate_self_signed_produces_valid_cert() {
        init_crypto();
        let (certs, key) = generate_self_signed().unwrap();
        assert_eq!(certs.len(), 1, "Should produce exactly one certificate");
        assert!(!certs[0].is_empty(), "Certificate DER should not be empty");
        match &key {
            PrivateKeyDer::Pkcs8(k) => {
                assert!(!k.secret_pkcs8_der().is_empty(), "Key should not be empty");
            }
            _ => panic!("Expected PKCS8 key"),
        }
    }

    #[test]
    fn self_signed_cert_builds_valid_tls_config() {
        init_crypto();
        let (certs, key) = generate_self_signed().unwrap();
        let config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key);
        assert!(
            config.is_ok(),
            "Self-signed cert should produce valid TLS config"
        );
    }

    #[test]
    fn load_certs_roundtrip_via_temp_files() {
        init_crypto();
        let (certs, key) = generate_self_signed().unwrap();

        // Write cert PEM
        let dir = std::env::temp_dir().join(format!("beam-tls-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cert_path = dir.join("cert.pem");
        let key_path = dir.join("key.pem");

        let cert_pem = pem::encode(&pem::Pem::new("CERTIFICATE", certs[0].to_vec()));
        std::fs::write(&cert_path, &cert_pem).unwrap();

        let key_bytes = match &key {
            PrivateKeyDer::Pkcs8(k) => k.secret_pkcs8_der(),
            _ => unreachable!(),
        };
        let key_pem = pem::encode(&pem::Pem::new("PRIVATE KEY", key_bytes.to_vec()));
        std::fs::write(&key_path, &key_pem).unwrap();

        // Load back
        let (loaded_certs, _loaded_key) =
            load_certs_from_files(cert_path.to_str().unwrap(), key_path.to_str().unwrap()).unwrap();
        assert_eq!(loaded_certs.len(), 1);
        assert_eq!(
            loaded_certs[0], certs[0],
            "Loaded cert should match original"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_certs_fails_on_missing_file() {
        let result = load_certs_from_files("/nonexistent/cert.pem", "/nonexistent/key.pem");
        assert!(result.is_err());
    }

    #[test]
    fn load_certs_fails_on_invalid_pem() {
        let dir = std::env::temp_dir().join(format!("beam-tls-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cert_path = dir.join("cert.pem");
        let key_path = dir.join("key.pem");
        std::fs::write(&cert_path, "not a real PEM").unwrap();
        std::fs::write(&key_path, "also not PEM").unwrap();

        let result = load_certs_from_files(cert_path.to_str().unwrap(), key_path.to_str().unwrap());
        assert!(result.is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn make_acceptor_from_self_signed() {
        init_crypto();
        let (certs, key) = generate_self_signed().unwrap();
        let config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .unwrap();
        let _acceptor = make_acceptor(config);
        // Just verify it doesn't panic
    }
}
