use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use k8s_openapi::api::apps::v1::{ReplicaSet, ReplicaSetSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta};

use kube::api::{Api, Patch, PatchParams, PostParams};
use kube::{Resource, ResourceExt};
use tracing::*;

use crate::resources::restatedeployments::RestateDeployment;
use crate::Result;

// Label keys used to track and identify resources
const APP_INSTANCE_LABEL: &str = "app.kubernetes.io/instance";
const APP_NAME_LABEL: &str = "app.kubernetes.io/name";
const APP_MANAGED_BY_LABEL: &str = "app.kubernetes.io/managed-by";
pub const POD_TEMPLATE_HASH_LABEL: &str = "pod-template-hash";

pub const RESTATE_POD_TEMPLATE_ANNOTATION: &str = "restate.dev/pod-template";
pub const RESTATE_REMOVE_VERSION_AT_ANNOTATION: &str = "restate.dev/remove-version-at";

/// Ensure a ReplicaSet exists for the latest RestateDeployment version
pub async fn reconcile_replicaset(
    rs_api: &Api<ReplicaSet>,
    rsd: &RestateDeployment,
    namespace: &str,
    versioned_name: &str,
    match_labels: BTreeMap<String, String>,
    hash: &str,
    pod_template_annotation: &str,
) -> Result<ReplicaSet> {
    // Create replicaset labels
    let mut labels = BTreeMap::new();
    labels.insert(APP_INSTANCE_LABEL.to_string(), rsd.name_any());
    labels.insert(APP_NAME_LABEL.to_string(), "restate-service".to_string());
    labels.insert(
        APP_MANAGED_BY_LABEL.to_string(),
        "restate-operator".to_string(),
    );
    labels.insert(POD_TEMPLATE_HASH_LABEL.to_string(), hash.to_string());

    let mut annotations = BTreeMap::new();
    annotations.insert(
        RESTATE_POD_TEMPLATE_ANNOTATION.to_string(),
        pod_template_annotation.to_string(),
    );

    // Add version and hash to pod template
    let mut template = rsd.spec.template.clone();
    let template_labels = template
        .metadata
        .get_or_insert(ObjectMeta::default())
        .labels
        .get_or_insert(BTreeMap::new());

    template_labels.insert(POD_TEMPLATE_HASH_LABEL.to_string(), hash.to_string());

    // Create replicaset specification
    let replicaset_spec = ReplicaSetSpec {
        replicas: rsd.spec.replicas,
        selector: LabelSelector {
            match_expressions: rsd.spec.selector.match_expressions.clone(),
            match_labels: Some(match_labels.clone()),
        },
        template: Some(template),
        min_ready_seconds: rsd.spec.min_ready_seconds,
    };

    // Create replicaset ownership reference
    let owner_reference = rsd.controller_owner_ref(&()).unwrap();

    // Create replicaset metadata
    let metadata = ObjectMeta {
        name: Some(versioned_name.to_owned()),
        namespace: Some(namespace.to_owned()),
        labels: Some(labels),
        annotations: Some(annotations),
        owner_references: Some(vec![owner_reference]),
        ..Default::default()
    };

    // Create the replicaset object
    let replicaset = ReplicaSet {
        metadata,
        spec: Some(replicaset_spec),
        status: None,
    };

    // Create the replicaset
    let applied_rs = rs_api
        .create(
            &PostParams {
                dry_run: false,
                field_manager: Some("restate-operator".to_owned()),
            },
            &replicaset,
        )
        .await?;
    debug!("Created ReplicaSet {versioned_name} in namespace {namespace}");

    Ok(applied_rs)
}

pub fn pod_template_annotation(rs: &RestateDeployment) -> String {
    serde_json::to_string(&rs.spec.template).expect("PodTemplateSpec to serialize")
}

/// Generate a hash for a pod template to uniquely identify versions
pub fn generate_pod_template_hash(pod_template: &str, collision_count: Option<i32>) -> String {
    use std::hash::Hasher;

    let mut hasher = fnv::FnvHasher::default();

    hasher.write(pod_template.as_bytes());

    if let Some(collision_count) = collision_count {
        hasher.write(&collision_count.to_be_bytes());
    }

    let hash_bytes = hasher.finish().to_be_bytes();

    let mut first_4: [u8; 4] = [0; 4];
    first_4.clone_from_slice(&hash_bytes[..4]);

    safe_encode_u32(u32::from_be_bytes(first_4))
}

// For some reason kubernetes uses this really weird encoding where decimal digits of a u32 are swapped partially out for letters.
// I suspect this is an early bug that they can't fix now. We match it so things 'look' right.
fn safe_encode_u32(mut val: u32) -> String {
    const NUMBER_MAP: &[char] = &['4', '5', '6', '7', '8', '9', 'b', 'c', 'd', 'f'];

    // 10^10 - 1 > 2^32 - 1
    let mut buf: Vec<char> = vec!['\0'; 10];
    let mut curr = buf.len();
    // read out base 10 values and look them up in the map
    loop {
        let n = val % 10;
        val = val / 10;
        curr -= 1;
        buf[curr] = NUMBER_MAP[n as usize];
        if val == 0 {
            break;
        };
    }

    String::from_iter(buf[curr..].into_iter())
}

/// Delete ReplicaSets that are no longer needed
pub async fn cleanup_old_replicasets(
    namespace: &str,
    rs_api: &Api<ReplicaSet>,
    rsd: &RestateDeployment,
    active_endpoints: &HashSet<String>,
) -> Result<(i32, Option<Duration>)> {
    // List replicasets with the instance selector
    let label_selector = format!("{}={}", APP_INSTANCE_LABEL, rsd.name_any());
    let list_params = kube::api::ListParams::default().labels(&label_selector);
    let replicaset_list = rs_api.list(&list_params).await?;

    // Get revision history limit
    let revision_history_limit = rsd.spec.revision_history_limit.unwrap_or(10);

    // Sort replicasets by creation time (newest first)
    let mut sorted_replicasets = replicaset_list.items;
    sorted_replicasets.sort_by(|a, b| {
        b.metadata
            .creation_timestamp
            .cmp(&a.metadata.creation_timestamp)
    });

    // keep track of how many rs there are that are still in-use by restate (active services or invocations)
    let mut active_count = 0;
    // Keep track of how many zero-scaled rs there are (for revision history limit)
    let mut historic_count = 0;
    let mut next_removal = None;

    for mut rs in sorted_replicasets {
        let rs_name = rs.name_any();
        let service_endpoint = format!("http://{}.{}.svc.cluster.local:9080/", rs_name, namespace);

        // Skip active versions
        if active_endpoints.contains(&service_endpoint) {
            if rs
                .metadata
                .annotations
                .get_or_insert_default()
                .get(RESTATE_REMOVE_VERSION_AT_ANNOTATION)
                .is_some()
            {
                debug!(
                    "Unscheduling removal of active ReplicaSet {} in namespace {namespace}",
                    rs_name,
                );

                // if we previously scheduled it for removal, but it now seems active, reset the timer.
                let patch = serde_json::json!({
                    "apiVersion": ReplicaSet::api_version(&()),
                    "kind": ReplicaSet::kind(&()),
                    "metadata": {
                        "annotations": {
                            RESTATE_REMOVE_VERSION_AT_ANNOTATION: null,
                        }
                    },
                });

                let params: PatchParams = PatchParams::apply("restate-operator");
                rs_api
                    .patch(&rs.name_any(), &params, &Patch::Merge(patch))
                    .await?;
            }

            active_count += 1;
            continue;
        }

        match rs
            .metadata
            .annotations
            .get_or_insert_default()
            .get(RESTATE_REMOVE_VERSION_AT_ANNOTATION)
            .and_then(|remove_at| {
                chrono::DateTime::parse_from_rfc3339(remove_at)
                    .map(|t| t.to_utc())
                    .ok()
            }) {
            Some(remove_at) => {
                let seconds_until_remove = (remove_at - chrono::Utc::now()).num_seconds();

                if seconds_until_remove < 0 {
                    // we are past the remove at time

                    // If this version has active pods, scale it down to 0 first
                    if let Some(replicas) = rs
                        .spec
                        .as_mut()
                        .and_then(|s| s.replicas.as_mut())
                        .filter(|r| **r > 0)
                    {
                        debug!(
                            "Scaling down old ReplicaSet {} to 0 replicas in namespace {namespace}",
                            rs_name,
                        );

                        *replicas = 0;

                        let params: PatchParams = PatchParams::apply("restate-operator");
                        rs_api
                            .patch_scale(
                                &rs_name,
                                &params,
                                &Patch::Merge(serde_json::json!({"spec": { "replicas": 0 }})),
                            )
                            .await?;
                    }

                    // If we are here, there is a 0 sized replicaset which should be subject to the history limit
                    if historic_count < revision_history_limit {
                        historic_count += 1;
                        // we haven't hit that limit yet, so we don't need to delete this rs
                        continue;
                    }

                    debug!("Deleting old ReplicaSet {rs_name} in namespace {namespace}");
                    rs_api.delete(&rs_name, &Default::default()).await?;

                    continue;
                } else {
                    // remove at time is in the future, ensure we keep track of the soonest such time
                    let seconds_until_remove = Duration::from_secs(seconds_until_remove as u64);
                    next_removal = match next_removal {
                        None => Some(seconds_until_remove),
                        Some(next_removal) if next_removal > seconds_until_remove => {
                            Some(seconds_until_remove)
                        }
                        els => els,
                    };

                    continue;
                }
            }
            None => {
                // no valid remove_version_at annotation, create one
                debug!(
                    "Scheduling removal (after drain delay) of old ReplicaSet {} in namespace {namespace}",
                    rs_name,
                );

                let patch = serde_json::json!({
                    "apiVersion": ReplicaSet::api_version(&()),
                    "kind": ReplicaSet::kind(&()),
                    "metadata": {
                        "annotations": {
                            RESTATE_REMOVE_VERSION_AT_ANNOTATION: chrono::Utc::now()
                                .checked_add_signed(chrono::TimeDelta::minutes(5)) // todo configurable?
                                .expect("remove_version_at in bounds")
                                .to_rfc3339()
                        }
                    },
                });

                let params: PatchParams = PatchParams::apply("restate-operator");
                rs_api
                    .patch(&rs.name_any(), &params, &Patch::Merge(patch))
                    .await?;

                continue;
            }
        }
    }

    Ok((active_count, next_removal))
}
