// SPDX-FileCopyrightText: Copyright (c) 2026 Mirantis, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Standard `.status.conditions` helpers.
//!
//! The operator reports reconcile health as `metav1.Condition`s so it works
//! with the tooling that already understands them — `kubectl wait
//! --for=condition=Ready`, Argo CD / Flux health, and kstatus. Every resource
//! carries a single [`READY`] condition; its `reason` (a machine-readable
//! `PascalCase` slug) and `message` say why.
//!
//! [`set`] applies the `metav1.SetStatusCondition` rule: `lastTransitionTime`
//! only advances when `status` actually flips, so consumers can trust it as the
//! moment the resource entered its current state.

use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, Time};

/// The single summary condition every resource carries.
pub const READY: &str = "Ready";

/// Build a condition. `ok` maps to the `"True"`/`"False"` status string; `now`
/// is the candidate transition time (kept only if the status actually changes,
/// see [`set`]).
#[must_use]
pub fn condition(
    type_: &str,
    ok: bool,
    reason: &str,
    message: impl Into<String>,
    observed_generation: Option<i64>,
    now: Time,
) -> Condition {
    Condition {
        type_: type_.to_owned(),
        status: if ok { "True" } else { "False" }.to_owned(),
        reason: reason.to_owned(),
        message: message.into(),
        observed_generation,
        last_transition_time: now,
    }
}

/// Insert or update `new` in `conditions`, keyed by condition type.
///
/// When a condition of the same type already exists and its `status` is
/// unchanged, its `lastTransitionTime` is preserved (only the reason/message/
/// observedGeneration refresh); a changed status adopts `new`'s time.
pub fn set(conditions: &mut Vec<Condition>, new: Condition) {
    if let Some(existing) = conditions.iter_mut().find(|c| c.type_ == new.type_) {
        let last_transition_time = if existing.status == new.status {
            existing.last_transition_time.clone()
        } else {
            new.last_transition_time.clone()
        };
        *existing = Condition {
            last_transition_time,
            ..new
        };
    } else {
        conditions.push(new);
    }
}

#[cfg(test)]
mod tests {
    use super::{READY, condition, set};
    use chrono::{TimeZone, Utc};
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;

    fn at(secs: i64) -> Time {
        Time(
            Utc.timestamp_opt(secs, 0)
                .single()
                .expect("valid timestamp"),
        )
    }

    #[test]
    fn set_appends_a_new_condition() {
        let mut conditions = Vec::new();
        set(
            &mut conditions,
            condition(READY, true, "Reconciled", "all good", Some(3), at(100)),
        );
        assert_eq!(conditions.len(), 1);
        assert_eq!(conditions[0].status, "True");
        assert_eq!(conditions[0].observed_generation, Some(3));
    }

    #[test]
    fn set_preserves_transition_time_when_status_unchanged() {
        let mut conditions = vec![condition(READY, true, "Reconciled", "v1", Some(1), at(100))];
        set(
            &mut conditions,
            condition(READY, true, "Reconciled", "v2", Some(2), at(200)),
        );
        assert_eq!(conditions.len(), 1);
        // Time held (status stayed True) but message/generation refreshed.
        assert_eq!(conditions[0].last_transition_time, at(100));
        assert_eq!(conditions[0].message, "v2");
        assert_eq!(conditions[0].observed_generation, Some(2));
    }

    #[test]
    fn set_advances_transition_time_when_status_flips() {
        let mut conditions = vec![condition(READY, true, "Reconciled", "ok", Some(1), at(100))];
        set(
            &mut conditions,
            condition(READY, false, "GatewayError", "boom", Some(1), at(200)),
        );
        assert_eq!(conditions[0].status, "False");
        assert_eq!(conditions[0].reason, "GatewayError");
        assert_eq!(conditions[0].last_transition_time, at(200));
    }
}
