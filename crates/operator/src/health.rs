// SPDX-FileCopyrightText: Copyright (c) 2026 Mirantis, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Liveness and readiness probe endpoints.
//!
//! `/healthz` (liveness) answers 200 as long as the process can service HTTP —
//! a wedged runtime or a dead process stops answering and kubelet restarts the
//! pod. `/readyz` (readiness) answers 200 only once startup finished (gateway
//! client built), otherwise 503.
//!
//! Readiness deliberately does *not* track leadership. With leader election a
//! standby is still "ready" — able to take over — so a rolling update can mark
//! the new pod ready and retire the old leader; gating readiness on holding the
//! lease would deadlock the rollout (the new pod can't lead until the old one
//! dies, and the old one can't be retired until the new one is ready).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use tokio::net::TcpListener;
use tracing::info;

/// A cheap, cloneable handle to the operator's readiness flag. Every clone
/// shares one flag, so the startup path can flip it while the probe server reads
/// it.
#[derive(Clone, Default)]
pub struct Health {
    ready: Arc<AtomicBool>,
}

impl Health {
    /// Mark the operator ready to serve — startup is complete. Sticky: readiness
    /// never flips back, which is correct here because the process *exits* on any
    /// later failure (lost lease or a controller erroring), so there is no
    /// "ready but broken" state a probe would need to catch.
    pub fn mark_ready(&self) {
        self.ready.store(true, Ordering::Relaxed);
    }

    fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Relaxed)
    }
}

/// Bind the probe listener, returning it so the caller can fail fast on a port
/// conflict before starting real work.
pub async fn bind(listen: &str) -> anyhow::Result<TcpListener> {
    let listener = TcpListener::bind(listen).await?;
    info!(%listen, "serving health probes");
    Ok(listener)
}

/// Serve the probe endpoints on an already-bound listener until the process
/// stops.
pub async fn serve(listener: TcpListener, health: Health) -> anyhow::Result<()> {
    axum::serve(listener, router(health)).await?;
    Ok(())
}

/// The probe router. Split out so the endpoints can be exercised in tests
/// without binding a socket.
fn router(health: Health) -> Router {
    Router::new()
        .route("/healthz", get(|| async { StatusCode::OK }))
        .route("/readyz", get(readyz))
        .with_state(health)
}

async fn readyz(State(health): State<Health>) -> StatusCode {
    if health.is_ready() {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

#[cfg(test)]
mod tests {
    use super::{Health, router};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    /// Route `path` through a fresh router and return the response status.
    async fn get_status(health: Health, path: &str) -> StatusCode {
        router(health)
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn healthz_is_always_ok() {
        // Liveness ignores readiness — the process is up, so it answers 200.
        assert_eq!(
            get_status(Health::default(), "/healthz").await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn readyz_flips_with_readiness() {
        let health = Health::default();
        assert_eq!(
            get_status(health.clone(), "/readyz").await,
            StatusCode::SERVICE_UNAVAILABLE
        );
        // The flag is shared across clones: a flip through the startup handle is
        // seen by the probe router.
        health.mark_ready();
        assert_eq!(get_status(health, "/readyz").await, StatusCode::OK);
    }
}
