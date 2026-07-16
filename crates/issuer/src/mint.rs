// SPDX-FileCopyrightText: Copyright (c) 2026 Mirantis, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `mint`: generate the signing key, mint the operator's admin JWT, and publish
//! it alongside a public JWKS the gateway can discover.
//!
//! Idempotent: if the token `Secret` already exists the key material is left
//! untouched, so a chart upgrade re-running the Job neither rotates the live
//! token nor invalidates the served JWKS.

use std::collections::BTreeMap;

use anyhow::Context;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use k8s_openapi::api::core::v1::{ConfigMap, Secret};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::Client;
use kube::api::{Api, Patch, PatchParams, PostParams};
use rsa::RsaPrivateKey;
use rsa::pkcs8::{EncodePrivateKey, EncodePublicKey, LineEnding};
use rsa::traits::PublicKeyParts;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tracing::info;

/// RSA modulus size. 2048 is the floor the gateway's validators accept and
/// keeps mint fast enough for a pre-install hook.
const KEY_BITS: usize = 2048;

/// Seconds in a day, for the token lifetime.
const SECONDS_PER_DAY: u64 = 86_400;

/// Runtime configuration, resolved from the environment the chart injects.
struct Config {
    namespace: String,
    /// Public issuer URL the gateway will discover (no trailing slash). Doubles
    /// as the JWT `iss`; the served discovery `issuer` must match it exactly.
    issuer_url: String,
    audience: String,
    admin_role: String,
    subject: String,
    token_secret: String,
    jwks_configmap: String,
    token_ttl_days: u64,
}

impl Config {
    fn from_env() -> anyhow::Result<Self> {
        Ok(Self {
            namespace: env("ISSUER_NAMESPACE")?,
            issuer_url: env("ISSUER_URL")?.trim_end_matches('/').to_string(),
            audience: env("ISSUER_AUDIENCE")?,
            admin_role: env_or("ISSUER_ADMIN_ROLE", "openshell-admin"),
            subject: env_or("ISSUER_SUBJECT", "openshell-operator"),
            token_secret: env("ISSUER_TOKEN_SECRET")?,
            jwks_configmap: env("ISSUER_JWKS_CONFIGMAP")?,
            token_ttl_days: env_or("ISSUER_TOKEN_TTL_DAYS", "3650")
                .parse()
                .context("ISSUER_TOKEN_TTL_DAYS must be a non-negative integer")?,
        })
    }
}

/// JWT claims. The gateway reads roles from the configurable `roles` claim and
/// grants admin when one matches its `admin_role`.
#[derive(Serialize, serde::Deserialize)]
struct Claims {
    iss: String,
    sub: String,
    aud: String,
    iat: u64,
    /// Required: the gateway rejects tokens without an expiry.
    exp: u64,
    roles: Vec<String>,
}

pub async fn run() -> anyhow::Result<()> {
    let cfg = Config::from_env()?;
    let client = Client::try_default()
        .await
        .context("build in-cluster kube client")?;

    // Idempotency gate: an existing token means a prior mint already published a
    // matching key + JWKS. Regenerating would break every live bearer.
    let secrets: Api<Secret> = Api::namespaced(client.clone(), &cfg.namespace);
    if secrets
        .get_opt(&cfg.token_secret)
        .await
        .context("look up existing token Secret")?
        .is_some()
    {
        info!(
            secret = %cfg.token_secret,
            "operator token already present; leaving key material untouched"
        );
        return Ok(());
    }

    let key = SigningKey::generate()?;
    let token = key.sign(&cfg)?;

    // Publish the public JWKS first, then the token Secret. The Secret is the
    // idempotency marker (the gate above), so it must be written last: only
    // then does an existing Secret reliably imply the matching JWKS is already
    // served. Server-side apply also overwrites a stale JWKS from a
    // half-finished prior run.
    let cms: Api<ConfigMap> = Api::namespaced(client, &cfg.namespace);
    cms.patch(
        &cfg.jwks_configmap,
        &PatchParams::apply("openshell-issuer").force(),
        &Patch::Apply(&jwks_configmap(&cfg, &key)?),
    )
    .await
    .context("apply JWKS ConfigMap")?;

    secrets
        .create(&PostParams::default(), &token_secret(&cfg, token))
        .await
        .context("create token Secret")?;

    info!(
        secret = %cfg.token_secret,
        configmap = %cfg.jwks_configmap,
        kid = %key.kid,
        "minted operator token and published JWKS"
    );
    Ok(())
}

/// An RS256 signing key plus the derived JWK material.
struct SigningKey {
    encoding: EncodingKey,
    /// Key id shared by the JWT header and the published JWK.
    kid: String,
    /// base64url big-endian modulus.
    n: String,
    /// base64url big-endian exponent.
    e: String,
}

impl SigningKey {
    fn generate() -> anyhow::Result<Self> {
        let mut rng = rand::thread_rng();
        let private = RsaPrivateKey::new(&mut rng, KEY_BITS).context("generate RSA key")?;
        let public = private.to_public_key();

        // Derive a stable kid from the public key so header and JWK always agree.
        let spki = public
            .to_public_key_der()
            .context("encode public key DER")?;
        let kid = URL_SAFE_NO_PAD.encode(Sha256::digest(spki.as_bytes()));

        let pkcs8 = private
            .to_pkcs8_pem(LineEnding::LF)
            .context("encode private key PEM")?;
        let encoding =
            EncodingKey::from_rsa_pem(pkcs8.as_bytes()).context("load RSA signing key")?;

        Ok(Self {
            encoding,
            kid,
            n: URL_SAFE_NO_PAD.encode(public.n().to_bytes_be()),
            e: URL_SAFE_NO_PAD.encode(public.e().to_bytes_be()),
        })
    }

    fn sign(&self, cfg: &Config) -> anyhow::Result<String> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .context("system clock is before the Unix epoch")?
            .as_secs();
        let claims = Claims {
            iss: cfg.issuer_url.clone(),
            sub: cfg.subject.clone(),
            aud: cfg.audience.clone(),
            iat: now,
            exp: now.saturating_add(cfg.token_ttl_days.saturating_mul(SECONDS_PER_DAY)),
            roles: vec![cfg.admin_role.clone()],
        };
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(self.kid.clone());
        jsonwebtoken::encode(&header, &claims, &self.encoding).context("sign operator JWT")
    }

    /// The single-key JWK set the gateway fetches from the issuer.
    fn jwks(&self) -> serde_json::Value {
        serde_json::json!({
            "keys": [{
                "kty": "RSA",
                "use": "sig",
                "alg": "RS256",
                "kid": self.kid,
                "n": self.n,
                "e": self.e,
            }],
        })
    }
}

fn token_secret(cfg: &Config, token: String) -> Secret {
    Secret {
        metadata: metadata(&cfg.token_secret, &cfg.namespace),
        string_data: Some(BTreeMap::from([("token".to_string(), token)])),
        ..Default::default()
    }
}

fn jwks_configmap(cfg: &Config, key: &SigningKey) -> anyhow::Result<ConfigMap> {
    let discovery = serde_json::json!({
        "issuer": cfg.issuer_url,
        "jwks_uri": format!("{}/keys", cfg.issuer_url),
        "response_types_supported": ["id_token"],
        "subject_types_supported": ["public"],
        "id_token_signing_alg_values_supported": ["RS256"],
    });
    Ok(ConfigMap {
        metadata: metadata(&cfg.jwks_configmap, &cfg.namespace),
        data: Some(BTreeMap::from([
            (
                "openid-configuration".to_string(),
                serde_json::to_string_pretty(&discovery)?,
            ),
            (
                "jwks.json".to_string(),
                serde_json::to_string_pretty(&key.jwks())?,
            ),
        ])),
        ..Default::default()
    })
}

fn metadata(name: &str, namespace: &str) -> ObjectMeta {
    ObjectMeta {
        name: Some(name.to_string()),
        namespace: Some(namespace.to_string()),
        labels: Some(BTreeMap::from([
            (
                "app.kubernetes.io/managed-by".to_string(),
                "openshell-issuer".to_string(),
            ),
            (
                "app.kubernetes.io/part-of".to_string(),
                "openshell-operator".to_string(),
            ),
        ])),
        ..Default::default()
    }
}

fn env(key: &str) -> anyhow::Result<String> {
    std::env::var(key).with_context(|| format!("{key} must be set"))
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

#[cfg(test)]
mod tests {
    use super::{Claims, Config, SigningKey};
    use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode};

    fn test_config() -> Config {
        Config {
            namespace: "openshell-system".to_string(),
            issuer_url: "https://issuer.openshell-system.svc:8081".to_string(),
            audience: "openshell-operator".to_string(),
            admin_role: "openshell-admin".to_string(),
            subject: "openshell-operator".to_string(),
            token_secret: "op-token".to_string(),
            jwks_configmap: "op-jwks".to_string(),
            token_ttl_days: 3650,
        }
    }

    /// The token the gateway receives must verify against the JWKS the gateway
    /// discovers: same `kid`, RS256, matching `iss`/`aud`, and an admin role.
    #[test]
    fn minted_token_verifies_against_published_jwks() {
        let cfg = test_config();
        let key = SigningKey::generate().expect("keygen");
        let token = key.sign(&cfg).expect("sign");

        // Rebuild the verifier the way the gateway does: from the JWK's n/e.
        let jwks = key.jwks();
        let jwk = &jwks["keys"][0];
        assert_eq!(jwk["kid"], key.kid, "header and JWK kid must agree");
        let decoding = DecodingKey::from_rsa_components(
            jwk["n"].as_str().unwrap(),
            jwk["e"].as_str().unwrap(),
        )
        .expect("decoding key");

        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_issuer(&[&cfg.issuer_url]);
        validation.set_audience(&[&cfg.audience]);
        let data = decode::<Claims>(&token, &decoding, &validation).expect("verify");

        assert_eq!(data.claims.iss, cfg.issuer_url);
        assert_eq!(data.claims.sub, cfg.subject);
        assert!(data.claims.roles.contains(&cfg.admin_role));
        assert!(
            data.claims.exp > data.claims.iat,
            "expiry must be in future"
        );
    }

    /// A tampered `kid` breaks the header/JWK pairing the gateway keys on.
    #[test]
    fn kid_is_derived_from_the_key() {
        let one = SigningKey::generate().expect("keygen");
        let two = SigningKey::generate().expect("keygen");
        assert_ne!(one.kid, two.kid, "distinct keys must yield distinct kids");
    }
}
