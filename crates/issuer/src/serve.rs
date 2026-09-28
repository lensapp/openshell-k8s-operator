// SPDX-FileCopyrightText: Copyright (c) 2026 Mirantis, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `serve`: expose the OIDC discovery document and JWKS the gateway fetches,
//! over HTTPS with the serving cert `mint` provisioned.
//!
//! Both documents are read from files mounted from the `ConfigMap` that `mint`
//! published, at request time, so a rotation that rewrites the `ConfigMap` is
//! picked up without a restart. This process holds no signing key.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum_server::tls_rustls::RustlsConfig;
use tracing::{error, info};

/// Runtime configuration, resolved from the environment the chart injects.
struct Config {
    /// Directory the JWKS `ConfigMap` is mounted into.
    jwks_dir: PathBuf,
    /// Directory the TLS `Secret` (`tls.crt` + `tls.key`) is mounted into.
    tls_dir: PathBuf,
    listen: String,
}

impl Config {
    fn from_env() -> Self {
        Self {
            jwks_dir: std::env::var("ISSUER_CONFIG_DIR")
                .unwrap_or_else(|_| "/etc/oidc".to_string())
                .into(),
            tls_dir: std::env::var("ISSUER_TLS_DIR")
                .unwrap_or_else(|_| "/etc/oidc-tls".to_string())
                .into(),
            listen: std::env::var("ISSUER_LISTEN").unwrap_or_else(|_| "0.0.0.0:8081".to_string()),
        }
    }
}

pub async fn run() -> anyhow::Result<()> {
    let cfg = Arc::new(Config::from_env());
    let app = Router::new()
        .route("/.well-known/openid-configuration", get(discovery))
        .route("/keys", get(keys))
        .with_state(Arc::clone(&cfg));

    let addr: SocketAddr = cfg
        .listen
        .parse()
        .with_context(|| format!("invalid ISSUER_LISTEN: {}", cfg.listen))?;
    let tls = RustlsConfig::from_pem_file(cfg.tls_dir.join("tls.crt"), cfg.tls_dir.join("tls.key"))
        .await
        .with_context(|| format!("load TLS cert from {}", cfg.tls_dir.display()))?;
    info!(listen = %cfg.listen, dir = %cfg.jwks_dir.display(), "serving OIDC discovery + JWKS");
    axum_server::bind_rustls(addr, tls)
        .serve(app.into_make_service())
        .await
        .context("serve OIDC endpoints")
}

async fn discovery(State(cfg): State<Arc<Config>>) -> Response {
    serve_json(&cfg.jwks_dir.join("openid-configuration")).await
}

async fn keys(State(cfg): State<Arc<Config>>) -> Response {
    serve_json(&cfg.jwks_dir.join("jwks.json")).await
}

/// Stream a mounted JSON document back verbatim. A read failure means the
/// `ConfigMap` mount is missing or not yet populated — surface it as a 500 so
/// the gateway retries rather than caching an empty JWKS.
async fn serve_json(path: &Path) -> Response {
    match tokio::fs::read(path).await {
        Ok(bytes) => ([(header::CONTENT_TYPE, "application/json")], bytes).into_response(),
        Err(err) => {
            error!(path = %path.display(), %err, "failed to read OIDC document");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "issuer document unavailable",
            )
                .into_response()
        }
    }
}
