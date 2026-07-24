// SPDX-FileCopyrightText: Copyright (c) 2026 Mirantis, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Provider-profile document conversion.
//!
//! Translates an [`OpenShellProviderProfileSpec`] into the gateway's proto
//! `ProviderProfile` by rendering the canonical profile document and handing it
//! to `openshell-providers` — the gateway's own parser and validator. As with
//! [`crate::policy`], this module deliberately does **not** reimplement any
//! schema checks: it only assembles the document the gateway understands and
//! surfaces the parser's diagnostics.

use openshell_providers::{parse_profile_json, validate_profile_set};
use openshell_sdk::raw::proto;
use serde_json::{Map, Value, json};

use crate::crd::OpenShellProviderProfileSpec;
use crate::error::{Error, Result};

/// Build the proto `ProviderProfile` for `spec` under profile id `name`,
/// validating it through the gateway's parser. Returns [`Error::ProfileInvalid`]
/// if the document fails to parse or validate.
pub fn to_proto(name: &str, spec: &OpenShellProviderProfileSpec) -> Result<proto::ProviderProfile> {
    let document = canonical_document(name, spec);
    // The parser accepts JSON directly, and the opaque arrays already arrive as
    // JSON values, so serialize the assembled document to JSON rather than YAML.
    let json = serde_json::to_string(&document).map_err(|err| {
        Error::ProfileInvalid(format!("failed to render profile document: {err}"))
    })?;
    let profile =
        parse_profile_json(&json).map_err(|err| Error::ProfileInvalid(format!("{err}")))?;

    // Structural parsing does not catch semantic rules (id shape, duplicate
    // credential names/env vars, malformed endpoints); the gateway's own
    // validator does, and it is the same one the gateway runs on import.
    let diagnostics = validate_profile_set(&[(String::new(), profile.clone())]);
    if !diagnostics.is_empty() {
        return Err(Error::ProfileInvalid(render_diagnostics(&diagnostics)));
    }

    Ok(profile.to_proto())
}

/// Render the canonical profile document consumed by the gateway parser.
///
/// The spine keys are re-spelled to the parser's `snake_case` schema
/// (`display_name`, `inference_capable`), which differs from the CRD's
/// `camelCase` surface — so this cannot be replaced by serializing the spec
/// directly. The opaque arrays (`credentials`/`endpoints`/`binaries`/
/// `discovery`) already use the gateway's native `snake_case` schema and are
/// passed through unmodified. The profile id is the resource name.
fn canonical_document(name: &str, spec: &OpenShellProviderProfileSpec) -> Value {
    let mut doc = Map::new();
    doc.insert("id".to_owned(), json!(name));
    doc.insert("display_name".to_owned(), json!(spec.display_name));
    if let Some(description) = &spec.description {
        doc.insert("description".to_owned(), json!(description));
    }
    if let Some(category) = &spec.category {
        doc.insert("category".to_owned(), json!(category));
    }
    doc.insert(
        "inference_capable".to_owned(),
        json!(spec.inference_capable),
    );
    if !spec.credentials.is_empty() {
        doc.insert("credentials".to_owned(), json!(spec.credentials));
    }
    if !spec.endpoints.is_empty() {
        doc.insert("endpoints".to_owned(), json!(spec.endpoints));
    }
    if !spec.binaries.is_empty() {
        doc.insert("binaries".to_owned(), json!(spec.binaries));
    }
    if let Some(discovery) = &spec.discovery {
        doc.insert("discovery".to_owned(), json!(discovery));
    }
    if !spec.annotations.is_empty() {
        doc.insert("annotations".to_owned(), json!(spec.annotations));
    }
    Value::Object(doc)
}

/// Join validation diagnostics into a single human-readable message for the
/// `Ready=False` condition, in the parser's own `field: message` shape.
fn render_diagnostics(diagnostics: &[openshell_providers::ProfileValidationDiagnostic]) -> String {
    diagnostics
        .iter()
        .map(|diagnostic| format!("{}: {}", diagnostic.field, diagnostic.message))
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod tests {
    use super::to_proto;
    use crate::crd::{OpenShellProviderProfileSpec, PreservedValue};
    use serde_json::json;

    fn spec() -> OpenShellProviderProfileSpec {
        OpenShellProviderProfileSpec {
            display_name: "Widget".to_owned(),
            ..OpenShellProviderProfileSpec::default()
        }
    }

    #[test]
    fn renders_spine_fields_to_proto() {
        let mut spec = spec();
        spec.description = Some("a widget provider".to_owned());
        spec.category = Some("inference".to_owned());
        spec.inference_capable = true;

        let profile = to_proto("widget", &spec).expect("valid profile");
        assert_eq!(profile.id, "widget");
        assert_eq!(profile.display_name, "Widget");
        assert_eq!(profile.description, "a widget provider");
        assert!(profile.inference_capable);
        // New profiles carry no stored resource version.
        assert_eq!(profile.resource_version, 0);
    }

    #[test]
    fn passes_opaque_credentials_through_to_parser() {
        let mut spec = spec();
        spec.credentials = vec![PreservedValue(json!({
            "name": "api_key",
            "env_vars": ["WIDGET_API_KEY"],
            "required": true,
        }))];

        let profile = to_proto("widget", &spec).expect("valid profile");
        assert_eq!(profile.credentials.len(), 1);
        assert_eq!(profile.credentials[0].name, "api_key");
        assert_eq!(profile.credentials[0].env_vars, vec!["WIDGET_API_KEY"]);
    }

    #[test]
    fn rejects_unsupported_category() {
        let mut spec = spec();
        spec.category = Some("nonsense".to_owned());
        // The parser's category deserializer rejects unknown values.
        assert!(to_proto("widget", &spec).is_err());
    }

    #[test]
    fn rejects_non_kebab_id() {
        // The gateway requires lowercase kebab-case ids; a dotted resource name
        // is rejected by the validator rather than silently imported.
        assert!(to_proto("Widget.Co", &spec()).is_err());
    }

    #[test]
    fn rejects_duplicate_credential_names() {
        let mut spec = spec();
        let credential = |env: &str| {
            PreservedValue(json!({
                "name": "api_key",
                "env_vars": [env],
            }))
        };
        spec.credentials = vec![credential("A"), credential("B")];
        assert!(to_proto("widget", &spec).is_err());
    }
}
