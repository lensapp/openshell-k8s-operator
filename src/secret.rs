// SPDX-FileCopyrightText: Copyright (c) 2026 Mirantis, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Resolution of provider credentials from Kubernetes Secrets.
//!
//! The Secret is always read in the referencing resource's own namespace, so a
//! `Provider` can never reach another tenant's Secrets. On top of that
//! same-namespace boundary, the Secret must explicitly opt in to being used as
//! provider credentials via [`ENTITLEMENT_ANNOTATION`] — this is the
//! entitlement check.

use std::collections::BTreeMap;

use k8s_openapi::ByteString;
use k8s_openapi::api::core::v1::Secret;
use kube::{Api, Client};

use crate::crd::SecretRef;
use crate::error::{Error, Result};

/// Annotation a Secret must carry (`= "true"`) to be referenceable as provider
/// credentials.
pub const ENTITLEMENT_ANNOTATION: &str = "openshell.lenshq.io/allow-provider-ref";

/// Fetch and resolve credential values from the referenced Secret in `namespace`.
pub async fn resolve_credentials(
    kube: &Client,
    namespace: &str,
    secret_ref: &SecretRef,
) -> Result<BTreeMap<String, String>> {
    let api: Api<Secret> = Api::namespaced(kube.clone(), namespace);
    let secret = api
        .get_opt(&secret_ref.name)
        .await?
        .ok_or_else(|| Error::SecretNotFound {
            namespace: namespace.to_owned(),
            name: secret_ref.name.clone(),
        })?;

    if !is_entitled(&secret) {
        return Err(Error::SecretNotEntitled {
            namespace: namespace.to_owned(),
            name: secret_ref.name.clone(),
            annotation: ENTITLEMENT_ANNOTATION,
        });
    }

    extract_credentials(
        namespace,
        &secret_ref.name,
        secret.data.unwrap_or_default(),
        &secret_ref.keys,
    )
}

/// Whether a Secret opts in to being referenced as provider credentials.
fn is_entitled(secret: &Secret) -> bool {
    secret
        .metadata
        .annotations
        .as_ref()
        .and_then(|annotations| annotations.get(ENTITLEMENT_ANNOTATION))
        .is_some_and(|value| value == "true")
}

/// Select and decode credential values. When `keys` is empty every key in the
/// Secret is used; otherwise exactly the requested keys, erroring if any is
/// absent. Pure over the Secret's data map, so it is unit-testable.
fn extract_credentials(
    namespace: &str,
    name: &str,
    data: BTreeMap<String, ByteString>,
    keys: &[String],
) -> Result<BTreeMap<String, String>> {
    let selected: Vec<String> = if keys.is_empty() {
        data.keys().cloned().collect()
    } else {
        keys.to_vec()
    };

    let mut credentials = BTreeMap::new();
    for key in selected {
        let value = data.get(&key).ok_or_else(|| Error::SecretKeyMissing {
            namespace: namespace.to_owned(),
            name: name.to_owned(),
            key: key.clone(),
        })?;
        let text = String::from_utf8(value.0.clone()).map_err(|_| Error::SecretValueNotUtf8 {
            namespace: namespace.to_owned(),
            name: name.to_owned(),
            key: key.clone(),
        })?;
        credentials.insert(key, text);
    }
    Ok(credentials)
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
    use std::collections::BTreeMap;

    fn data(pairs: &[(&str, &str)]) -> BTreeMap<String, ByteString> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), ByteString((*v).as_bytes().to_vec())))
            .collect()
    }

    fn secret_with_annotation(value: Option<&str>) -> Secret {
        Secret {
            metadata: ObjectMeta {
                annotations: value.map(|v| {
                    std::iter::once((ENTITLEMENT_ANNOTATION.to_owned(), v.to_owned())).collect()
                }),
                ..ObjectMeta::default()
            },
            ..Secret::default()
        }
    }

    #[test]
    fn entitlement_requires_annotation_true() {
        assert!(is_entitled(&secret_with_annotation(Some("true"))));
        assert!(!is_entitled(&secret_with_annotation(Some("false"))));
        assert!(!is_entitled(&secret_with_annotation(None)));
    }

    #[test]
    fn extracts_all_keys_when_none_requested() {
        let creds =
            extract_credentials("ns", "s", data(&[("A", "1"), ("B", "2")]), &[]).expect("extract");
        assert_eq!(creds.get("A").map(String::as_str), Some("1"));
        assert_eq!(creds.get("B").map(String::as_str), Some("2"));
    }

    #[test]
    fn extracts_only_requested_subset() {
        let keys = vec!["A".to_owned()];
        let creds = extract_credentials("ns", "s", data(&[("A", "1"), ("B", "2")]), &keys)
            .expect("extract");
        assert_eq!(creds.len(), 1);
        assert!(creds.contains_key("A"));
    }

    #[test]
    fn errors_on_missing_requested_key() {
        let keys = vec!["MISSING".to_owned()];
        let err = extract_credentials("ns", "s", data(&[("A", "1")]), &keys).unwrap_err();
        assert!(matches!(err, Error::SecretKeyMissing { .. }));
    }

    #[test]
    fn errors_on_non_utf8_value() {
        let mut d = BTreeMap::new();
        d.insert("A".to_owned(), ByteString(vec![0xff, 0xfe]));
        let err = extract_credentials("ns", "s", d, &[]).unwrap_err();
        assert!(matches!(err, Error::SecretValueNotUtf8 { .. }));
    }
}
