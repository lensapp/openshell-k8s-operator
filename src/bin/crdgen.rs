// SPDX-FileCopyrightText: Copyright (c) 2026 Mirantis, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Generate the CRD manifests from the Rust types.
//!
//! `cargo run --bin crdgen > deploy/crds/crds.yaml`

use kube::CustomResourceExt;
use openshell_operator::crd::{OpenShellPolicy, OpenShellProvider, OpenShellSandbox};

fn main() {
    for crd in [
        serde_yaml::to_string(&OpenShellSandbox::crd()).expect("serialize OpenShellSandbox CRD"),
        serde_yaml::to_string(&OpenShellProvider::crd()).expect("serialize OpenShellProvider CRD"),
        serde_yaml::to_string(&OpenShellPolicy::crd()).expect("serialize OpenShellPolicy CRD"),
    ] {
        println!("---");
        print!("{crd}");
    }
}
