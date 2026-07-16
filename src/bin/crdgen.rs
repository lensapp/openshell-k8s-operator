// SPDX-FileCopyrightText: Copyright (c) 2026 Mirantis, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Generate the CRD manifest from the Rust types.
//!
//! `cargo run --bin crdgen > deploy/crds/openshellsandbox.yaml`

use kube::CustomResourceExt;
use openshell_operator::crd::OpenShellSandbox;

fn main() {
    let crd = OpenShellSandbox::crd();
    let yaml = serde_yaml::to_string(&crd).expect("serialize CRD to YAML");
    print!("{yaml}");
}
