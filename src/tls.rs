use anyhow::{Context, Result};
use axum_server::tls_rustls::RustlsConfig;
use std::path::PathBuf;

/// Returns the directory where the cert and key are stored.
/// Same parent as config.toml: `~/.config/fileshare/`
fn tls_dir() -> PathBuf {
    crate::config::Config::config_path()
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Load an existing cert+key, or generate a fresh self-signed one and persist it.
/// The cert is valid for 10 years and covers the `fileshare.local` common name plus
/// every local IPv4 address we can find — this prevents browser SAN errors.
pub async fn load_or_generate() -> Result<RustlsConfig> {
    let dir = tls_dir();
    std::fs::create_dir_all(&dir)?;

    let cert_path = dir.join("cert.pem");
    let key_path = dir.join("key.pem");

    // Regenerate if either file is missing
    if !cert_path.exists() || !key_path.exists() {
        generate_and_save(&cert_path, &key_path)?;
    }

    RustlsConfig::from_pem_file(&cert_path, &key_path)
        .await
        .context("Failed to load TLS cert/key")
}

fn generate_and_save(cert_path: &PathBuf, key_path: &PathBuf) -> Result<()> {
    use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair, SanType};

    let mut params = CertificateParams::new(vec!["fileshare.local".to_string()])?;

    // Friendly name in the cert
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "fileshare local");
    dn.push(DnType::OrganizationName, "fileshare");
    params.distinguished_name = dn;

    // Valid for 10 years
    params.not_before = rcgen::date_time_ymd(2024, 1, 1);
    params.not_after = rcgen::date_time_ymd(2034, 1, 1);

    // Add every local IPv4 as a SAN so the browser doesn't complain about the IP
    let mut sans: Vec<SanType> = vec![SanType::DnsName("fileshare.local".try_into()?)];
    if let Ok(sock) = std::net::UdpSocket::bind("0.0.0.0:0") {
        if sock.connect("8.8.8.8:80").is_ok() {
            if let Ok(addr) = sock.local_addr() {
                if let std::net::IpAddr::V4(ip) = addr.ip() {
                    sans.push(SanType::IpAddress(std::net::IpAddr::V4(ip)));
                }
            }
        }
    }
    // Always include loopback
    sans.push(SanType::IpAddress(std::net::IpAddr::V4(
        std::net::Ipv4Addr::new(127, 0, 0, 1),
    )));
    params.subject_alt_names = sans;

    let key_pair = KeyPair::generate()?;
    let cert = params.self_signed(&key_pair)?;

    std::fs::write(cert_path, cert.pem())?;
    std::fs::write(key_path, key_pair.serialize_pem())?;

    Ok(())
}
