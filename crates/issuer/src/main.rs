// SPDX-FileCopyrightText: Copyright (c) 2026 Mirantis, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Static OIDC issuer for the operator's gateway bearer.
//!
//! Two subcommands:
//!
//! - `mint` — one-shot: generate an RS256 signing key, mint the operator's
//!   admin JWT, and publish the token `Secret` + JWKS `ConfigMap`. The private
//!   key lives only for the life of this process and is never persisted.
//! - `serve` — long-running: expose the discovery document and JWKS read from a
//!   mounted `ConfigMap`. Public material only; it cannot sign.
//!
//! Splitting the two keeps signing confined to a short-lived Job while the
//! always-on pod holds nothing secret.

mod mint;
mod serve;

use anyhow::bail;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::prelude::*;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(tracing_subscriber::fmt::layer())
        .init();

    match std::env::args().nth(1).as_deref() {
        Some("mint") => mint::run().await,
        Some("serve") => serve::run().await,
        other => bail!("usage: openshell-issuer <mint|serve> (got {other:?})"),
    }
}
