// SPDX-FileCopyrightText: Copyright (c) 2026 Mirantis, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Operator error type. Distinguishes gateway-side, Kubernetes-side, and
//! local failures so the reconciler's error policy can decide requeue timing.

use thiserror::Error;

/// Operator result alias.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors produced while reconciling OpenShell resources.
#[derive(Debug, Error)]
pub enum Error {
    /// A call to the OpenShell gateway failed.
    #[error("gateway error: {0}")]
    Gateway(#[from] openshell_sdk::SdkError),

    /// A Kubernetes API call failed.
    #[error("kubernetes error: {0}")]
    Kube(#[from] kube::Error),

    /// A namespaced resource arrived without a namespace (should not happen).
    #[error("resource has no namespace")]
    MissingNamespace,

    /// The finalizer machinery failed to apply or clean up the resource.
    #[error("finalizer error: {0}")]
    Finalizer(#[source] Box<kube::runtime::finalizer::Error<Self>>),
}
