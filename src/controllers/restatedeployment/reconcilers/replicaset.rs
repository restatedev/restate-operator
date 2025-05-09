use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use k8s_openapi::api::apps::v1::{ReplicaSet, ReplicaSetSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta};

use kube::api::{Api, Patch, PatchParams};
use kube::{Resource, ResourceExt};
use tracing::*;

use crate::resources::restatedeployments::RestateDeployment;
use crate::{Error, Result};

// Label keys used to track and identify resources
const APP_INSTANCE_LABEL: &str = "app.kubernetes.io/instance";
const APP_NAME_LABEL: &str = "app.kubernetes.io/name";
const APP_MANAGED_BY_LABEL: &str = "app.kubernetes.io/managed-by";
const POD_TEMPLATE_HASH_LABEL: &str = "pod-template-hash";

pub const RESTATE_REMOVE_VERSION_AT_ANNOTATION: &str = "restate.dev/remove-version-at";

/// Ensure a ReplicaSet exists for the latest RestateDeployment version
pub async fn reconcile_replicaset(
    rs_api: &Api<ReplicaSet>,
    rs: &RestateDeployment,
    namespace: &str,
    versioned_name: &str,
    hash: &str,
) -> Result<(ReplicaSet, BTreeMap<String, String>)> {
    // Create replicaset labels
    let mut labels = BTreeMap::new();
    labels.insert(APP_INSTANCE_LABEL.to_string(), rs.name_any());
    labels.insert(APP_NAME_LABEL.to_string(), "restate-service".to_string());
    labels.insert(
        APP_MANAGED_BY_LABEL.to_string(),
        "restate-operator".to_string(),
    );
    labels.insert(POD_TEMPLATE_HASH_LABEL.to_string(), hash.to_string());

    // Add version and hash to pod template
    let mut template = rs.spec.template.clone();
    let template_labels = template
        .metadata
        .get_or_insert(ObjectMeta::default())
        .labels
        .get_or_insert(BTreeMap::new());

    template_labels.insert(POD_TEMPLATE_HASH_LABEL.to_string(), hash.to_string());

    let match_labels = match &rs.spec.selector.match_labels {
        None => BTreeMap::from([(POD_TEMPLATE_HASH_LABEL.to_owned(), hash.to_owned())]),
        Some(match_labels) => {
            let mut match_labels = match_labels.clone();
            match_labels.insert(POD_TEMPLATE_HASH_LABEL.to_owned(), hash.to_owned());
            match_labels
        }
    };

    // Create replicaset specification
    let replicaset_spec = ReplicaSetSpec {
        replicas: rs.spec.replicas,
        selector: LabelSelector {
            match_expressions: rs.spec.selector.match_expressions.clone(),
            match_labels: Some(match_labels.clone()),
        },
        template: Some(template),
        min_ready_seconds: rs.spec.min_ready_seconds,
    };

    // Create replicaset ownership reference
    let owner_reference = rs.controller_owner_ref(&()).unwrap();

    // Create replicaset metadata
    let metadata = ObjectMeta {
        name: Some(versioned_name.to_owned()),
        namespace: Some(namespace.to_owned()),
        labels: Some(labels),
        owner_references: Some(vec![owner_reference]),
        ..Default::default()
    };

    // Create the replicaset object
    let replicaset = ReplicaSet {
        metadata,
        spec: Some(replicaset_spec),
        status: None,
    };

    // Apply the replicaset - create or update as needed
    apply_replicaset(namespace, rs_api, &replicaset).await?;

    Ok((replicaset, match_labels))
}

async fn apply_replicaset(
    namespace: &str,
    rs_api: &Api<ReplicaSet>,
    rs: &ReplicaSet,
) -> Result<(), Error> {
    let name = rs.metadata.name.as_ref().unwrap();
    let params: PatchParams = PatchParams::apply("restate-operator").force();
    debug!("Applying ReplicaSet {} in namespace {}", name, namespace);
    rs_api.patch(name, &params, &Patch::Apply(rs)).await?;
    Ok(())
}

/// Generate a hash for a pod template to uniquely identify versions
pub fn generate_pod_template_hash(rs: &RestateDeployment) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();

    // Hash the pod template spec
    let template_spec = &rs.spec.template.spec;
    if let Some(spec) = template_spec {
        // Hash specific fields of the pod spec that identify its functionality
        for container in &spec.containers {
            container.image.hash(&mut hasher);
            if let Some(command) = &container.command {
                command.hash(&mut hasher);
            }
            if let Some(args) = &container.args {
                args.hash(&mut hasher);
            }
            if let Some(env) = &container.env {
                for env_var in env {
                    env_var.name.hash(&mut hasher);
                    if let Some(value) = &env_var.value {
                        value.hash(&mut hasher);
                    }
                }
            }
        }
    }

    // Hash generation to ensure each update gets a different hash
    if let Some(generation) = rs.metadata.generation {
        generation.hash(&mut hasher);
    }

    // Return the hash as a short hex string
    format!("{:x}", hasher.finish())[..8].to_string()
}

/// Delete ReplicaSets that are no longer needed
pub async fn cleanup_old_replicasets(
    namespace: &str,
    rs_api: &Api<ReplicaSet>,
    rs: &RestateDeployment,
    active_endpoints: &HashSet<String>,
) -> Result<(i32, Option<Duration>)> {
    // List replicasets with the instance selector
    let label_selector = format!("{}={}", APP_INSTANCE_LABEL, rs.name_any());
    let list_params = kube::api::ListParams::default().labels(&label_selector);
    let replicaset_list = rs_api.list(&list_params).await?;

    // Get revision history limit
    let revision_history_limit = rs.spec.revision_history_limit.unwrap_or(10);

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
        let service_endpoint = format!("http://{}.{}.svc.cluster.local:9080", rs_name, namespace);

        // Skip active versions
        if active_endpoints.contains(&service_endpoint) {
            active_count += 1;
            continue;
        }

        // Skip zero-replica versions
        if rs.spec.as_ref().and_then(|spec| spec.replicas).unwrap_or(0) == 0 {
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
                        info!(
                            "Scaling down old ReplicaSet {} to 0 replicas in namespace {namespace}",
                            rs_name,
                        );

                        *replicas = 0;

                        apply_replicaset(namespace, rs_api, &rs).await?;
                    }

                    // If we are here, there is a 0 sized replicaset which should be subject to the history limit
                    if historic_count < revision_history_limit {
                        historic_count += 1;
                        // we haven't hit that limit yet, so we don't need to delete this rs
                        continue;
                    }

                    info!("Deleting old ReplicaSet {rs_name} in namespace {namespace}");
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
                // no valid remove version at annotation
                rs.annotations_mut().insert(
                    RESTATE_REMOVE_VERSION_AT_ANNOTATION.to_owned(),
                    chrono::Utc::now()
                        .checked_add_signed(chrono::TimeDelta::minutes(5)) // todo configurable?
                        .expect("remove_version_at in bounds")
                        .to_rfc3339(),
                );
                apply_replicaset(namespace, rs_api, &rs).await?;

                continue;
            }
        }
    }

    Ok((active_count, next_removal))
}
