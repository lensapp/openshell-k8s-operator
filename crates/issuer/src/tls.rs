// SPDX-FileCopyrightText: Copyright (c) 2026 Mirantis, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The issuer's serving certificate: a private CA plus one leaf for the
//! issuer's Service names. The CA key signs the leaf and is then dropped, so
//! nothing can mint another cert under this CA.

use anyhow::Context;
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    KeyUsagePurpose, date_time_ymd,
};

/// PEM material the issuer serves with and the gateway trusts.
pub struct ServingCert {
    pub cert: String,
    pub key: String,
    pub ca: String,
}

/// Every name the gateway may dial the issuer by: the bare Service name (the
/// bundled gateway shares the namespace), its namespaced forms, and the host of
/// an explicit issuer URL.
pub fn subject_names(service: &str, namespace: &str, issuer_host: &str) -> Vec<String> {
    let mut names = vec![
        service.to_string(),
        format!("{service}.{namespace}"),
        format!("{service}.{namespace}.svc"),
        format!("{service}.{namespace}.svc.cluster.local"),
    ];
    if !names.iter().any(|name| name == issuer_host) {
        names.push(issuer_host.to_string());
    }
    names
}

pub fn generate(names: Vec<String>) -> anyhow::Result<ServingCert> {
    let ca_key = KeyPair::generate().context("generate issuer CA key")?;
    let mut ca_params =
        CertificateParams::new(Vec::<String>::new()).context("build issuer CA params")?;
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    set_validity(&mut ca_params);
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "openshell-issuer-ca");
    ca_params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::CrlSign,
    ];
    let ca = ca_params
        .self_signed(&ca_key)
        .context("self-sign issuer CA")?;

    let common_name = names.first().cloned().unwrap_or_default();
    let leaf_key = KeyPair::generate().context("generate issuer serving key")?;
    let mut leaf_params = CertificateParams::new(names).context("build issuer cert params")?;
    set_validity(&mut leaf_params);
    leaf_params
        .distinguished_name
        .push(DnType::CommonName, common_name);
    leaf_params.use_authority_key_identifier_extension = true;
    leaf_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    leaf_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let leaf = leaf_params
        .signed_by(&leaf_key, &ca, &ca_key)
        .context("sign issuer serving cert")?;

    Ok(ServingCert {
        cert: leaf.pem(),
        key: leaf_key.serialize_pem(),
        ca: ca.pem(),
    })
}

/// No rotation, like the long-lived operator token: rotating means deleting the
/// TLS Secret and re-running the mint Job.
fn set_validity(params: &mut CertificateParams) {
    params.not_before = date_time_ymd(2020, 1, 1);
    params.not_after = date_time_ymd(2125, 1, 1);
}

#[cfg(test)]
mod tests {
    use super::{generate, subject_names};

    #[test]
    fn subject_names_cover_service_forms_and_issuer_host() {
        let names = subject_names("openshell-issuer", "ops", "issuer.example.com");
        assert_eq!(
            names,
            [
                "openshell-issuer",
                "openshell-issuer.ops",
                "openshell-issuer.ops.svc",
                "openshell-issuer.ops.svc.cluster.local",
                "issuer.example.com",
            ]
        );
    }

    #[test]
    fn subject_names_skip_a_duplicate_issuer_host() {
        let names = subject_names("openshell-issuer", "ops", "openshell-issuer");
        assert_eq!(names.len(), 4);
    }

    #[test]
    fn generated_material_is_pem() {
        let cert = generate(subject_names("openshell-issuer", "ops", "openshell-issuer"))
            .expect("generate");
        assert!(cert.cert.starts_with("-----BEGIN CERTIFICATE-----"));
        assert!(cert.ca.starts_with("-----BEGIN CERTIFICATE-----"));
        assert!(cert.key.starts_with("-----BEGIN PRIVATE KEY-----"));
        assert_ne!(cert.cert, cert.ca);
    }
}
