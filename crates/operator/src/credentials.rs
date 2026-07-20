// SPDX-FileCopyrightText: Copyright (c) 2026 Mirantis, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Provider credential handling strategy.
//!
//! Given a provider-type profile and the credential values resolved from a
//! Secret, decide how to hand a credential to the gateway. The operator climbs
//! a ladder from least to most protective and degrades gracefully:
//!
//! 1. **Static copy** — push the raw value inline; it lives in the gateway
//!    store. The unconditional fallback.
//! 2. **Refresh** — configure a gateway-minted, short-lived credential from
//!    long-lived seed *material* (`OAuth2` refresh/client-credentials, AWS STS,
//!    Google service-account JWT). The seed is stored as refresh material; only
//!    a short-lived token is injected on the wire.
//!
//! (A future tier 3 — identity token grant, which stores no seed at all — is
//! out of scope for this module today.)
//!
//! This module is pure: it depends on neither the gateway SDK nor Kubernetes,
//! so the ladder logic is unit-tested in isolation. [`crate::gateway`] maps
//! these domain types to and from the proto.

use std::collections::{BTreeMap, BTreeSet};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A gateway-mintable credential refresh strategy — the tier-2 strategies the
/// gateway drives itself from stored seed material.
///
/// Deliberately excludes the proto's `static` and `external` strategies: those
/// are not gateway-minted, so they never yield a [`PlannedRefresh`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefreshStrategy {
    /// Exchange an `OAuth2` refresh token for a short-lived access token.
    Oauth2RefreshToken,
    /// Mint a short-lived access token via the `OAuth2` client-credentials grant.
    Oauth2ClientCredentials,
    /// Mint a Google access token from a service-account JWT assertion.
    GoogleServiceAccountJwt,
    /// Assume an AWS role for short-lived STS credentials.
    AwsStsAssumeRole,
}

/// One piece of seed material a refresh strategy consumes, as declared by a
/// provider profile credential's `refresh.material`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MaterialSpec {
    /// Material key name; also the Secret key its value is sourced from.
    pub name: String,
    /// Whether the strategy cannot operate without this material.
    pub required: bool,
    /// Whether the value is sensitive (reported to the gateway as a secret key).
    pub secret: bool,
}

/// A profile credential's declared, gateway-minted refresh behaviour.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefreshSpec {
    /// The strategy the gateway configures.
    pub strategy: RefreshStrategy,
    /// Seed material the strategy consumes.
    pub material: Vec<MaterialSpec>,
}

/// Resolved refresh configuration, ready to hand to the gateway.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefreshPlan {
    /// Strategy to configure.
    pub strategy: RefreshStrategy,
    /// Seed material values, keyed by material name, sourced from the Secret.
    pub material: BTreeMap<String, String>,
    /// Names within `material` whose values are sensitive.
    pub secret_material_keys: Vec<String>,
}

/// Required refresh material the resolved credentials did not supply.
#[derive(Clone, Debug, PartialEq, Eq)]
struct MissingMaterial {
    /// The strategy that could not be configured.
    pub strategy: RefreshStrategy,
    /// Required material names absent from the resolved credentials.
    pub missing: Vec<String>,
}

/// Collect the seed material a refresh strategy needs from the resolved
/// credential values.
///
/// Material is matched by name: a [`MaterialSpec`] named `refresh_token` is
/// sourced from the resolved key `refresh_token`. Optional material that is
/// absent is simply omitted.
///
/// # Errors
///
/// Returns [`MissingMaterial`], listing the required material names the resolved
/// credentials did not provide.
fn build_refresh_material(
    spec: &RefreshSpec,
    resolved: &BTreeMap<String, String>,
) -> Result<RefreshPlan, MissingMaterial> {
    let mut material = BTreeMap::new();
    let mut secret_material_keys = Vec::new();
    let mut missing = Vec::new();

    for item in &spec.material {
        if let Some(value) = resolved.get(&item.name) {
            material.insert(item.name.clone(), value.clone());
            if item.secret {
                secret_material_keys.push(item.name.clone());
            }
        } else if item.required {
            missing.push(item.name.clone());
        }
    }

    if missing.is_empty() {
        Ok(RefreshPlan {
            strategy: spec.strategy,
            material,
            secret_material_keys,
        })
    } else {
        Err(MissingMaterial {
            strategy: spec.strategy,
            missing,
        })
    }
}

/// How the operator handed a whole provider's credentials to the gateway,
/// surfaced in `.status.credentialMode`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
pub enum CredentialMode {
    /// Every credential is a static value stored in the gateway.
    Copied,
    /// Every credential is a gateway-minted, short-lived token; no static value
    /// is stored on the provider record.
    Refresh,
    /// A mix — some credentials static, some gateway-minted (multi-credential
    /// providers where only part of the material supports refresh).
    Mixed,
}

/// One credential the gateway will mint via refresh, paired with the provider
/// credential key it configures.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannedRefresh {
    /// Provider credential key (the profile credential name) to configure.
    pub credential_key: String,
    /// Strategy and seed material for the gateway to mint from.
    pub plan: RefreshPlan,
}

/// A credential the profile wanted to configure as a gateway-minted refresh but
/// could not, because the Secret supplied only *some* of the required material.
///
/// The partial material is copied into the gateway store as a long-lived secret
/// instead — the very exposure refresh exists to avoid — so the caller is
/// expected to surface this rather than let it pass silently. (A credential with
/// *no* material supplied is simply unused and is not reported here.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DegradedCredential {
    /// Provider credential key (the profile credential name) that fell back.
    pub credential_key: String,
    /// Required material names the Secret did not supply.
    pub missing: Vec<String>,
}

/// The full plan for handing one provider's credentials to the gateway.
///
/// Splits the resolved values into those copied verbatim and those configured
/// as gateway-minted refreshes (whose seed material is routed away from the
/// stored credentials).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProviderCredentialPlan {
    /// Values pushed into the provider's `credentials` map and stored as-is.
    pub static_credentials: BTreeMap<String, String>,
    /// Gateway-minted refreshes to configure after the provider exists.
    pub refreshes: Vec<PlannedRefresh>,
    /// Refresh-capable credentials that fell back to a static copy because the
    /// Secret supplied incomplete material — surfaced so it is not silent.
    pub degraded: Vec<DegradedCredential>,
}

impl ProviderCredentialPlan {
    /// Summarise the plan for `.status`.
    #[must_use]
    pub fn mode(&self) -> CredentialMode {
        match (
            self.refreshes.is_empty(),
            self.static_credentials.is_empty(),
        ) {
            (true, _) => CredentialMode::Copied,
            (false, true) => CredentialMode::Refresh,
            (false, false) => CredentialMode::Mixed,
        }
    }
}

/// Plan how to hand a provider's credentials to the gateway, given its profile's
/// declared credentials and the values resolved from the Secret.
///
/// For each profile credential that declares a gateway-mintable refresh and
/// whose required material the Secret supplies in full, the seed material is
/// routed into a [`PlannedRefresh`] and *removed* from the static credentials —
/// so the long-lived seed is supplied only through `ConfigureProviderRefresh`
/// and never stored as a provider credential. Everything else — profile
/// credentials without a refresh, and any Secret keys the profile does not
/// mention — is copied verbatim, matching the pre-ladder behaviour for unknown
/// provider types.
///
/// A refresh-capable credential whose material is *partially* supplied lands in
/// [`ProviderCredentialPlan::degraded`]: it falls back to a static copy, but the
/// caller is expected to warn rather than degrade silently.
///
/// AWS STS assume-role is gateway-gated behind `providers_v2`, so it is not
/// auto-selected yet: such a credential falls back to a static copy.
#[must_use]
pub fn plan_credentials<'a, I>(
    profile_credentials: I,
    resolved: &BTreeMap<String, String>,
) -> ProviderCredentialPlan
where
    I: IntoIterator<Item = (&'a str, Option<&'a RefreshSpec>)>,
{
    let mut refreshes = Vec::new();
    let mut degraded = Vec::new();
    let mut consumed: BTreeSet<String> = BTreeSet::new();

    for (name, refresh) in profile_credentials {
        let Some(spec) = refresh else { continue };
        match build_refresh_material(spec, resolved) {
            Ok(plan) => {
                if plan.strategy == RefreshStrategy::AwsStsAssumeRole {
                    // Deferred: gateway-gated behind `providers_v2`. Static copy.
                    continue;
                }
                for key in plan.material.keys() {
                    consumed.insert(key.clone());
                }
                refreshes.push(PlannedRefresh {
                    credential_key: name.to_owned(),
                    plan,
                });
            }
            Err(missing) => {
                // Required material absent. Only a signal when the user supplied
                // *some* of it — that partial material would otherwise be copied
                // into the gateway store silently. No material at all just means
                // the credential is unused.
                let partially_supplied = spec
                    .material
                    .iter()
                    .any(|item| resolved.contains_key(&item.name));
                if partially_supplied {
                    degraded.push(DegradedCredential {
                        credential_key: name.to_owned(),
                        missing: missing.missing,
                    });
                }
            }
        }
    }

    let static_credentials = resolved
        .iter()
        .filter(|(key, _)| !consumed.contains(key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();

    ProviderCredentialPlan {
        static_credentials,
        refreshes,
        degraded,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CredentialMode, MaterialSpec, RefreshSpec, RefreshStrategy, build_refresh_material,
        plan_credentials,
    };
    use std::collections::BTreeMap;

    fn resolved(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn oauth2_refresh_spec() -> RefreshSpec {
        RefreshSpec {
            strategy: RefreshStrategy::Oauth2RefreshToken,
            material: vec![
                MaterialSpec {
                    name: "client_id".to_owned(),
                    required: true,
                    secret: false,
                },
                MaterialSpec {
                    name: "refresh_token".to_owned(),
                    required: true,
                    secret: true,
                },
                MaterialSpec {
                    name: "client_secret".to_owned(),
                    required: false,
                    secret: true,
                },
            ],
        }
    }

    #[test]
    fn optional_material_absent_still_refreshes() {
        let spec = oauth2_refresh_spec();
        // client_secret is optional and absent; the two required keys are present.
        let creds = resolved(&[("client_id", "id"), ("refresh_token", "rt")]);
        let plan = build_refresh_material(&spec, &creds).expect("required material present");
        assert!(!plan.material.contains_key("client_secret"));
    }

    #[test]
    fn only_secret_material_reported_as_secret_keys() {
        let spec = oauth2_refresh_spec();
        let creds = resolved(&[
            ("client_id", "id"),
            ("refresh_token", "rt"),
            ("client_secret", "cs"),
        ]);
        let plan = build_refresh_material(&spec, &creds).expect("required material present");
        // refresh_token and client_secret are secret; client_id is not.
        assert!(
            plan.secret_material_keys
                .contains(&"refresh_token".to_owned())
        );
        assert!(
            plan.secret_material_keys
                .contains(&"client_secret".to_owned())
        );
        assert!(!plan.secret_material_keys.contains(&"client_id".to_owned()));
    }

    #[test]
    fn build_refresh_material_lists_missing_required() {
        let spec = oauth2_refresh_spec();
        let creds = resolved(&[("client_secret", "cs")]);
        let err = build_refresh_material(&spec, &creds).expect_err("required material missing");
        assert_eq!(err.strategy, RefreshStrategy::Oauth2RefreshToken);
        assert!(err.missing.contains(&"client_id".to_owned()));
        assert!(err.missing.contains(&"refresh_token".to_owned()));
    }

    fn aws_sts_spec() -> RefreshSpec {
        RefreshSpec {
            strategy: RefreshStrategy::AwsStsAssumeRole,
            material: vec![MaterialSpec {
                name: "role_arn".to_owned(),
                required: true,
                secret: false,
            }],
        }
    }

    #[test]
    fn no_profile_credentials_copies_everything() {
        let creds = resolved(&[("api_key", "sk-x"), ("region", "us")]);
        let plan = plan_credentials(std::iter::empty(), &creds);
        assert_eq!(plan.static_credentials, creds);
        assert!(plan.refreshes.is_empty());
        assert_eq!(plan.mode(), CredentialMode::Copied);
    }

    #[test]
    fn routes_refresh_material_away_from_static_credentials() {
        let spec = oauth2_refresh_spec();
        let creds = resolved(&[("client_id", "id"), ("refresh_token", "rt")]);
        let plan = plan_credentials([("gcloud_adc_token", Some(&spec))], &creds);

        // Both material keys are routed to the refresh, so nothing is copied.
        assert!(plan.static_credentials.is_empty());
        assert_eq!(plan.refreshes.len(), 1);
        assert_eq!(plan.refreshes[0].credential_key, "gcloud_adc_token");
        assert_eq!(
            plan.refreshes[0].plan.strategy,
            RefreshStrategy::Oauth2RefreshToken
        );
        assert_eq!(plan.mode(), CredentialMode::Refresh);
    }

    #[test]
    fn mixes_static_and_refreshed_credentials() {
        let spec = oauth2_refresh_spec();
        // `api_key` is a plain static credential; the oauth2 material is refreshed.
        let creds = resolved(&[
            ("api_key", "sk-x"),
            ("client_id", "id"),
            ("refresh_token", "rt"),
        ]);
        let plan = plan_credentials(
            [("api_key", None), ("gcloud_adc_token", Some(&spec))],
            &creds,
        );

        assert_eq!(plan.static_credentials.len(), 1);
        assert_eq!(
            plan.static_credentials.get("api_key"),
            Some(&"sk-x".to_owned())
        );
        assert_eq!(plan.refreshes.len(), 1);
        assert_eq!(plan.mode(), CredentialMode::Mixed);
    }

    #[test]
    fn aws_sts_is_deferred_to_static_copy() {
        let spec = aws_sts_spec();
        let creds = resolved(&[("role_arn", "arn:aws:iam::1:role/r")]);
        let plan = plan_credentials([("access_key_id", Some(&spec))], &creds);

        // Not routed: the material stays a static credential (no refresh configured).
        assert!(plan.refreshes.is_empty());
        assert_eq!(
            plan.static_credentials.get("role_arn"),
            Some(&"arn:aws:iam::1:role/r".to_owned())
        );
        assert_eq!(plan.mode(), CredentialMode::Copied);
    }

    #[test]
    fn incomplete_refresh_material_is_reported_as_degraded() {
        let spec = oauth2_refresh_spec();
        // `client_id` is supplied but the required `refresh_token` is not, so the
        // partial material is copied — and flagged rather than degraded silently.
        let creds = resolved(&[("client_id", "id")]);
        let plan = plan_credentials([("gcloud_adc_token", Some(&spec))], &creds);

        assert!(plan.refreshes.is_empty());
        assert_eq!(
            plan.static_credentials.get("client_id"),
            Some(&"id".to_owned())
        );
        assert_eq!(plan.degraded.len(), 1);
        assert_eq!(plan.degraded[0].credential_key, "gcloud_adc_token");
        assert!(
            plan.degraded[0]
                .missing
                .contains(&"refresh_token".to_owned())
        );
        assert_eq!(plan.mode(), CredentialMode::Copied);
    }

    #[test]
    fn unused_refresh_credential_is_not_degraded() {
        let spec = oauth2_refresh_spec();
        // None of the credential's material is present: it is simply unused, not
        // a botched refresh, so nothing is flagged.
        let creds = resolved(&[("api_key", "sk-x")]);
        let plan = plan_credentials([("gcloud_adc_token", Some(&spec))], &creds);

        assert!(plan.refreshes.is_empty());
        assert!(plan.degraded.is_empty());
        assert_eq!(
            plan.static_credentials.get("api_key"),
            Some(&"sk-x".to_owned())
        );
    }
}
