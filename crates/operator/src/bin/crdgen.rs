// SPDX-FileCopyrightText: Copyright (c) 2026 Mirantis, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Generate the CRD manifests from the Rust types.
//!
//! `cargo run --bin crdgen > deploy/charts/openshell-operator/files/crds.yaml`

use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use kube::CustomResourceExt;
use openshell_operator::crd::{
    OpenShellPolicy, OpenShellProvider, OpenShellProviderProfile, OpenShellSandbox,
    OpenShellWorkspace,
};

/// The Helm chart renders these CRDs as ordinary release resources (so
/// `helm upgrade` updates them). Without this, `helm uninstall` would delete
/// them and cascade into every custom resource in the cluster; the annotation
/// tells Helm to keep them.
fn keep(mut crd: CustomResourceDefinition) -> CustomResourceDefinition {
    crd.metadata
        .annotations
        .get_or_insert_default()
        .insert("helm.sh/resource-policy".to_owned(), "keep".to_owned());
    crd
}

fn main() {
    for crd in [
        serde_yaml::to_string(&keep(OpenShellSandbox::crd()))
            .expect("serialize OpenShellSandbox CRD"),
        serde_yaml::to_string(&keep(OpenShellProvider::crd()))
            .expect("serialize OpenShellProvider CRD"),
        serde_yaml::to_string(&keep(OpenShellPolicy::crd()))
            .expect("serialize OpenShellPolicy CRD"),
        serde_yaml::to_string(&keep(OpenShellWorkspace::crd()))
            .expect("serialize OpenShellWorkspace CRD"),
        serde_yaml::to_string(&keep(OpenShellProviderProfile::crd()))
            .expect("serialize OpenShellProviderProfile CRD"),
    ] {
        println!("---");
        print!("{crd}");
    }
}
