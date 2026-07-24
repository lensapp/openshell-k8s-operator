// SPDX-FileCopyrightText: Copyright (c) 2026 Mirantis, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Admission webhooks that confine interactive access to sandbox pods.
//!
//! An OpenShell sandbox's agent container runs privileged (root, with the caps
//! the supervisor needs). A plain `kubectl exec` therefore lands as root,
//! *outside* the per-process sandbox the supervisor applies to the workload —
//! defeating the confinement. Two webhooks close that gap for sandbox pods,
//! which are recognised by an ownerReference to `agents.x-k8s.io/Sandbox`:
//!
//! * **mutating**, on `pods/exec` (CONNECT): rewrite the exec command to
//!   re-enter the sandbox via the supervisor (`openshell-sandbox --mode=process
//!   -- <cmd>`), so the shell drops to the sandbox user under Landlock. The
//!   supervisor re-derives the policy from the live gateway.
//! * **validating**, on `pods/attach` (CONNECT) and `pods/ephemeralcontainers`
//!   (UPDATE): deny outright — attach reaches the root supervisor's stdio and a
//!   `kubectl debug` ephemeral container runs unwrapped, both bypassing the
//!   exec rewrite.
//!
//! The confinement itself lives in the upstream supervisor binary; this module
//! only translates a Kubernetes mechanism (admission) into invoking it, in
//! keeping with the operator's thin-front-end role.
//!
//! Serving certs are self-managed: [`bootstrap`] generates a self-signed CA and
//! leaf on first start, persists them in a Secret (adopting a peer replica's if
//! it raced ahead), and injects the CA into both webhook configs' `caBundle`.
//! No cert-manager, no chart-baked certs — nothing to rotate on `helm upgrade`.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, bail};
use axum::body::Bytes;
use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use axum_server::tls_rustls::RustlsConfig;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use k8s_openapi::api::admissionregistration::v1::{
    MutatingWebhookConfiguration, ValidatingWebhookConfiguration,
};
use k8s_openapi::api::core::v1::{Pod, Secret};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{Patch, PatchParams, PostParams};
use kube::{Api, Client};
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    KeyUsagePurpose, date_time_ymd,
};
use serde::{Deserialize, Serialize};
use tracing::info;

/// Default address the webhook's HTTPS listener binds to.
const DEFAULT_LISTEN: &str = "0.0.0.0:9443";
/// Wrapper argv prepended to an exec into the sandboxed agent container. The
/// supervisor re-enters the sandbox domain, then execs the user's command.
const DEFAULT_WRAPPER: [&str; 3] = [
    "/opt/openshell/bin/openshell-sandbox",
    "--mode=process",
    "--",
];
/// Env var the upstream driver sets on the agent container. Its presence marks
/// the container we rewrite execs into — matched on env, not container name,
/// since the name is an image-internal convention.
const SANDBOX_ID_ENV: &str = "OPENSHELL_SANDBOX_ID";
/// Owner `kind`/group identifying a sandbox pod (the agent-sandbox controller
/// stamps this ownerReference onto every sandbox pod the gateway creates).
const SANDBOX_OWNER_KIND: &str = "Sandbox";
const SANDBOX_OWNER_GROUP: &str = "agents.x-k8s.io";
/// Annotation kubectl reads to pick a default container; we resolve the same way.
const DEFAULT_CONTAINER_ANNOTATION: &str = "kubectl.kubernetes.io/default-container";
/// Webhook `name`s — must match the `name:` fields in the chart's webhook
/// configs, since the caBundle is injected by a strategic merge keyed on them.
const MUTATING_WEBHOOK_NAME: &str = "exec.openshell.lenshq.io";
const VALIDATING_WEBHOOK_NAME: &str = "guard.openshell.lenshq.io";

const ADMISSION_API_VERSION: &str = "admission.k8s.io/v1";
const ADMISSION_KIND: &str = "AdmissionReview";

/// Runtime configuration for the admission webhook server, from the environment.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Config {
    /// `host:port` the HTTPS listener binds to.
    pub listen: String,
    /// Namespace the serving-cert Secret and the webhook Service live in.
    pub namespace: String,
    /// Service name the webhook is reached by (drives the cert SANs).
    pub service: String,
    /// Secret the self-managed serving cert (and CA) is persisted in.
    pub secret: String,
    /// `MutatingWebhookConfiguration` the CA is injected into.
    pub mutating_config: String,
    /// `ValidatingWebhookConfiguration` the CA is injected into.
    pub validating_config: String,
    /// Argv prepended to a sandbox exec to re-enter the confinement.
    pub wrapper: Vec<String>,
}

impl Config {
    /// Read the webhook config from the environment, or `None` when the webhook
    /// is disabled (`OPENSHELL_WEBHOOK_ENABLED` unset/not `"true"`).
    ///
    /// # Errors
    ///
    /// Returns an error if the webhook is enabled but a required variable is
    /// missing, or `OPENSHELL_WEBHOOK_WRAPPER` is not a non-empty JSON string
    /// array.
    pub fn from_env() -> anyhow::Result<Option<Self>> {
        if std::env::var("OPENSHELL_WEBHOOK_ENABLED").ok().as_deref() != Some("true") {
            return Ok(None);
        }
        let wrapper = match std::env::var("OPENSHELL_WEBHOOK_WRAPPER") {
            Ok(raw) => serde_json::from_str::<Vec<String>>(&raw)
                .context("OPENSHELL_WEBHOOK_WRAPPER must be a JSON array of strings")?,
            Err(_) => DEFAULT_WRAPPER.iter().map(|s| (*s).to_owned()).collect(),
        };
        if wrapper.is_empty() {
            bail!("OPENSHELL_WEBHOOK_WRAPPER must not be empty");
        }
        Ok(Some(Self {
            listen: std::env::var("OPENSHELL_WEBHOOK_LISTEN")
                .unwrap_or_else(|_| DEFAULT_LISTEN.to_owned()),
            namespace: req_env("OPENSHELL_WEBHOOK_NAMESPACE")?,
            service: req_env("OPENSHELL_WEBHOOK_SERVICE")?,
            secret: req_env("OPENSHELL_WEBHOOK_SECRET")?,
            mutating_config: req_env("OPENSHELL_WEBHOOK_MUTATING_CONFIG")?,
            validating_config: req_env("OPENSHELL_WEBHOOK_VALIDATING_CONFIG")?,
            wrapper,
        }))
    }
}

fn req_env(key: &str) -> anyhow::Result<String> {
    std::env::var(key).with_context(|| format!("{key} is required when the webhook is enabled"))
}

/// A prepared webhook server: cert issued, caBundle injected, TLS + routes ready.
///
/// Kept separate from [`serve`] so [`bootstrap`]'s Kubernetes calls fail fast
/// during startup, before the accept loop is detached.
pub struct Prepared {
    addr: SocketAddr,
    tls: RustlsConfig,
    router: Router,
}

/// Ensure a serving cert exists, inject its CA into the webhook configs, and
/// build the TLS listener + router.
///
/// # Errors
///
/// Returns an error if the crypto provider, cert issuance/persistence, caBundle
/// injection, listen-address parse, or TLS setup fails.
pub async fn bootstrap(kube: Client, config: Config) -> anyhow::Result<Prepared> {
    // Pin the process crypto provider to `ring` (already in the TLS stack).
    // Idempotent across replicas of this process and harmless if a dependency
    // installed it first.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let bundle = ensure_cert(&kube, &config).await?;
    inject_ca(&kube, &config, &bundle.ca).await?;

    let addr: SocketAddr = config
        .listen
        .parse()
        .with_context(|| format!("invalid OPENSHELL_WEBHOOK_LISTEN: {}", config.listen))?;
    let tls = RustlsConfig::from_pem(bundle.cert.into_bytes(), bundle.key.into_bytes())
        .await
        .context("building webhook TLS config")?;
    let state = Arc::new(HandlerState {
        kube,
        wrapper: config.wrapper,
    });
    info!(%addr, "admission webhook ready");
    Ok(Prepared {
        addr,
        tls,
        router: router(state),
    })
}

/// Serve the prepared webhook until the process stops.
///
/// # Errors
///
/// Returns an error if the HTTPS server stops with one.
pub async fn serve(prepared: Prepared) -> anyhow::Result<()> {
    axum_server::bind_rustls(prepared.addr, prepared.tls)
        .serve(prepared.router.into_make_service())
        .await?;
    Ok(())
}

#[derive(Clone)]
struct HandlerState {
    kube: Client,
    wrapper: Vec<String>,
}

fn router(state: Arc<HandlerState>) -> Router {
    Router::new()
        .route("/mutate/exec", post(handle_exec))
        .route("/validate/guard", post(handle_guard))
        .with_state(state)
}

// --- admission wire types -------------------------------------------------
//
// `PodExecOptions`/`PodAttachOptions` are subresource-option types k8s-openapi
// does not generate, so the request payloads are parsed with minimal structs.

#[derive(Deserialize)]
struct AdmissionReviewRequest {
    request: AdmissionRequest,
}

#[derive(Deserialize)]
struct AdmissionRequest {
    uid: String,
    #[serde(default)]
    namespace: Option<String>,
    /// Name of the pod being exec'd/attached/debugged.
    #[serde(default)]
    name: Option<String>,
    #[serde(rename = "subResource", default)]
    sub_resource: Option<String>,
    #[serde(default)]
    object: serde_json::Value,
}

#[derive(Deserialize, Default)]
struct PodExecOptions {
    #[serde(default)]
    command: Vec<String>,
    #[serde(default)]
    container: Option<String>,
}

#[derive(Serialize)]
struct AdmissionReviewResponse {
    #[serde(rename = "apiVersion")]
    api_version: &'static str,
    kind: &'static str,
    response: AdmissionResponse,
}

#[derive(Serialize)]
struct AdmissionResponse {
    uid: String,
    allowed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<StatusMessage>,
    #[serde(rename = "patchType", skip_serializing_if = "Option::is_none")]
    patch_type: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    patch: Option<String>,
}

#[derive(Serialize)]
struct StatusMessage {
    message: String,
}

fn review(response: AdmissionResponse) -> Json<AdmissionReviewResponse> {
    Json(AdmissionReviewResponse {
        api_version: ADMISSION_API_VERSION,
        kind: ADMISSION_KIND,
        response,
    })
}

fn allow(uid: &str) -> Json<AdmissionReviewResponse> {
    review(AdmissionResponse {
        uid: uid.to_owned(),
        allowed: true,
        status: None,
        patch_type: None,
        patch: None,
    })
}

fn deny(uid: &str, message: impl Into<String>) -> Json<AdmissionReviewResponse> {
    review(AdmissionResponse {
        uid: uid.to_owned(),
        allowed: false,
        status: Some(StatusMessage {
            message: message.into(),
        }),
        patch_type: None,
        patch: None,
    })
}

fn wrapped(uid: &str, command: &[String]) -> Json<AdmissionReviewResponse> {
    let patch = serde_json::json!([{ "op": "replace", "path": "/command", "value": command }]);
    let encoded = BASE64.encode(serde_json::to_vec(&patch).unwrap_or_default());
    review(AdmissionResponse {
        uid: uid.to_owned(),
        allowed: true,
        status: None,
        patch_type: Some("JSONPatch"),
        patch: Some(encoded),
    })
}

// --- handlers -------------------------------------------------------------

async fn handle_exec(
    State(state): State<Arc<HandlerState>>,
    body: Bytes,
) -> Json<AdmissionReviewResponse> {
    let Ok(review) = serde_json::from_slice::<AdmissionReviewRequest>(&body) else {
        return deny("", "invalid AdmissionReview payload");
    };
    let req = review.request;
    let exec: PodExecOptions = serde_json::from_value(req.object).unwrap_or_default();
    let (Some(namespace), Some(name)) = (req.namespace.as_deref(), req.name.as_deref()) else {
        return deny(
            &req.uid,
            "exec request is missing the pod namespace or name",
        );
    };
    // Fail closed: if we cannot fetch the pod we cannot prove it is *not* a
    // sandbox, so we must not let a possibly-privileged exec through.
    let pod = match get_pod(&state.kube, namespace, name).await {
        Ok(pod) => pod,
        Err(err) => {
            return deny(
                &req.uid,
                format!("cannot verify sandbox status of pod {namespace}/{name}: {err}"),
            );
        }
    };
    let facts = PodFacts::from_pod(&pod);
    match decide_exec(
        &exec.command,
        exec.container.as_deref().unwrap_or(""),
        &facts,
        &state.wrapper,
    ) {
        ExecAction::Passthrough => allow(&req.uid),
        ExecAction::Wrap(command) => wrapped(&req.uid, &command),
        ExecAction::Deny(reason) => deny(&req.uid, reason),
    }
}

async fn handle_guard(
    State(state): State<Arc<HandlerState>>,
    body: Bytes,
) -> Json<AdmissionReviewResponse> {
    let Ok(review) = serde_json::from_slice::<AdmissionReviewRequest>(&body) else {
        return deny("", "invalid AdmissionReview payload");
    };
    let req = review.request;
    // `ephemeralcontainers` (UPDATE) carries the whole Pod, so classify it
    // directly; `attach` (CONNECT) carries only options, so fetch the pod.
    let is_sandbox = if req.sub_resource.as_deref() == Some("ephemeralcontainers") {
        match serde_json::from_value::<Pod>(req.object) {
            Ok(pod) => PodFacts::from_pod(&pod).is_sandbox,
            Err(err) => return deny(&req.uid, format!("cannot parse pod object: {err}")),
        }
    } else {
        let (Some(namespace), Some(name)) = (req.namespace.as_deref(), req.name.as_deref()) else {
            return deny(&req.uid, "request is missing the pod namespace or name");
        };
        match get_pod(&state.kube, namespace, name).await {
            Ok(pod) => PodFacts::from_pod(&pod).is_sandbox,
            Err(err) => {
                return deny(
                    &req.uid,
                    format!("cannot verify sandbox status of pod {namespace}/{name}: {err}"),
                );
            }
        }
    };
    if is_sandbox {
        deny(
            &req.uid,
            "attaching to, or injecting an ephemeral/debug container into, an OpenShell sandbox \
             pod is not permitted: it would bypass sandbox confinement. Use `kubectl exec`, which \
             is confined.",
        )
    } else {
        allow(&req.uid)
    }
}

async fn get_pod(kube: &Client, namespace: &str, name: &str) -> Result<Pod, kube::Error> {
    Api::<Pod>::namespaced(kube.clone(), namespace)
        .get(name)
        .await
}

// --- pure classification + decision (unit-tested below) -------------------

/// What to do with an exec into a pod.
#[derive(Debug, PartialEq, Eq)]
enum ExecAction {
    /// Not a sandbox workload — forward the exec unchanged.
    Passthrough,
    /// Sandbox agent container — replace the command with this wrapped argv.
    Wrap(Vec<String>),
    /// Sandbox pod, but exec is not allowed here — refuse with this reason.
    Deny(String),
}

/// The identity and container shape of a pod, distilled to what the exec
/// decision needs.
struct PodFacts {
    is_sandbox: bool,
    default_container: Option<String>,
    first_container: Option<String>,
    /// Containers carrying the sandbox env marker (the agent container(s)).
    agent_containers: BTreeSet<String>,
}

impl PodFacts {
    fn from_pod(pod: &Pod) -> Self {
        let is_sandbox = pod.metadata.owner_references.iter().flatten().any(|owner| {
            owner.kind == SANDBOX_OWNER_KIND
                && owner.api_version.split('/').next() == Some(SANDBOX_OWNER_GROUP)
        });
        let default_container = pod
            .metadata
            .annotations
            .as_ref()
            .and_then(|a| a.get(DEFAULT_CONTAINER_ANNOTATION))
            .cloned();
        let containers = pod
            .spec
            .as_ref()
            .map(|s| s.containers.as_slice())
            .unwrap_or_default();
        let first_container = containers.first().map(|c| c.name.clone());
        let agent_containers = containers
            .iter()
            .filter(|c| c.env.iter().flatten().any(|e| e.name == SANDBOX_ID_ENV))
            .map(|c| c.name.clone())
            .collect();
        Self {
            is_sandbox,
            default_container,
            first_container,
            agent_containers,
        }
    }

    /// Resolve which container an exec targets, mirroring the API server: the
    /// requested container, else the default-container annotation, else the
    /// first container.
    fn resolve_container<'a>(&'a self, requested: &'a str) -> Option<&'a str> {
        if !requested.is_empty() {
            return Some(requested);
        }
        self.default_container
            .as_deref()
            .or(self.first_container.as_deref())
    }
}

fn decide_exec(
    command: &[String],
    requested_container: &str,
    facts: &PodFacts,
    wrapper: &[String],
) -> ExecAction {
    if !facts.is_sandbox {
        return ExecAction::Passthrough;
    }
    let Some(target) = facts.resolve_container(requested_container) else {
        return ExecAction::Deny("unable to resolve the target container for exec".to_owned());
    };
    if !facts.agent_containers.contains(target) {
        return ExecAction::Deny(format!(
            "exec into container '{target}' of an OpenShell sandbox pod is not permitted; only the \
             sandboxed agent container may be entered, and only under confinement"
        ));
    }
    if command.is_empty() {
        return ExecAction::Deny("exec into a sandbox requires a command".to_owned());
    }
    // Idempotent under reinvocation: if another mutation re-triggers this
    // webhook after we wrapped, don't wrap twice.
    if command.starts_with(wrapper) {
        return ExecAction::Passthrough;
    }
    let mut rewritten = wrapper.to_vec();
    rewritten.extend_from_slice(command);
    ExecAction::Wrap(rewritten)
}

// --- serving cert: self-managed CA + leaf, persisted in a Secret ----------

struct CertBundle {
    cert: String,
    key: String,
    ca: String,
}

async fn ensure_cert(kube: &Client, config: &Config) -> anyhow::Result<CertBundle> {
    let api = Api::<Secret>::namespaced(kube.clone(), &config.namespace);
    if let Some(existing) = api.get_opt(&config.secret).await?
        && let Some(bundle) = read_bundle(&existing)
    {
        info!(secret = %config.secret, "reusing existing webhook serving cert");
        return Ok(bundle);
    }
    let bundle = generate_cert(&config.service, &config.namespace)?;
    let secret = Secret {
        metadata: ObjectMeta {
            name: Some(config.secret.clone()),
            namespace: Some(config.namespace.clone()),
            ..ObjectMeta::default()
        },
        string_data: Some(BTreeMap::from([
            ("tls.crt".to_owned(), bundle.cert.clone()),
            ("tls.key".to_owned(), bundle.key.clone()),
            ("ca.crt".to_owned(), bundle.ca.clone()),
        ])),
        ..Secret::default()
    };
    match api.create(&PostParams::default(), &secret).await {
        Ok(_) => {
            info!(secret = %config.secret, "generated webhook serving cert");
            Ok(bundle)
        }
        // A peer replica created it first; adopt theirs so every replica serves
        // a cert the one shared caBundle trusts.
        Err(kube::Error::Api(err)) if err.code == 409 => {
            let existing = api.get(&config.secret).await?;
            read_bundle(&existing)
                .context("peer-created webhook secret is missing tls.crt/tls.key/ca.crt")
        }
        Err(err) => Err(err.into()),
    }
}

fn read_bundle(secret: &Secret) -> Option<CertBundle> {
    let data = secret.data.as_ref()?;
    let value = |key: &str| {
        data.get(key)
            .and_then(|b| String::from_utf8(b.0.clone()).ok())
    };
    Some(CertBundle {
        cert: value("tls.crt")?,
        key: value("tls.key")?,
        ca: value("ca.crt")?,
    })
}

fn generate_cert(service: &str, namespace: &str) -> anyhow::Result<CertBundle> {
    let ca_key = KeyPair::generate().context("generating webhook CA key")?;
    let mut ca_params =
        CertificateParams::new(Vec::<String>::new()).context("building webhook CA params")?;
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    set_validity(&mut ca_params);
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "openshell-operator-webhook-ca");
    ca_params.key_usages.push(KeyUsagePurpose::KeyCertSign);
    ca_params.key_usages.push(KeyUsagePurpose::DigitalSignature);
    ca_params.key_usages.push(KeyUsagePurpose::CrlSign);
    let ca = ca_params
        .self_signed(&ca_key)
        .context("self-signing webhook CA")?;

    let dns_short = format!("{service}.{namespace}.svc");
    let dns_full = format!("{dns_short}.cluster.local");
    let leaf_key = KeyPair::generate().context("generating webhook serving key")?;
    let mut leaf_params = CertificateParams::new(vec![dns_short.clone(), dns_full])
        .context("building webhook serving-cert params")?;
    set_validity(&mut leaf_params);
    leaf_params
        .distinguished_name
        .push(DnType::CommonName, dns_short);
    leaf_params.use_authority_key_identifier_extension = true;
    leaf_params
        .key_usages
        .push(KeyUsagePurpose::DigitalSignature);
    leaf_params
        .extended_key_usages
        .push(ExtendedKeyUsagePurpose::ServerAuth);
    let leaf = leaf_params
        .signed_by(&leaf_key, &ca, &ca_key)
        .context("signing webhook cert")?;

    Ok(CertBundle {
        cert: leaf.pem(),
        key: leaf_key.serialize_pem(),
        ca: ca.pem(),
    })
}

/// Pin an effectively-permanent validity window. v1 does not rotate: the cert is
/// reused from the Secret across restarts, and rotation means deleting the Secret
/// and restarting. This is a deliberate choice (matching the long-lived bundled
/// OIDC token), stated here rather than inherited from a library default.
fn set_validity(params: &mut CertificateParams) {
    params.not_before = date_time_ymd(2020, 1, 1);
    params.not_after = date_time_ymd(2125, 1, 1);
}

async fn inject_ca(kube: &Client, config: &Config, ca_pem: &str) -> anyhow::Result<()> {
    let ca_bundle = BASE64.encode(ca_pem.as_bytes());

    let mutating = Api::<MutatingWebhookConfiguration>::all(kube.clone());
    mutating
        .patch(
            &config.mutating_config,
            &PatchParams::default(),
            &Patch::Strategic(ca_bundle_patch(MUTATING_WEBHOOK_NAME, &ca_bundle)),
        )
        .await
        .with_context(|| format!("injecting caBundle into {}", config.mutating_config))?;

    let validating = Api::<ValidatingWebhookConfiguration>::all(kube.clone());
    validating
        .patch(
            &config.validating_config,
            &PatchParams::default(),
            &Patch::Strategic(ca_bundle_patch(VALIDATING_WEBHOOK_NAME, &ca_bundle)),
        )
        .await
        .with_context(|| format!("injecting caBundle into {}", config.validating_config))?;

    info!("injected webhook caBundle");
    Ok(())
}

/// Strategic merge patch setting one named webhook's `caBundle` — merged by the
/// `name` key, leaving every other field of the config untouched.
fn ca_bundle_patch(webhook_name: &str, ca_bundle: &str) -> serde_json::Value {
    serde_json::json!({
        "webhooks": [{ "name": webhook_name, "clientConfig": { "caBundle": ca_bundle } }],
    })
}

#[cfg(test)]
mod tests {
    use super::{ExecAction, PodFacts, decide_exec};
    use k8s_openapi::api::core::v1::Pod;

    fn wrapper() -> Vec<String> {
        [
            "/opt/openshell/bin/openshell-sandbox",
            "--mode=process",
            "--",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    }

    fn cmd(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_owned()).collect()
    }

    /// A sandbox pod: owned by an `agents.x-k8s.io/Sandbox`, one `agent`
    /// container carrying the sandbox env marker plus a plain sidecar.
    fn sandbox_pod() -> Pod {
        serde_json::from_value(serde_json::json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {
                "name": "sbx",
                "ownerReferences": [
                    { "apiVersion": "agents.x-k8s.io/v1alpha1", "kind": "Sandbox",
                      "name": "sbx", "uid": "1" }
                ]
            },
            "spec": { "containers": [
                { "name": "agent", "env": [{ "name": "OPENSHELL_SANDBOX_ID", "value": "abc" }] },
                { "name": "sidecar" }
            ] }
        }))
        .unwrap()
    }

    #[test]
    fn non_sandbox_pod_passes_through() {
        let pod: Pod = serde_json::from_value(serde_json::json!({
            "metadata": { "name": "plain" },
            "spec": { "containers": [{ "name": "app" }] }
        }))
        .unwrap();
        let facts = PodFacts::from_pod(&pod);
        assert!(!facts.is_sandbox);
        assert_eq!(
            decide_exec(&cmd(&["id"]), "", &facts, &wrapper()),
            ExecAction::Passthrough
        );
    }

    #[test]
    fn agent_container_exec_is_wrapped() {
        let facts = PodFacts::from_pod(&sandbox_pod());
        let mut expected = wrapper();
        expected.extend(cmd(&["id"]));
        // Empty requested container resolves to the first container (`agent`).
        assert_eq!(
            decide_exec(&cmd(&["id"]), "", &facts, &wrapper()),
            ExecAction::Wrap(expected)
        );
    }

    #[test]
    fn explicit_agent_container_is_wrapped() {
        let facts = PodFacts::from_pod(&sandbox_pod());
        assert!(matches!(
            decide_exec(&cmd(&["sh"]), "agent", &facts, &wrapper()),
            ExecAction::Wrap(_)
        ));
    }

    #[test]
    fn already_wrapped_exec_is_not_double_wrapped() {
        let facts = PodFacts::from_pod(&sandbox_pod());
        let mut already = wrapper();
        already.extend(cmd(&["id"]));
        assert_eq!(
            decide_exec(&already, "agent", &facts, &wrapper()),
            ExecAction::Passthrough
        );
    }

    #[test]
    fn exec_into_sidecar_of_sandbox_is_denied() {
        let facts = PodFacts::from_pod(&sandbox_pod());
        assert!(matches!(
            decide_exec(&cmd(&["sh"]), "sidecar", &facts, &wrapper()),
            ExecAction::Deny(_)
        ));
    }

    #[test]
    fn empty_command_into_agent_is_denied() {
        let facts = PodFacts::from_pod(&sandbox_pod());
        assert!(matches!(
            decide_exec(&[], "agent", &facts, &wrapper()),
            ExecAction::Deny(_)
        ));
    }

    #[test]
    fn default_container_annotation_wins_over_first() {
        let pod: Pod = serde_json::from_value(serde_json::json!({
            "metadata": {
                "name": "sbx",
                "annotations": { "kubectl.kubernetes.io/default-container": "agent" },
                "ownerReferences": [
                    { "apiVersion": "agents.x-k8s.io/v1alpha1", "kind": "Sandbox",
                      "name": "sbx", "uid": "1" }
                ]
            },
            "spec": { "containers": [
                { "name": "sidecar" },
                { "name": "agent", "env": [{ "name": "OPENSHELL_SANDBOX_ID", "value": "abc" }] }
            ] }
        }))
        .unwrap();
        let facts = PodFacts::from_pod(&pod);
        // First container is the sidecar, but the annotation selects `agent`.
        assert!(matches!(
            decide_exec(&cmd(&["id"]), "", &facts, &wrapper()),
            ExecAction::Wrap(_)
        ));
    }
}
