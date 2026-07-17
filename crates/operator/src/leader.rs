// SPDX-FileCopyrightText: Copyright (c) 2026 Mirantis, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Lease-based leader election.
//!
//! `kube-runtime` ships no leader election and the maintained crate pins a
//! different `kube` major, so we implement the minimal client-go algorithm over
//! a `coordination.k8s.io/v1` Lease: a single active replica holds the Lease and
//! renews it; standbys poll and take over only once it expires. The winner runs
//! the controllers; the losers wait.
//!
//! Expiry is measured from when *this* process first observed the current
//! `renewTime`, not from the (possibly clock-skewed) timestamp in the object, so
//! a standby with a fast clock never evicts a leader that is in fact still
//! renewing. Reconcilers are idempotent, so the brief overlap a handover can
//! cause is harmless; correctness never depends on election being perfect.

use std::future::Future;
use std::time::{Duration, Instant};

use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime;
use kube::Client;
use kube::api::{Api, ObjectMeta, PostParams};
use tokio::time::sleep;
use tracing::{info, warn};

use crate::error::{Error, Result};

/// How long a lease stays valid without a renewal.
const LEASE_DURATION: Duration = Duration::from_secs(15);
/// How often the leader renews, and how often a standby re-checks. Well under
/// `LEASE_DURATION` so a healthy leader renews several times per lease window.
const RETRY_PERIOD: Duration = Duration::from_secs(2);

/// Identity and location of the lease a replica contends for.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Config {
    /// Lease object name (shared by all replicas of one release).
    pub lease_name: String,
    /// Namespace the lease lives in.
    pub namespace: String,
    /// Unique holder identity — the pod name, so a takeover names its peer.
    pub identity: String,
}

impl Config {
    /// Build a config from the environment, or `None` if leader election is not
    /// configured (no lease name), in which case the caller runs the controllers
    /// directly. The chart sets all three variables together (lease name plus
    /// the downward-API pod name/namespace); a bare `cargo run` sets none.
    #[must_use]
    pub fn from_env() -> Option<Self> {
        Some(Self {
            lease_name: std::env::var("OPENSHELL_LEADER_ELECTION_LEASE").ok()?,
            namespace: std::env::var("POD_NAMESPACE").ok()?,
            identity: std::env::var("POD_NAME").ok()?,
        })
    }
}

/// What to do with the lease this reconcile step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Decision {
    /// We already hold the lease; renew it.
    Renew,
    /// The lease is free (unheld or expired); try to seize it.
    Acquire,
    /// A peer holds a still-valid lease; back off and re-check later.
    Wait,
}

/// Decide what to do with a lease purely from its holder and how long its
/// `renewTime` has looked unchanged from our vantage point.
///
/// `unchanged_for` is `None` when we have no prior observation of the current
/// `renewTime` — it just changed, or we have never seen it — so a freshly
/// renewed lease is never mistaken for an expired one. `Some(d)` is how long it
/// has stood still locally.
fn decide(holder: Option<&str>, identity: &str, unchanged_for: Option<Duration>) -> Decision {
    if holder == Some(identity) {
        return Decision::Renew;
    }
    match holder {
        // Unheld — free to take.
        None => Decision::Acquire,
        // Held by a peer: seize only once its renewTime has been frozen, from
        // our own clock, for a full lease duration.
        Some(_) => match unchanged_for {
            Some(elapsed) if elapsed >= LEASE_DURATION => Decision::Acquire,
            _ => Decision::Wait,
        },
    }
}

/// A record of the lease's `renewTime` and when this process first saw that
/// value locally. Underpins the clock-skew-safe expiry in [`decide`].
#[derive(Clone, Debug, PartialEq, Eq)]
struct Observation {
    renew_time: Option<MicroTime>,
    first_seen: Instant,
}

/// Tracks the most recent lease observation to derive `unchanged_for`.
#[derive(Default)]
struct Tracker {
    last: Option<Observation>,
}

impl Tracker {
    /// Fold in the `renewTime` seen at `now`, returning how long it has been
    /// unchanged locally — or `None` when it just changed (timer resets).
    fn observe(&mut self, renew_time: Option<MicroTime>, now: Instant) -> Option<Duration> {
        match &self.last {
            Some(prev) if prev.renew_time == renew_time => {
                Some(now.duration_since(prev.first_seen))
            }
            _ => {
                self.last = Some(Observation {
                    renew_time,
                    first_seen: now,
                });
                None
            }
        }
    }
}

/// Contends for a lease and reports whether this replica holds it.
struct Elector {
    api: Api<Lease>,
    config: Config,
    tracker: Tracker,
}

impl Elector {
    /// One acquire-or-renew attempt against the cluster. Optimistic concurrency:
    /// a 409 on create or replace means a peer won the race, i.e. not us.
    async fn step(&mut self) -> Result<Decision> {
        let now = MicroTime(chrono::Utc::now());
        let Some(existing) = self.api.get_opt(&self.config.lease_name).await? else {
            // No lease object yet — create it with us as the holder.
            let lease = self.new_lease(&now);
            return match self.api.create(&PostParams::default(), &lease).await {
                Ok(_) => Ok(Decision::Acquire),
                Err(kube::Error::Api(err)) if err.code == 409 => Ok(Decision::Wait),
                Err(err) => Err(err.into()),
            };
        };

        let spec = existing.spec.clone().unwrap_or_default();
        let unchanged_for = self
            .tracker
            .observe(spec.renew_time.clone(), Instant::now());
        let decision = decide(
            spec.holder_identity.as_deref(),
            &self.config.identity,
            unchanged_for,
        );
        if decision == Decision::Wait {
            return Ok(Decision::Wait);
        }

        let transition = decision == Decision::Acquire;
        let mut updated = existing;
        updated.spec = Some(self.renewed_spec(&spec, &now, transition));
        match self
            .api
            .replace(&self.config.lease_name, &PostParams::default(), &updated)
            .await
        {
            Ok(_) => Ok(decision),
            // The lease changed under us — a peer got there first.
            Err(kube::Error::Api(err)) if err.code == 409 => Ok(Decision::Wait),
            Err(err) => Err(err.into()),
        }
    }

    /// A brand-new lease object owned by us.
    fn new_lease(&self, now: &MicroTime) -> Lease {
        Lease {
            metadata: ObjectMeta {
                name: Some(self.config.lease_name.clone()),
                namespace: Some(self.config.namespace.clone()),
                ..ObjectMeta::default()
            },
            spec: Some(self.renewed_spec(&LeaseSpec::default(), now, true)),
        }
    }

    /// The lease spec after we renew (`transition == false`) or take it over
    /// (`transition == true`, which bumps `leaseTransitions` and stamps a fresh
    /// `acquireTime`, matching client-go semantics).
    fn renewed_spec(&self, prior: &LeaseSpec, now: &MicroTime, transition: bool) -> LeaseSpec {
        let transitions = prior.lease_transitions.unwrap_or(0) + i32::from(transition);
        let acquire_time = if transition {
            Some(now.clone())
        } else {
            prior.acquire_time.clone().or_else(|| Some(now.clone()))
        };
        LeaseSpec {
            holder_identity: Some(self.config.identity.clone()),
            lease_duration_seconds: i32::try_from(LEASE_DURATION.as_secs()).ok(),
            acquire_time,
            renew_time: Some(now.clone()),
            lease_transitions: Some(transitions),
            ..LeaseSpec::default()
        }
    }

    /// Block, retrying, until this replica holds the lease.
    async fn campaign(&mut self) {
        info!(
            identity = %self.config.identity,
            lease = %self.config.lease_name,
            "waiting for leadership"
        );
        loop {
            match self.step().await {
                Ok(Decision::Renew | Decision::Acquire) => return,
                Ok(Decision::Wait) => {}
                Err(err) => warn!(error = %err, "leader election step failed; retrying"),
            }
            sleep(RETRY_PERIOD).await;
        }
    }

    /// Renew on every tick for as long as we hold the lease. Returns once
    /// leadership is lost — a peer took over, or renewal failed for longer than
    /// a lease duration — so the caller can step down.
    async fn keep_leadership(&mut self) -> Error {
        let mut last_renewed = Instant::now();
        loop {
            sleep(RETRY_PERIOD).await;
            match self.step().await {
                Ok(Decision::Renew) => last_renewed = Instant::now(),
                Ok(Decision::Acquire) => {
                    // We re-took the lease, so it had lapsed since our last renew
                    // — a peer could have briefly held it. Idempotency covers the
                    // overlap; surface it so a real handover isn't silent.
                    warn!("re-acquired lease after a lapse in renewal");
                    last_renewed = Instant::now();
                }
                Ok(Decision::Wait) => return Error::LeadershipLost("a peer holds the lease"),
                Err(err) => {
                    warn!(error = %err, "failed to renew lease");
                    // Tolerate a transient API blip, but once we could not renew
                    // for a full lease duration the lease has expired and a peer
                    // may already have taken over — step down.
                    if last_renewed.elapsed() >= LEASE_DURATION {
                        return Error::LeadershipLost("could not renew within lease duration");
                    }
                }
            }
        }
    }
}

/// Win the lease, then run `controllers` until they finish or leadership is lost.
///
/// Losing the lease returns [`Error::LeadershipLost`]; `main` exits non-zero on
/// it so Kubernetes restarts this replica as a standby rather than leaving two
/// active operators driving one gateway.
pub async fn run<F, Fut>(client: Client, config: Config, controllers: F) -> Result<()>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<()>>,
{
    let api = Api::<Lease>::namespaced(client, &config.namespace);
    let mut elector = Elector {
        api,
        config,
        tracker: Tracker::default(),
    };
    elector.campaign().await;
    info!(
        identity = %elector.config.identity,
        lease = %elector.config.lease_name,
        "acquired leadership; starting controllers"
    );
    tokio::select! {
        result = controllers() => result,
        lost = elector.keep_leadership() => Err(lost),
    }
}

#[cfg(test)]
mod tests {
    use super::{Decision, LEASE_DURATION, Tracker, decide};
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime;
    use std::time::{Duration, Instant};

    /// A distinct, deterministic timestamp per `secs` — never the wall clock, so
    /// two calls with different `secs` are guaranteed unequal (a clock read can
    /// repeat within a tick and make the test flaky).
    fn a_time(secs: i64) -> MicroTime {
        MicroTime(chrono::DateTime::from_timestamp(secs, 0).expect("valid timestamp"))
    }

    #[test]
    fn our_own_lease_is_always_renewed() {
        // Holder is us — renew regardless of how the timer looks.
        assert_eq!(decide(Some("me"), "me", None), Decision::Renew);
        assert_eq!(
            decide(Some("me"), "me", Some(LEASE_DURATION)),
            Decision::Renew
        );
    }

    #[test]
    fn unheld_lease_is_acquired() {
        assert_eq!(decide(None, "me", None), Decision::Acquire);
    }

    #[test]
    fn fresh_peer_lease_makes_us_wait() {
        // Just observed, or renewed within the window → leave the peer alone.
        assert_eq!(decide(Some("peer"), "me", None), Decision::Wait);
        assert_eq!(
            decide(Some("peer"), "me", Some(Duration::from_secs(14))),
            Decision::Wait
        );
    }

    #[test]
    fn expired_peer_lease_is_acquired() {
        assert_eq!(
            decide(Some("peer"), "me", Some(LEASE_DURATION)),
            Decision::Acquire
        );
    }

    #[test]
    fn tracker_resets_when_renew_time_changes() {
        let mut tracker = Tracker::default();
        let start = Instant::now();
        let first = a_time(1);
        // First sighting of a value: no elapsed time yet.
        assert_eq!(tracker.observe(Some(first.clone()), start), None);
        // Same value later: reports how long it has stood still.
        assert_eq!(
            tracker.observe(Some(first), start + LEASE_DURATION),
            Some(LEASE_DURATION)
        );
        // A changed value resets the timer.
        assert_eq!(
            tracker.observe(Some(a_time(2)), start + LEASE_DURATION),
            None
        );
    }
}
