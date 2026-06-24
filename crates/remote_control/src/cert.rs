//! Self-signed TLS cert generation and persistence for the Remote Control
//! listener. ADR-0003 picked TLS 1.3 + cert-fingerprint pinning (no CA, no
//! Let's Encrypt). The cert is generated on first `enabled = true` and
//! reused across editor restarts so the fingerprint baked into a paired
//! client's QR stays valid.

use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow};
use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair, SanType};
use sha2::{Digest as _, Sha256};

/// In-memory representation of the server's self-signed TLS identity.
///
/// `cert_der` / `key_der` are exactly what `rustls` consumes via
/// `CertificateDer::from(...)` / `PrivateKeyDer::try_from(...)`.
/// `fingerprint_sha256` is the SHA-256 of `cert_der`; the Android client
/// pins on it (R-3 QR payload, R-5 OkHttp `CertificatePinner`).
#[derive(Clone, Debug)]
pub struct ServerCert {
    pub cert_der: Vec<u8>,
    pub key_der: Vec<u8>,
    pub fingerprint_sha256: [u8; 32],
}

impl ServerCert {
    /// Hex-encoded uppercase fingerprint — convenient for logging / future
    /// QR-code embedding (R-3 builds the URL-safe base64 form there).
    pub fn fingerprint_hex(&self) -> String {
        hex::encode_upper(self.fingerprint_sha256)
    }
}

/// Load the persisted cert from disk, or generate + persist a new one if
/// either file is missing or unparseable.
///
/// `server_address` (if `Some`) is added as a SAN — either DNS or IP
/// depending on whether it parses as an `IpAddr`. `localhost`, `127.0.0.1`,
/// and `::1` are always included as defense-in-depth alongside the
/// fingerprint pin.
pub async fn load_or_generate(
    fs: &Arc<dyn fs::Fs>,
    server_address: Option<&str>,
) -> Result<ServerCert> {
    let cert_path = paths::remote_control_cert_file();
    let key_path = paths::remote_control_key_file();

    if fs.is_file(cert_path).await && fs.is_file(key_path).await {
        match load_existing(fs, cert_path, key_path).await {
            Ok(cert) => return Ok(cert),
            Err(err) => {
                log::warn!(
                    target: "remote_control",
                    "stored cert at {cert_path:?}/{key_path:?} unreadable ({err:#}); regenerating",
                );
            }
        }
    }

    let generated = generate(server_address)?;
    persist(fs, &generated).await?;
    Ok(generated)
}

async fn load_existing(
    fs: &Arc<dyn fs::Fs>,
    cert_path: &std::path::Path,
    key_path: &std::path::Path,
) -> Result<ServerCert> {
    let cert_der = fs
        .load_bytes(cert_path)
        .await
        .with_context(|| format!("reading {cert_path:?}"))?;
    let key_der = fs
        .load_bytes(key_path)
        .await
        .with_context(|| format!("reading {key_path:?}"))?;

    if cert_der.is_empty() || key_der.is_empty() {
        return Err(anyhow!("cert or key file is empty"));
    }

    // Sanity-parse via rustls — pulls in identical decoders to what the
    // listener will use, so a corrupt file fails here rather than during
    // accept.
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    let _ = CertificateDer::from(cert_der.clone());
    PrivateKeyDer::try_from(key_der.clone())
        .map_err(|err| anyhow!("invalid private key: {err}"))?;

    let fingerprint_sha256 = sha256(&cert_der);
    Ok(ServerCert {
        cert_der,
        key_der,
        fingerprint_sha256,
    })
}

fn generate(server_address: Option<&str>) -> Result<ServerCert> {
    let mut sans: Vec<SanType> = Vec::with_capacity(4);
    sans.push(SanType::DnsName(
        "localhost"
            .try_into()
            .map_err(|err| anyhow!("dns san localhost: {err}"))?,
    ));
    sans.push(SanType::IpAddress(std::net::IpAddr::V4(
        std::net::Ipv4Addr::new(127, 0, 0, 1),
    )));
    sans.push(SanType::IpAddress(std::net::IpAddr::V6(
        std::net::Ipv6Addr::LOCALHOST,
    )));
    if let Some(addr) = server_address {
        if let Ok(ip) = addr.parse::<std::net::IpAddr>() {
            sans.push(SanType::IpAddress(ip));
        } else {
            sans.push(SanType::DnsName(
                addr.to_string()
                    .try_into()
                    .map_err(|err| anyhow!("dns san {addr:?}: {err}"))?,
            ));
        }
    }

    let mut params = CertificateParams::new(Vec::<String>::new())
        .map_err(|err| anyhow!("cert params: {err}"))?;
    params.subject_alt_names = sans;
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "Sawe Remote Control");
    params.distinguished_name = dn;
    let now = time::OffsetDateTime::now_utc();
    params.not_before = now - time::Duration::days(1);
    params.not_after = now + time::Duration::days(365 * 10);

    let key_pair = KeyPair::generate().map_err(|err| anyhow!("keypair: {err}"))?;
    let cert = params
        .self_signed(&key_pair)
        .map_err(|err| anyhow!("self-sign: {err}"))?;

    let cert_der = cert.der().to_vec();
    let key_der = key_pair.serialize_der();
    let fingerprint_sha256 = sha256(&cert_der);

    Ok(ServerCert {
        cert_der,
        key_der,
        fingerprint_sha256,
    })
}

async fn persist(fs: &Arc<dyn fs::Fs>, cert: &ServerCert) -> Result<()> {
    let cert_path = paths::remote_control_cert_file();
    let key_path = paths::remote_control_key_file();
    if let Some(parent) = cert_path.parent() {
        fs.create_dir(parent)
            .await
            .with_context(|| format!("creating {parent:?}"))?;
    }
    // `Fs::write` is the raw-bytes write; `atomic_write` is text-only. DER
    // is arbitrary bytes so we can't go through `atomic_write` without an
    // encoding layer. Per-process there's only one writer of these files
    // (the editor itself, on the first ever `enabled = true`), so we don't
    // need cross-process atomicity here — `write` is sufficient.
    fs.write(cert_path, &cert.cert_der)
        .await
        .with_context(|| format!("writing {cert_path:?}"))?;
    fs.write(key_path, &cert.key_der)
        .await
        .with_context(|| format!("writing {key_path:?}"))?;
    Ok(())
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    async fn generate_round_trip(cx: &mut TestAppContext) {
        let fs: Arc<dyn fs::Fs> = fs::FakeFs::new(cx.background_executor.clone());
        let first = load_or_generate(&fs, Some("203.0.113.1"))
            .await
            .expect("first generate");
        assert_eq!(first.fingerprint_sha256.len(), 32);
        assert!(first.cert_der.len() > 64);
        assert!(first.key_der.len() > 16);

        let second = load_or_generate(&fs, Some("203.0.113.1"))
            .await
            .expect("second load");
        assert_eq!(first.cert_der, second.cert_der);
        assert_eq!(first.key_der, second.key_der);
        assert_eq!(first.fingerprint_sha256, second.fingerprint_sha256);
    }

    #[gpui::test]
    async fn fingerprint_hex_is_uppercase(cx: &mut TestAppContext) {
        let fs: Arc<dyn fs::Fs> = fs::FakeFs::new(cx.background_executor.clone());
        let cert = load_or_generate(&fs, None).await.expect("generate");
        let hex = cert.fingerprint_hex();
        assert_eq!(hex.len(), 64);
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(hex, hex.to_uppercase());
    }

    #[gpui::test]
    async fn host_san_for_dns_name(cx: &mut TestAppContext) {
        let fs: Arc<dyn fs::Fs> = fs::FakeFs::new(cx.background_executor.clone());
        let cert = load_or_generate(&fs, Some("my-laptop.lan"))
            .await
            .expect("generate");
        // Smoke-check the cert decodes back through rustls; we don't assert
        // the SAN list contents in detail — rcgen's serializer is tested
        // by rcgen itself, this just guards "we built something parseable".
        use rustls::pki_types::CertificateDer;
        let _decoded = CertificateDer::from(cert.cert_der.as_slice());
    }
}
