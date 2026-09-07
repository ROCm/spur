// Copyright (c) 2026 Advanced Micro Devices, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The TLS certificate the gateway serves.

use anyhow::{Context, Result};

use super::kube;

const ENVOY_NAMESPACE: &str = "envoy-gateway-system";
const CLUSTER_TLS_SECRET: &str = "cluster-tls";

/// Where the certificate comes from: a pair the operator staged, or one generated here.
pub enum CertSource<'a> {
    Existing { cert: &'a str, key: &'a str },
    Generate { domain: &'a str },
}

/// The names the platform stack serves. cluster-bloom builds the same list.
fn cert_sans(domain: &str) -> Vec<String> {
    ["k8s.", "kc.", "*."]
        .iter()
        .map(|prefix| format!("{prefix}{domain}"))
        .collect()
}

/// How long a generated certificate lasts. It is a convenience for a test cluster, so it expires
/// rather than living forever; a real deployment passes `--cert-option existing`.
const GENERATED_CERT_DAYS: i64 = 365;

/// Write a self-signed certificate and its key, and return where they landed.
///
/// The pair is built in this process rather than by `openssl`, which is a package a minimal node
/// may not carry. The key file is created 0600, because the node is shared.
fn write_self_signed(
    domain: &str,
    dir: &std::path::Path,
) -> Result<(std::path::PathBuf, std::path::PathBuf)> {
    let (cert_path, key_path) = (dir.join("tls.crt"), dir.join("tls.key"));
    let (cert_pem, key_pem) = self_signed_pair(domain)?;
    std::fs::write(&cert_path, cert_pem)
        .with_context(|| format!("could not write {}", cert_path.display()))?;
    write_private(&key_path, &key_pem)?;
    Ok((cert_path, key_path))
}

fn self_signed_pair(domain: &str) -> Result<(String, String)> {
    use chrono::Datelike;

    let mut params = rcgen::CertificateParams::new(cert_sans(domain))
        .with_context(|| format!("could not build a certificate for {domain}"))?;
    params.distinguished_name = rcgen::DistinguishedName::new();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, domain);
    let today = chrono::Utc::now().date_naive();
    let expiry = today + chrono::Duration::days(GENERATED_CERT_DAYS);
    params.not_before = rcgen::date_time_ymd(today.year(), today.month() as u8, today.day() as u8);
    params.not_after =
        rcgen::date_time_ymd(expiry.year(), expiry.month() as u8, expiry.day() as u8);

    let key = rcgen::KeyPair::generate().context("could not generate a private key")?;
    let cert = params
        .self_signed(&key)
        .with_context(|| format!("could not sign a certificate for {domain}"))?;
    Ok((cert.pem(), key.serialize_pem()))
}

fn write_private(path: &std::path::Path, contents: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("could not create {}", path.display()))?;
    file.write_all(contents.as_bytes())
        .with_context(|| format!("could not write {}", path.display()))
}

/// Put the TLS certificate the gateway serves into the secret its listener names. Without it the
/// `https` listener reports `InvalidCertificateRef` and never programs. cluster-bloom does this in
/// a role `install-silo` does not run, and writes the certificate under `/etc/rancher/rke2`.
pub async fn ensure_cluster_tls(source: CertSource<'_>, dir: &std::path::Path) -> Result<()> {
    if kube::exists(&["get", "secret", CLUSTER_TLS_SECRET, "-n", ENVOY_NAMESPACE]).await {
        return Ok(());
    }
    kube::apply_echoing(
        format!("apiVersion: v1\nkind: Namespace\nmetadata:\n  name: {ENVOY_NAMESPACE}\n")
            .as_bytes(),
        "the gateway namespace",
    )
    .await?;

    let (cert, key) = match source {
        CertSource::Existing { cert, key } => (
            std::path::PathBuf::from(cert),
            std::path::PathBuf::from(key),
        ),
        CertSource::Generate { domain } => write_self_signed(domain, dir)?,
    };

    // `create secret --dry-run=client` renders the YAML; piping it into `apply` avoids encoding the
    // key here, and keeps a re-run idempotent.
    let rendered = kube::kubectl()
        .args([
            "create",
            "secret",
            "tls",
            CLUSTER_TLS_SECRET,
            "-n",
            ENVOY_NAMESPACE,
        ])
        .arg("--cert")
        .arg(&cert)
        .arg("--key")
        .arg(&key)
        .args(["--dry-run=client", "-o", "yaml"])
        .output()
        .await?;
    if !rendered.status.success() {
        anyhow::bail!(
            "could not build the {CLUSTER_TLS_SECRET} secret: {}",
            String::from_utf8_lossy(&rendered.stderr).trim()
        );
    }
    kube::apply_echoing(&rendered.stdout, "the gateway certificate").await?;
    eprintln!("Created secret {CLUSTER_TLS_SECRET} in {ENVOY_NAMESPACE}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // The gateway serves these three names, so a certificate without them fails validation in a
    // browser even though the listener programs.
    #[test]
    fn the_certificate_covers_the_names_the_gateway_serves() {
        assert_eq!(
            cert_sans("cf.example.com"),
            [
                "k8s.cf.example.com",
                "kc.cf.example.com",
                "*.cf.example.com"
            ]
        );
    }

    #[test]
    fn a_generated_certificate_carries_the_domain_and_every_name() {
        let (cert, key) = self_signed_pair("cf.example.com").expect("a certificate");
        assert!(cert.starts_with("-----BEGIN CERTIFICATE-----"));
        assert!(key.contains("PRIVATE KEY-----"));

        // Parse it back rather than trust the builder: a certificate missing the wildcard still
        // programs the listener, and only fails later in a browser.
        let names = subject_alt_names(&cert);
        for expected in cert_sans("cf.example.com") {
            assert!(
                names.contains(&expected),
                "{expected} is missing from {names:?}"
            );
        }
    }

    #[test]
    fn a_generated_certificate_names_the_domain_and_expires() {
        // A pair that never expires outlives the test cluster it was made for, and the common name
        // is what an operator reads off the certificate to tell one cluster from another.
        let (cert, _) = self_signed_pair("cf.example.com").expect("a certificate");
        let pem = x509_parser::pem::parse_x509_pem(cert.as_bytes())
            .expect("valid pem")
            .1;
        let parsed = pem.parse_x509().expect("a parsable certificate");
        assert!(parsed.subject().to_string().contains("cf.example.com"));
        assert!(parsed.validity().not_after > parsed.validity().not_before);
    }

    /// The DNS names inside a PEM certificate.
    fn subject_alt_names(pem: &str) -> Vec<String> {
        let block = x509_parser::pem::parse_x509_pem(pem.as_bytes())
            .expect("valid pem")
            .1;
        let parsed = block.parse_x509().expect("a parsable certificate");
        let san = parsed
            .subject_alternative_name()
            .expect("readable extensions")
            .expect("a subjectAltName extension");
        san.value
            .general_names
            .iter()
            .filter_map(|name| match name {
                x509_parser::extensions::GeneralName::DNSName(dns) => Some((*dns).to_string()),
                _ => None,
            })
            .collect()
    }
}
