// SPDX-FileCopyrightText: Copyright (c) 2026 Mirantis, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Sandbox volume plumbing.
//!
//! Pure helpers that translate the CR's [`SandboxVolume`]s into the two things
//! the controller needs: the `PersistentVolumeClaim`s to provision (owned by
//! the sandbox, so they survive gateway-side recreation) and the gateway's
//! Kubernetes-driver `driver_config` that mounts them into the sandbox pod.
//!
//! No Kubernetes or gateway I/O lives here — the reconciler applies the objects
//! this module builds.

use std::collections::{BTreeMap, BTreeSet};

use k8s_openapi::api::core::v1::PersistentVolumeClaim;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use serde_json::{Value, json};

use crate::crd::SandboxVolume;
use crate::error::{Error, Result};

/// Standard `managed-by` label stamped on every provisioned PVC.
pub const MANAGED_BY_LABEL: &str = "app.kubernetes.io/managed-by";
/// Value for [`MANAGED_BY_LABEL`].
pub const MANAGED_BY_VALUE: &str = "openshell-operator";
/// Label linking a provisioned PVC back to its owning sandbox (by name).
pub const SANDBOX_LABEL: &str = "openshell.lenshq.io/sandbox";

/// Compute-driver key the gateway matches when selecting the `driver_config`
/// block. This feature is inherently Kubernetes-specific (it provisions PVCs),
/// so the block is always keyed for the Kubernetes driver.
const DRIVER_KEY: &str = "kubernetes";

/// Reject volumes the sandbox cannot honor before any object is created.
///
/// Names must be present and unique, mount paths absolute, and `volumeMode`
/// must not be `Block` — the sandbox mounts a filesystem, not a raw device.
pub fn validate(volumes: &[SandboxVolume]) -> Result<()> {
    let mut seen = BTreeSet::new();
    for volume in volumes {
        if volume.name.trim().is_empty() {
            return Err(Error::VolumeInvalid(
                "volume name must not be empty".to_owned(),
            ));
        }
        if !seen.insert(volume.name.as_str()) {
            return Err(Error::VolumeInvalid(format!(
                "duplicate volume name {:?}",
                volume.name
            )));
        }
        if !volume.mount_path.starts_with('/') {
            return Err(Error::VolumeInvalid(format!(
                "volume {:?} mountPath must be absolute, got {:?}",
                volume.name, volume.mount_path
            )));
        }
        if volume.claim.volume_mode.as_deref() == Some("Block") {
            return Err(Error::VolumeInvalid(format!(
                "volume {:?} uses volumeMode Block; the sandbox mounts a filesystem",
                volume.name
            )));
        }
    }
    Ok(())
}

/// Deterministic PVC name for a sandbox volume. Stable across delete+recreate
/// of the sandbox (it derives only from names), which is what lets a recreated
/// sandbox reattach the same data.
#[must_use]
pub fn pvc_name(sandbox: &str, volume: &SandboxVolume) -> String {
    format!("{sandbox}-{}", volume.name)
}

/// Label selector matching every PVC provisioned for `sandbox`.
#[must_use]
pub fn selector(sandbox: &str) -> String {
    format!("{SANDBOX_LABEL}={sandbox}")
}

/// Build the `PersistentVolumeClaim` to provision for a volume.
///
/// The claim carries no owner reference: lifecycle is managed explicitly by the
/// finalizer per the sandbox's `volumeRetention`, so it survives the resource
/// by default. Association is tracked via [`SANDBOX_LABEL`].
#[must_use]
pub fn build_pvc(sandbox: &str, volume: &SandboxVolume) -> PersistentVolumeClaim {
    let labels = BTreeMap::from([
        (MANAGED_BY_LABEL.to_owned(), MANAGED_BY_VALUE.to_owned()),
        (SANDBOX_LABEL.to_owned(), sandbox.to_owned()),
    ]);
    PersistentVolumeClaim {
        metadata: ObjectMeta {
            name: Some(pvc_name(sandbox, volume)),
            labels: Some(labels),
            ..ObjectMeta::default()
        },
        spec: Some(volume.claim.clone()),
        ..PersistentVolumeClaim::default()
    }
}

/// Build the Kubernetes-driver `driver_config` envelope mounting `volumes` into
/// the sandbox, or `None` when there are no volumes.
///
/// The shape mirrors the gateway's `KubernetesSandboxDriverConfig`: a `volumes`
/// list referencing the provisioned PVCs by name, and matching `volume_mounts`
/// on the agent container. The whole block is keyed by [`DRIVER_KEY`] because
/// the gateway forwards only the block matching the active compute driver.
#[must_use]
pub fn driver_config_json(sandbox: &str, volumes: &[SandboxVolume]) -> Option<Value> {
    if volumes.is_empty() {
        return None;
    }

    let volume_entries: Vec<Value> = volumes
        .iter()
        .map(|volume| {
            json!({
                "name": volume.name,
                "persistent_volume_claim": {
                    "claim_name": pvc_name(sandbox, volume),
                    "read_only": volume.read_only,
                },
            })
        })
        .collect();

    let mount_entries: Vec<Value> = volumes
        .iter()
        .map(|volume| {
            let mut mount = serde_json::Map::new();
            mount.insert("name".to_owned(), json!(volume.name));
            mount.insert("mount_path".to_owned(), json!(volume.mount_path));
            mount.insert("read_only".to_owned(), json!(volume.read_only));
            if let Some(sub_path) = &volume.sub_path {
                mount.insert("sub_path".to_owned(), json!(sub_path));
            }
            Value::Object(mount)
        })
        .collect();

    Some(json!({
        DRIVER_KEY: {
            "volumes": volume_entries,
            "containers": { "agent": { "volume_mounts": mount_entries } },
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::{
        MANAGED_BY_LABEL, SANDBOX_LABEL, build_pvc, driver_config_json, pvc_name, selector,
        validate,
    };
    use crate::crd::SandboxVolume;
    use k8s_openapi::api::core::v1::PersistentVolumeClaimSpec;

    fn volume(name: &str, mount_path: &str) -> SandboxVolume {
        SandboxVolume {
            name: name.to_owned(),
            mount_path: mount_path.to_owned(),
            sub_path: None,
            read_only: false,
            claim: PersistentVolumeClaimSpec::default(),
        }
    }

    #[test]
    fn validate_accepts_well_formed_volumes() {
        let volumes = vec![volume("data", "/data"), volume("cache", "/cache")];
        assert!(validate(&volumes).is_ok());
    }

    #[test]
    fn validate_rejects_empty_name() {
        assert!(validate(&[volume("", "/data")]).is_err());
    }

    #[test]
    fn validate_rejects_duplicate_names() {
        let volumes = vec![volume("data", "/data"), volume("data", "/other")];
        assert!(validate(&volumes).is_err());
    }

    #[test]
    fn validate_rejects_relative_mount_path() {
        assert!(validate(&[volume("data", "data")]).is_err());
    }

    #[test]
    fn validate_rejects_block_volume_mode() {
        let mut vol = volume("data", "/data");
        vol.claim.volume_mode = Some("Block".to_owned());
        assert!(validate(&[vol]).is_err());
    }

    #[test]
    fn pvc_name_is_deterministic() {
        assert_eq!(pvc_name("box", &volume("data", "/data")), "box-data");
    }

    #[test]
    fn selector_matches_sandbox_label() {
        assert_eq!(selector("box"), format!("{SANDBOX_LABEL}=box"));
    }

    #[test]
    fn build_pvc_sets_name_labels_and_spec() {
        let mut vol = volume("data", "/data");
        vol.claim.storage_class_name = Some("fast".to_owned());
        let pvc = build_pvc("box", &vol);

        assert_eq!(pvc.metadata.name.as_deref(), Some("box-data"));
        let labels = pvc.metadata.labels.expect("labels present");
        assert_eq!(
            labels.get(MANAGED_BY_LABEL).map(String::as_str),
            Some("openshell-operator")
        );
        assert_eq!(labels.get(SANDBOX_LABEL).map(String::as_str), Some("box"));
        assert_eq!(
            pvc.spec.and_then(|spec| spec.storage_class_name).as_deref(),
            Some("fast")
        );
    }

    #[test]
    fn driver_config_is_none_without_volumes() {
        assert!(driver_config_json("box", &[]).is_none());
    }

    #[test]
    fn driver_config_maps_volumes_and_mounts() {
        let mut vol = volume("data", "/sandbox");
        vol.read_only = true;
        vol.sub_path = Some("workspace".to_owned());
        let config = driver_config_json("box", &[vol]).expect("config present");

        let k8s = &config["kubernetes"];
        let pvc = &k8s["volumes"][0]["persistent_volume_claim"];
        assert_eq!(pvc["claim_name"], "box-data");
        assert_eq!(pvc["read_only"], true);

        let mount = &k8s["containers"]["agent"]["volume_mounts"][0];
        assert_eq!(mount["name"], "data");
        assert_eq!(mount["mount_path"], "/sandbox");
        assert_eq!(mount["sub_path"], "workspace");
        assert_eq!(mount["read_only"], true);
    }

    #[test]
    fn driver_config_omits_absent_sub_path() {
        let config = driver_config_json("box", &[volume("data", "/data")]).expect("config present");
        let mount = &config["kubernetes"]["containers"]["agent"]["volume_mounts"][0];
        assert!(mount.get("sub_path").is_none());
    }
}
