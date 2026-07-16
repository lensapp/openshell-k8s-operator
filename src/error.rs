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
    /// A curated SDK gateway call failed.
    #[error("gateway error: {0}")]
    Gateway(#[from] openshell_sdk::SdkError),

    /// A raw gateway gRPC call failed.
    #[error("gateway rpc error: {0}")]
    GatewayRpc(#[from] tonic::Status),

    /// A Kubernetes API call failed.
    #[error("kubernetes error: {0}")]
    Kube(#[from] kube::Error),

    /// A namespaced resource arrived without a namespace (should not happen).
    #[error("resource has no namespace")]
    MissingNamespace,

    /// The referenced credentials Secret does not exist.
    #[error("secret {namespace}/{name} not found")]
    SecretNotFound {
        /// Namespace searched.
        namespace: String,
        /// Secret name.
        name: String,
    },

    /// The referenced Secret is not entitled to be used as provider credentials.
    #[error(
        "secret {namespace}/{name} is not entitled for provider references (set annotation {annotation}=true)"
    )]
    SecretNotEntitled {
        /// Namespace of the Secret.
        namespace: String,
        /// Secret name.
        name: String,
        /// Annotation the Secret must carry.
        annotation: &'static str,
    },

    /// A requested credential key is absent from the Secret.
    #[error("secret {namespace}/{name} is missing key {key}")]
    SecretKeyMissing {
        /// Namespace of the Secret.
        namespace: String,
        /// Secret name.
        name: String,
        /// Missing key.
        key: String,
    },

    /// A credential value is not valid UTF-8.
    #[error("secret {namespace}/{name} value for key {key} is not valid UTF-8")]
    SecretValueNotUtf8 {
        /// Namespace of the Secret.
        namespace: String,
        /// Secret name.
        name: String,
        /// Offending key.
        key: String,
    },

    /// A sandbox references an `OpenShellPolicy` that does not exist.
    #[error("policy {namespace}/{name} not found")]
    PolicyNotFound {
        /// Namespace searched.
        namespace: String,
        /// Policy name.
        name: String,
    },

    /// An `OpenShellPolicy` document failed to parse or validate against the gateway
    /// schema. The message is the parser's diagnostic.
    #[error("invalid policy: {0}")]
    PolicyInvalid(String),

    /// A sandbox set both `spec.policy` and `spec.policyRef`; they are mutually
    /// exclusive.
    #[error("specify at most one of spec.policy or spec.policyRef, not both")]
    PolicySourceConflict,

    /// A sandbox volume is malformed (bad name, relative mount path, or an
    /// unsupported `volumeMode`). The message names the offending volume.
    #[error("invalid volume: {0}")]
    VolumeInvalid(String),

    /// The finalizer machinery failed to apply or clean up the resource.
    #[error("finalizer error: {0}")]
    Finalizer(#[source] Box<kube::runtime::finalizer::Error<Self>>),
}

impl Error {
    /// A machine-readable `PascalCase` slug for this error, used as the `reason`
    /// on a `Ready=False` status condition and Kubernetes event.
    #[must_use]
    pub const fn reason(&self) -> &'static str {
        match self {
            Self::Gateway(_) | Self::GatewayRpc(_) => "GatewayError",
            Self::Kube(_) => "KubernetesError",
            Self::MissingNamespace => "MissingNamespace",
            Self::SecretNotFound { .. } => "SecretNotFound",
            Self::SecretNotEntitled { .. } => "SecretNotEntitled",
            Self::SecretKeyMissing { .. } => "SecretKeyMissing",
            Self::SecretValueNotUtf8 { .. } => "SecretValueNotUtf8",
            Self::PolicyNotFound { .. } => "PolicyNotFound",
            Self::PolicyInvalid(_) => "PolicyInvalid",
            Self::PolicySourceConflict => "PolicyConflict",
            Self::VolumeInvalid(_) => "VolumeInvalid",
            Self::Finalizer(_) => "FinalizerError",
        }
    }

    /// Whether this error can only be fixed by editing the resource spec.
    ///
    /// A terminal error will not clear on its own, so retrying it on the fast
    /// error cadence is pure churn — the spec edit that fixes it bumps
    /// `.metadata.generation` and re-triggers reconcile anyway. (`PolicyNotFound`
    /// is deliberately *not* terminal: the referenced policy may appear later,
    /// and requeueing lets the sandbox recover once it does.)
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::PolicySourceConflict | Self::VolumeInvalid(_))
    }
}
