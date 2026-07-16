// SPDX-FileCopyrightText: Copyright (c) 2026 Mirantis, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Policy document conversion.
//!
//! Translates a [`PolicySpec`] into the gateway's proto `SandboxPolicy` by
//! rendering the canonical policy document and handing it to
//! `openshell-policy::parse_sandbox_policy`. That parser is the gateway's own
//! validation authority, so this module deliberately does **not** reimplement
//! any schema checks — it only assembles the document the gateway understands.

use openshell_sdk::raw::proto;
use serde_json::{Map, Value, json};

use crate::crd::PolicySpec;
use crate::error::{Error, Result};

/// Build the proto `SandboxPolicy` for a [`PolicySpec`], validating it through
/// the gateway's parser. Returns [`Error::PolicyInvalid`] if the document is
/// rejected.
pub fn to_proto(spec: &PolicySpec) -> Result<proto::SandboxPolicy> {
    let document = canonical_document(spec);
    // The parser accepts YAML; JSON is a YAML subset but we serialize via YAML
    // to stay on the documented input path.
    let yaml = serde_yaml::to_string(&document)
        .map_err(|err| Error::PolicyInvalid(format!("failed to render policy document: {err}")))?;
    openshell_policy::parse_sandbox_policy(&yaml)
        .map_err(|err| Error::PolicyInvalid(format!("{err:#}")))
}

/// Render the canonical policy document consumed by the gateway parser.
///
/// The keys are deliberately re-spelled to the parser's `snake_case` schema
/// (`filesystem_policy`, `read_only`, …), which differs from the CRD's
/// `camelCase` surface — so this cannot be replaced by serializing `PolicySpec`
/// directly. Typed sections are emitted only when present; `networkPolicies`
/// values are passed through unmodified.
fn canonical_document(spec: &PolicySpec) -> Value {
    let mut doc = Map::new();
    doc.insert("version".to_owned(), json!(spec.version));

    if let Some(fs) = &spec.filesystem {
        doc.insert(
            "filesystem_policy".to_owned(),
            json!({
                "include_workdir": fs.include_workdir,
                "read_only": fs.read_only,
                "read_write": fs.read_write,
            }),
        );
    }

    if let Some(landlock) = &spec.landlock {
        doc.insert(
            "landlock".to_owned(),
            json!({ "compatibility": landlock.compatibility }),
        );
    }

    if let Some(process) = &spec.process {
        doc.insert(
            "process".to_owned(),
            json!({
                "run_as_user": process.run_as_user,
                "run_as_group": process.run_as_group,
            }),
        );
    }

    if !spec.network_policies.is_empty() {
        doc.insert("network_policies".to_owned(), json!(spec.network_policies));
    }

    Value::Object(doc)
}

#[cfg(test)]
mod tests {
    use super::to_proto;
    use crate::crd::{FilesystemPolicy, PolicySpec, PreservedValue, ProcessPolicy};
    use std::collections::BTreeMap;

    #[test]
    fn converts_typed_sections_to_proto() {
        let spec = PolicySpec {
            version: 1,
            filesystem: Some(FilesystemPolicy {
                include_workdir: true,
                read_only: vec!["/etc".to_owned()],
                read_write: vec!["/work".to_owned()],
            }),
            process: Some(ProcessPolicy {
                run_as_user: "sandbox".to_owned(),
                run_as_group: "sandbox".to_owned(),
            }),
            ..PolicySpec::default()
        };

        let policy = to_proto(&spec).expect("valid policy");
        assert_eq!(policy.version, 1);
        let fs = policy.filesystem.expect("filesystem present");
        assert!(fs.include_workdir);
        assert_eq!(fs.read_only, vec!["/etc".to_owned()]);
        let process = policy.process.expect("process present");
        assert_eq!(process.run_as_user, "sandbox");
    }

    #[test]
    fn passes_network_policies_through_to_parser() {
        let mut network = BTreeMap::new();
        network.insert(
            "claude_code".to_owned(),
            PreservedValue(serde_json::json!({
                "endpoints": [{ "host": "api.anthropic.com", "port": 443 }],
            })),
        );
        let spec = PolicySpec {
            version: 1,
            network_policies: network,
            ..PolicySpec::default()
        };

        let policy = to_proto(&spec).expect("valid policy");
        assert!(policy.network_policies.contains_key("claude_code"));
    }

    #[test]
    fn rejects_invalid_network_rule() {
        let mut network = BTreeMap::new();
        // `nonsense` is not a field of the gateway's network rule schema, and
        // the parser rejects unknown fields.
        network.insert(
            "bad".to_owned(),
            PreservedValue(serde_json::json!({ "nonsense": true })),
        );
        let spec = PolicySpec {
            version: 1,
            network_policies: network,
            ..PolicySpec::default()
        };

        assert!(to_proto(&spec).is_err());
    }

    #[test]
    fn empty_spec_yields_versioned_policy() {
        let spec = PolicySpec {
            version: 1,
            ..PolicySpec::default()
        };
        let policy = to_proto(&spec).expect("valid policy");
        assert_eq!(policy.version, 1);
        assert!(policy.filesystem.is_none());
        assert!(policy.network_policies.is_empty());
    }
}
