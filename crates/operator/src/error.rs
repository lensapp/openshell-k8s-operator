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

    /// The gateway rejected an in-place policy update as invalid — typically a
    /// non-additive `filesystem` change (dropping a path or flipping
    /// `includeWorkdir`), which the gateway forbids on a live sandbox. The
    /// message is the gateway's diagnostic. Terminal: it won't clear until the
    /// policy is edited, so this must not hot-loop the reconciler.
    #[error("gateway rejected policy update: {0}")]
    PolicyUpdateRejected(String),

    /// A sandbox volume is malformed (bad name, relative mount path, or an
    /// unsupported `volumeMode`). The message names the offending volume.
    #[error("invalid volume: {0}")]
    VolumeInvalid(String),

    /// A sandbox recreate deleted the gateway sandbox but it did not disappear
    /// within the poll budget (~60s), so the recreate was not completed this
    /// reconcile. Transient — the requeue retries once termination finishes.
    #[error("sandbox {name} did not finish deleting before recreate")]
    RecreateTimeout {
        /// Sandbox name.
        name: String,
    },

    /// An `OpenShellWorkspace` cannot be deleted because sandboxes or providers
    /// still reference it. Deleting a non-empty workspace permanently wedges it
    /// on the gateway (the workspace is marked terminating before the blocker
    /// check, with no undelete), so the finalizer refuses until it is empty.
    /// Transient — it clears once the referencing resources are removed.
    #[error("workspace {name} still has {count} referencing resource(s); not deleting")]
    WorkspaceNotEmpty {
        /// Workspace name.
        name: String,
        /// Number of sandboxes/providers still referencing the workspace.
        count: usize,
    },

    /// The finalizer machinery failed to apply or clean up the resource.
    #[error("finalizer error: {0}")]
    Finalizer(#[source] Box<kube::runtime::finalizer::Error<Self>>),

    /// This replica held leadership and then lost the lease (a peer took over,
    /// or renewal failed past the lease duration). The process must exit so
    /// Kubernetes restarts it as a standby rather than running two active
    /// operators against one gateway.
    #[error("lost leadership: {0}")]
    LeadershipLost(&'static str),
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
            Self::PolicyUpdateRejected(_) => "PolicyUpdateRejected",
            Self::VolumeInvalid(_) => "VolumeInvalid",
            Self::RecreateTimeout { .. } => "RecreateTimeout",
            Self::WorkspaceNotEmpty { .. } => "WorkspaceNotEmpty",
            Self::Finalizer(_) => "FinalizerError",
            Self::LeadershipLost(_) => "LeadershipLost",
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
        matches!(
            self,
            Self::PolicySourceConflict | Self::VolumeInvalid(_) | Self::PolicyUpdateRejected(_)
        )
    }
}
