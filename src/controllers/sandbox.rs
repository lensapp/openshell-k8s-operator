// SPDX-FileCopyrightText: Copyright (c) 2026 Mirantis, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Reconciliation loop for [`OpenShellSandbox`].
//!
//! Idempotent get-or-create against the gateway keyed on the resource name,
//! with a finalizer guaranteeing gateway-side cleanup on delete. Gateway state
//! is mirrored back into `.status`.

use std::sync::Arc;

use futures::StreamExt;
use kube::{
    Api, Resource, ResourceExt,
    api::{Patch, PatchParams},
    runtime::{
        Controller,
        controller::Action,
        finalizer::{Event as Finalizer, finalizer},
        watcher,
    },
};
use openshell_sdk::{SandboxPhase, SandboxSpec};
use serde_json::json;
use tracing::{info, warn};

use super::{Context, ERROR_REQUEUE_INTERVAL, REQUEUE_INTERVAL};
use crate::crd::{OpenShellSandbox, OpenShellSandboxSpec, OpenShellSandboxStatus, Phase};
use crate::error::{Error, Result};
use crate::gateway::{Gateway, SandboxState};

/// Finalizer key guaranteeing gateway-side deletion before the CR is removed.
pub const FINALIZER: &str = "openshell.lenshq.io/sandbox-cleanup";

/// Run the sandbox controller until the process is stopped.
pub async fn run(ctx: Arc<Context>) {
    let sandboxes: Api<OpenShellSandbox> = Api::all(ctx.kube.clone());

    Controller::new(sandboxes, watcher::Config::default())
        .run(reconcile, error_policy, ctx)
        .for_each(|result| async move {
            match result {
                Ok((obj, _)) => info!(sandbox = %obj.name, "reconciled"),
                Err(err) => warn!(error = %err, "sandbox reconcile loop error"),
            }
        })
        .await;
}

async fn reconcile(sandbox: Arc<OpenShellSandbox>, ctx: Arc<Context>) -> Result<Action> {
    let namespace = sandbox.namespace().ok_or(Error::MissingNamespace)?;
    let api: Api<OpenShellSandbox> = Api::namespaced(ctx.kube.clone(), &namespace);

    finalizer(&api, FINALIZER, sandbox, |event| async {
        match event {
            Finalizer::Apply(obj) => apply(obj, ctx.clone()).await,
            Finalizer::Cleanup(obj) => cleanup(obj, ctx.clone()).await,
        }
    })
    .await
    .map_err(|err| Error::Finalizer(Box::new(err)))
}

/// Create-or-sync the sandbox on the gateway and mirror its state to `.status`.
async fn apply(sandbox: Arc<OpenShellSandbox>, ctx: Arc<Context>) -> Result<Action> {
    let name = sandbox.name_any();
    let namespace = sandbox.namespace().ok_or(Error::MissingNamespace)?;
    info!(%name, %namespace, "reconciling OpenShellSandbox");

    let state = ensure_gateway_sandbox(ctx.gateway.as_ref(), &name, &sandbox.spec).await?;

    let status = OpenShellSandboxStatus {
        phase: Some(map_phase(state.phase)),
        sandbox_id: Some(state.id),
        observed_generation: sandbox.meta().generation,
    };
    patch_status(&ctx, &namespace, &name, &status).await?;

    Ok(Action::requeue(REQUEUE_INTERVAL))
}

/// Reuse the gateway sandbox if it already exists (keyed on the CR name),
/// otherwise create it. Idempotent: re-reconciles converge without duplicating.
async fn ensure_gateway_sandbox(
    gateway: &dyn Gateway,
    name: &str,
    spec: &OpenShellSandboxSpec,
) -> Result<SandboxState> {
    if let Some(existing) = gateway.get_sandbox(name).await? {
        Ok(existing)
    } else {
        info!(%name, "creating sandbox on gateway");
        gateway.create_sandbox(build_sandbox_spec(name, spec)).await
    }
}

/// Translate CR spec fields into the SDK's `SandboxSpec`.
fn build_sandbox_spec(name: &str, spec: &OpenShellSandboxSpec) -> SandboxSpec {
    SandboxSpec {
        name: Some(name.to_owned()),
        image: spec.image.clone(),
        environment: spec.environment.clone().into_iter().collect(),
        providers: spec.providers.clone(),
        gpu: spec.gpu,
        ..SandboxSpec::default()
    }
}

/// Delete the sandbox on the gateway before the finalizer releases the CR.
async fn cleanup(sandbox: Arc<OpenShellSandbox>, ctx: Arc<Context>) -> Result<Action> {
    let name = sandbox.name_any();
    info!(%name, "deleting sandbox on gateway");
    if !ctx.gateway.delete_sandbox(&name).await? {
        info!(%name, "sandbox already absent on gateway");
    }
    Ok(Action::await_change())
}

/// Merge-patch `.status`. Merge (not apply) keeps this free of field-manager
/// ceremony while remaining idempotent for these fields.
async fn patch_status(
    ctx: &Context,
    namespace: &str,
    name: &str,
    status: &OpenShellSandboxStatus,
) -> Result<()> {
    let api: Api<OpenShellSandbox> = Api::namespaced(ctx.kube.clone(), namespace);
    let patch = json!({ "status": status });
    api.patch_status(name, &PatchParams::default(), &Patch::Merge(&patch))
        .await?;
    Ok(())
}

/// Map the gateway's phase onto the CR's coarse phase.
fn map_phase(phase: SandboxPhase) -> Phase {
    match phase {
        SandboxPhase::Ready => Phase::Ready,
        SandboxPhase::Error => Phase::Error,
        SandboxPhase::Deleting => Phase::Deleting,
        // Provisioning, Unspecified, Unknown, and any future variant read as
        // still-settling.
        _ => Phase::Provisioning,
    }
}

fn error_policy(_sandbox: Arc<OpenShellSandbox>, err: &Error, _ctx: Arc<Context>) -> Action {
    warn!(error = %err, "sandbox reconcile failed; requeueing");
    Action::requeue(ERROR_REQUEUE_INTERVAL)
}

#[cfg(test)]
mod tests {
    use super::{Phase, build_sandbox_spec, ensure_gateway_sandbox, map_phase};
    use crate::crd::OpenShellSandboxSpec;
    use crate::error::Result;
    use crate::gateway::{Gateway, ProviderInput, SandboxState};
    use openshell_sdk::{SandboxPhase, SandboxSpec};
    use std::sync::Mutex;

    /// In-memory `Gateway` that records calls, for reconcile-logic tests.
    struct FakeGateway {
        existing: Option<SandboxState>,
        created: Mutex<Vec<SandboxSpec>>,
        deleted: Mutex<Vec<String>>,
    }

    impl FakeGateway {
        fn new(existing: Option<SandboxState>) -> Self {
            Self {
                existing,
                created: Mutex::new(Vec::new()),
                deleted: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl Gateway for FakeGateway {
        async fn create_sandbox(&self, spec: SandboxSpec) -> Result<SandboxState> {
            let id = spec.name.clone().unwrap_or_default();
            self.created.lock().unwrap().push(spec);
            Ok(SandboxState {
                id,
                phase: SandboxPhase::Provisioning,
            })
        }

        async fn get_sandbox(&self, _name: &str) -> Result<Option<SandboxState>> {
            Ok(self.existing.clone())
        }

        async fn delete_sandbox(&self, name: &str) -> Result<bool> {
            self.deleted.lock().unwrap().push(name.to_owned());
            Ok(true)
        }

        async fn upsert_provider(&self, _input: ProviderInput) -> Result<()> {
            unreachable!("sandbox controller does not touch providers")
        }

        async fn delete_provider(&self, _name: &str) -> Result<bool> {
            unreachable!("sandbox controller does not touch providers")
        }
    }

    fn sample_spec() -> OpenShellSandboxSpec {
        OpenShellSandboxSpec {
            image: Some("ghcr.io/example/sandbox:latest".to_owned()),
            providers: vec!["openai".to_owned()],
            gpu: true,
            ..OpenShellSandboxSpec::default()
        }
    }

    #[tokio::test]
    async fn creates_sandbox_when_absent() {
        let gateway = FakeGateway::new(None);
        let state = ensure_gateway_sandbox(&gateway, "sb-1", &sample_spec())
            .await
            .expect("reconcile");

        assert_eq!(state.id, "sb-1");
        let created = gateway.created.lock().unwrap();
        assert_eq!(created.len(), 1, "expected exactly one create");
        assert_eq!(created[0].name.as_deref(), Some("sb-1"));
        assert_eq!(
            created[0].image.as_deref(),
            Some("ghcr.io/example/sandbox:latest")
        );
        assert!(created[0].gpu);
    }

    #[tokio::test]
    async fn reuses_sandbox_when_present() {
        let existing = SandboxState {
            id: "gateway-assigned-id".to_owned(),
            phase: SandboxPhase::Ready,
        };
        let gateway = FakeGateway::new(Some(existing.clone()));
        let state = ensure_gateway_sandbox(&gateway, "sb-1", &sample_spec())
            .await
            .expect("reconcile");

        assert_eq!(state, existing);
        assert!(
            gateway.created.lock().unwrap().is_empty(),
            "must not create when the sandbox already exists"
        );
    }

    #[test]
    fn build_sandbox_spec_maps_cr_fields() {
        let spec = build_sandbox_spec("named", &sample_spec());
        assert_eq!(spec.name.as_deref(), Some("named"));
        assert_eq!(spec.providers, vec!["openai".to_owned()]);
        assert!(spec.gpu);
    }

    #[test]
    fn maps_gateway_phase_to_cr_phase() {
        assert_eq!(map_phase(SandboxPhase::Ready), Phase::Ready);
        assert_eq!(map_phase(SandboxPhase::Error), Phase::Error);
        assert_eq!(map_phase(SandboxPhase::Deleting), Phase::Deleting);
        assert_eq!(map_phase(SandboxPhase::Provisioning), Phase::Provisioning);
        // Unspecified/unknown settle as provisioning.
        assert_eq!(map_phase(SandboxPhase::Unspecified), Phase::Provisioning);
    }
}
