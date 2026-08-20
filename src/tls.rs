use anyhow::{Context, Result};
use std::fs::File;
use std::io::BufReader;
use std::sync::Arc;

/// Load a rustls server configuration from PEM certificate/key files.
pub fn load_server_config(
    cert_path: &str,
    key_path: &str,
    http2: bool,
) -> Result<Arc<rustls::ServerConfig>> {
    let certs = load_certs(cert_path)?;
    let key = load_private_key(key_path)?;

    let mut config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("Failed to build TLS server configuration")?;

    // HTTP/2 over TLS requires ALPN negotiation.
    config.alpn_protocols = if http2 {
        vec![b"h2".to_vec(), b"http/1.1".to_vec()]
    } else {
        vec![b"http/1.1".to_vec()]
    };

    Ok(Arc::new(config))
}

fn load_certs(path: &str) -> Result<Vec<rustls::pki_types::CertificateDer<'static>>> {
    let file = File::open(path).with_context(|| format!("Failed to open certificate file {}", path))?;
    let mut reader = BufReader::new(file);

    let certs = rustls_pemfile::certs(&mut reader)
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("Failed to parse certificate file {}", path))?;

    if certs.is_empty() {
        anyhow::bail!("No certificates found in {}", path);
    }

    Ok(certs)
}

fn load_private_key(path: &str) -> Result<rustls::pki_types::PrivateKeyDer<'static>> {
    let file = File::open(path).with_context(|| format!("Failed to open key file {}", path))?;
    let mut reader = BufReader::new(file);

    let key = rustls_pemfile::private_key(&mut reader)
        .with_context(|| format!("Failed to parse key file {}", path))?
        .with_context(|| format!("No private key found in {}", path))?;

    Ok(key)
}
