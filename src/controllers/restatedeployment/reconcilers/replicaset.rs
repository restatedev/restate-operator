use std::collections::{BTreeMap, HashMap};

use k8s_openapi::api::apps::v1::ReplicaSet;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta};

use kube::api::{
    Api, ApiResource, DynamicObject, PartialObjectMetaExt, Patch, PatchParams, PostParams,
};
use kube::core::subresource::Scale;
use kube::runtime::reflector::Store;
use kube::{Resource, ResourceExt};
use serde_json::json;
use tracing::*;
use url::Url;

use crate::controllers::restatedeployment::controller::{
    APP_MANAGED_BY_LABEL, OWNED_BY_LABEL, RESTATE_DEPLOYMENT_ID_ANNOTATION,
};
use crate::resources::restatedeployments::RestateDeployment;
use crate::{Error, Result};

pub const POD_TEMPLATE_HASH_LABEL: &str = "pod-template-hash";
pub const RESTATE_POD_TEMPLATE_ANNOTATION: &str = "restate.dev/pod-template";
pub const RESTATE_REMOVE_VERSION_AT_ANNOTATION: &str = "restate.dev/remove-version-at";

/// Ensure a ReplicaSet exists for the latest RestateDeployment version
pub async fn reconcile_replicaset(
    client: &kube::Client,
    rsd: &RestateDeployment,
    namespace: &str,
    versioned_name: &str,
    match_labels: BTreeMap<String, String>,
    annotations: BTreeMap<String, String>,
    hash: &str,
) -> Result<ReplicaSet> {
    // Add version and hash to pod template
    let mut template_metadata = rsd.spec.template.metadata.clone();
    let template_labels = template_metadata
        .get_or_insert_default()
        .labels
        .get_or_insert(BTreeMap::new());

    template_labels.insert(POD_TEMPLATE_HASH_LABEL.to_string(), hash.to_string());
    if let Some(cluster) = rsd.spec.restate.register.cluster.as_deref() {
        // so that the cluster is allowed to send traffic to these pods
        template_labels.insert(format!("allow.restate.dev/{cluster}"), "true".to_string());
    }

    // in native deployment controller, replicaset labels always match their template labels.
    let mut replicaset_labels = template_labels.clone();
    // but we want to add some extras to make it easier to find replicasets we own
    replicaset_labels.insert(OWNED_BY_LABEL.to_string(), rsd.name_any());
    replicaset_labels.insert(
        APP_MANAGED_BY_LABEL.to_string(),
        "restate-operator".to_owned(),
    );

    // Create replicaset ownership reference
    let owner_reference = rsd.controller_owner_ref(&()).unwrap();

    // Create the replicaset - the pod template should be passed through directly so we can't use the proper type
    let rs_resource = ApiResource::erase::<ReplicaSet>(&());
    let mut replicaset = DynamicObject::new(versioned_name, &rs_resource).within(namespace);
    replicaset.metadata.labels = Some(replicaset_labels);
    // annotations match the owning deployment
    replicaset.metadata.annotations = Some(annotations);
    replicaset.metadata.owner_references = Some(vec![owner_reference]);

    replicaset.data = json!({
        "spec": {
            "replicas": rsd.spec.replicas,
            "selector": LabelSelector {
                match_expressions: rsd.spec.selector.match_expressions.clone(),
                match_labels: Some(match_labels.clone()),
            },
            "template": {
                "metadata": template_metadata,
                "spec": rsd.spec.template.spec,
            },
            "minReadySeconds": rsd.spec.min_ready_seconds,
        }
    });

    let rs_api: Api<DynamicObject> = Api::namespaced_with(client.clone(), namespace, &rs_resource);
    let applied_rs: DynamicObject = rs_api
        .create(
            &PostParams {
                dry_run: false,
                field_manager: Some("restate-operator".to_owned()),
            },
            &replicaset,
        )
        .await?;
    let applied_rs: ReplicaSet = serde_json::from_value(serde_json::to_value(applied_rs)?)?;

    debug!("Created ReplicaSet {versioned_name} in namespace {namespace}");

    Ok(applied_rs)
}

pub fn pod_template_annotation(rs: &RestateDeployment) -> String {
    serde_json::to_string(&rs.spec.template).expect("PodTemplateSpec to serialize")
}

/// Generate a hash for a pod template to uniquely identify versions
pub fn generate_pod_template_hash(rsd: &RestateDeployment, pod_template: &str) -> String {
    use std::hash::Hasher;

    let mut hasher = fnv::FnvHasher::default();

    hasher.write(pod_template.as_bytes());

    // we set a pod label based on this field, so we have to incorporate it into the hash
    if let Some(cluster) = rsd.spec.restate.register.cluster.as_deref() {
        hasher.write(cluster.as_bytes());
    }

    // if you change the path, it creates a new deployment, which means we want a new replicaset too to keep things 1:1
    if let Some(service_path) = rsd.spec.restate.service_path.as_deref() {
        hasher.write(service_path.as_bytes());
    }

    // It's possible that changing this flag will create a new deployment id; by making it part of the replicaset name we guarantee that deployments and replicasets stay 1:1
    if let Some(true) = rsd.spec.restate.use_http11 {
        hasher.write(b"use_http11");
    }
    if let Some(collision_count) = rsd.status.as_ref().and_then(|s| s.collision_count) {
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
    let mut out = String::with_capacity(10);

    // this gets decimal digits in reverse, because it doesn't really matter.
    while val > 0 {
        let n = val % 10;
        val /= 10;
        out.push(NUMBER_MAP[n as usize]);
    }

    out
}

/// Delete ReplicaSets that are no longer needed
#[allow(clippy::too_many_arguments)]
pub async fn cleanup_old_replicasets(
    namespace: &str,
    rs_api: &Api<ReplicaSet>,
    replicasets_store: &Store<ReplicaSet>,
    http_client: &reqwest::Client,
    admin_endpoint: &Url,
    rsd_uid: &str,
    revision_history_limit: i32,
    deployments: &HashMap<String, bool>,
) -> Result<(i32, Option<chrono::DateTime<chrono::Utc>>)> {
    let replicasets_cell = std::cell::Cell::new(Vec::new());

    let _ = replicasets_store.find(|rs| {
        // replicasets in the same ns
        if rs.metadata.namespace.as_deref() != Some(namespace) {
            return false;
        }

        // replicasets owned by this restatedeployment (we make no attempt to handle orphaned ones if a rsd was deleted with --cascade=orphan and then recreated)
        if !rs.owner_references().iter().any(|reference| {
            reference.uid == rsd_uid && reference.kind == RestateDeployment::kind(&())
        }) {
            return false;
        };

        // for some reason find only takes a Fn, not FnMut.
        let mut replicasets = replicasets_cell.take();
        replicasets.push(rs.clone());
        replicasets_cell.set(replicasets);

        false
    });

    let mut replicasets = replicasets_cell.into_inner();

    // Sort replicasets by creation time (newest first)
    replicasets.sort_by(|a, b| {
        b.metadata
            .creation_timestamp
            .cmp(&a.metadata.creation_timestamp)
    });

    // keep track of how many rs there are that are still in-use by restate (active services or invocations)
    let mut active_count = 0;
    // Keep track of how many zero-scaled rs there are (for revision history limit)
    let mut historic_count = 0;
    let mut next_removal = None;

    let now = chrono::Utc::now();

    for rs in replicasets {
        let rs_name = rs.name_any();

        let rs_deployment_id = rs.annotations().get(RESTATE_DEPLOYMENT_ID_ANNOTATION);

        // Skip active deployments
        let deployment = rs_deployment_id
            .and_then(|rs_deployment_id| deployments.get(rs_deployment_id).cloned());
        let deployment_exists = deployment.is_some();
        let deployment_active = deployment.unwrap_or(false);

        if deployment_active {
            active_count += 1;

            if rs
                .annotations()
                .get(RESTATE_REMOVE_VERSION_AT_ANNOTATION)
                .is_none()
            {
                // not scheduled for removal; all good.
                continue;
            }

            debug!(
                "Unscheduling removal of active ReplicaSet {} in namespace {namespace}",
                rs_name,
            );

            // if we previously scheduled it for removal, but it now seems active, reset the timer by removing the annotation
            let params: PatchParams =
                PatchParams::apply("restate-operator/remove-version-at").force();
            rs_api
                .patch_metadata(
                    &rs_name,
                    &params,
                    &Patch::Apply(json!({
                        "apiVersion": ReplicaSet::api_version(&()),
                        "kind": ReplicaSet::kind(&()),
                        "metadata": {
                            "annotations": {
                                RESTATE_REMOVE_VERSION_AT_ANNOTATION: null,
                            }
                        }
                    })),
                )
                .await?;

            continue;
        }

        let current_remove_at = rs
            .annotations()
            .get(RESTATE_REMOVE_VERSION_AT_ANNOTATION)
            .and_then(|remove_at| {
                chrono::DateTime::parse_from_rfc3339(remove_at)
                    .map(|t| t.to_utc())
                    .ok()
            });

        let current_remove_at_in_past = current_remove_at.is_some_and(|c| c < now);

        match (
            current_remove_at,
            current_remove_at_in_past,
            deployment_exists,
        ) {
            (_, true, _) | (_, _, false) => {
                // we are past the remove at time, or the endpoint was removed by other means; can now scale it down

                // If this version has active pods, scale it down to 0 first
                if rs
                    .spec
                    .as_ref()
                    .and_then(|s| s.replicas.as_ref())
                    .is_some_and(|r| *r > 0)
                {
                    debug!(
                        "Scaling down old ReplicaSet {} to 0 replicas in namespace {namespace}",
                        rs_name,
                    );

                    let params: PatchParams =
                        PatchParams::apply("restate-operator/scale-down").force();
                    rs_api
                        .patch_scale(
                            &rs_name,
                            &params,
                            &Patch::Apply(serde_json::json!({
                                "apiVersion": Scale::api_version(&()),
                                "kind": Scale::kind(&()),
                                "spec": { "replicas": 0 }
                            })),
                        )
                        .await?;
                }

                // If we are here, there is a 0 sized replicaset which should be subject to the history limit
                if historic_count < revision_history_limit {
                    historic_count += 1;
                    // we haven't hit that limit yet, so we don't need to delete this rs
                    continue;
                }

                if deployment_exists {
                    let rs_deployment_id = rs_deployment_id.unwrap();

                    debug!("Force-deleting Restate deployment {rs_deployment_id} as its associated with old ReplicaSet {rs_name} in namespace {namespace}");

                    let resp = http_client
                        .delete(format!(
                            "{admin_endpoint}/deployments/{rs_deployment_id}?force=true"
                        ))
                        .send()
                        .await
                        .map_err(Error::AdminCallFailed)?;

                    // for idempotency we have to allow 404
                    if resp.status() != reqwest::StatusCode::NOT_FOUND {
                        let _ = resp.error_for_status().map_err(Error::AdminCallFailed)?;
                    }
                }

                debug!("Deleting old ReplicaSet {rs_name} in namespace {namespace}");
                rs_api.delete(&rs_name, &Default::default()).await?;

                continue;
            }
            (Some(remove_at), false, true) => {
                // endpoint exists and remove at time is in the future, ensure we keep track of the soonest such time
                next_removal = match next_removal {
                    None => Some(remove_at),
                    Some(next_removal) if next_removal > remove_at => Some(remove_at),
                    els => els,
                };

                continue;
            }
            (None, _, true) => {
                // endpoint exists and there's no valid remove_version_at annotation, create one
                debug!(
                    "Scheduling removal (after drain delay) of old ReplicaSet {} in namespace {namespace}",
                    rs_name,
                );

                let remove_at = chrono::Utc::now()
                    .checked_add_signed(chrono::TimeDelta::minutes(5)) // todo configurable?
                    .expect("remove_version_at in bounds");

                let params = PatchParams::apply("restate-operator/remove-version-at").force();
                let patch = ObjectMeta {
                    annotations: Some(
                        [(
                            RESTATE_REMOVE_VERSION_AT_ANNOTATION.to_string(),
                            remove_at.to_rfc3339(),
                        )]
                        .into(),
                    ),
                    ..Default::default()
                }
                .into_request_partial::<ReplicaSet>();

                rs_api
                    .patch_metadata(&rs_name, &params, &Patch::Apply(patch))
                    .await?;

                // ensure we keep track of the soonest remove_at
                next_removal = match next_removal {
                    None => Some(remove_at),
                    Some(next_removal) if next_removal > remove_at => Some(remove_at),
                    els => els,
                };

                continue;
            }
        }
    }

    Ok((active_count, next_removal))
}
