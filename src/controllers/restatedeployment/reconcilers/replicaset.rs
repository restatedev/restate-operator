use std::collections::BTreeMap;

use k8s_openapi::api::apps::v1::ReplicaSet;
use k8s_openapi::api::autoscaling::v2::HorizontalPodAutoscaler;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector;

use kube::api::{Api, ApiResource, DynamicObject, Patch, PatchParams, PostParams};
use kube::core::subresource::Scale;
use kube::runtime::events::{Event, EventType};
use kube::runtime::reflector::ObjectRef;
use kube::{Resource, ResourceExt};
use reqwest::Method;
use serde_json::json;
use tracing::*;

use crate::controllers::restatedeployment::cleanup::{
    BlockingVersion, CleanupMode, CleanupOutcome, DeploymentUsageMap,
    RESTATE_REMOVE_VERSION_AT_ANNOTATION, VersionAction, VersionFacts, planned_pass,
    schedule_version_removal, unschedule_version_removal,
};
use crate::controllers::restatedeployment::controller::{
    APP_MANAGED_BY_LABEL, Context, OWNED_BY_LABEL, RESTATE_DEPLOYMENT_ID_ANNOTATION,
};
use crate::resources::restatecloudenvironments::InProcessTunnelParams;
use crate::resources::restatedeployments::RestateDeployment;
use crate::{Error, Result};

use super::autoscaling::HpaPlan;

pub const POD_TEMPLATE_HASH_LABEL: &str = "pod-template-hash";
pub const RESTATE_POD_TEMPLATE_ANNOTATION: &str = "restate.dev/pod-template";
/// Records the tunnel name a `tunnelMode: in-process` ReplicaSet was created with —
/// the same value injected into its pods as RESTATE_INPROC_TUNNEL_NAME.
pub const RESTATE_TUNNEL_NAME_ANNOTATION: &str = "restate.dev/tunnel-name";

// The environment variables in-process tunnel clients (e.g.
// @restatedev/restate-sdk-tunnel) resolve their configuration from.
pub const INPROC_TUNNEL_NAME_ENV: &str = "RESTATE_INPROC_TUNNEL_NAME";
pub const INPROC_ENVIRONMENT_ID_ENV: &str = "RESTATE_INPROC_ENVIRONMENT_ID";
pub const INPROC_CLOUD_REGION_ENV: &str = "RESTATE_INPROC_CLOUD_REGION";
pub const INPROC_SIGNING_PUBLIC_KEY_ENV: &str = "RESTATE_INPROC_SIGNING_PUBLIC_KEY";

/// Ensure a ReplicaSet exists for the latest RestateDeployment version
#[allow(clippy::too_many_arguments)]
pub async fn reconcile_replicaset(
    client: &kube::Client,
    rsd: &RestateDeployment,
    namespace: &str,
    versioned_name: &str,
    match_labels: BTreeMap<String, String>,
    annotations: BTreeMap<String, String>,
    hash: &str,
    in_process_tunnel: Option<&InProcessTunnelParams>,
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

    // Like the pod-template-hash label, the RESTATE_INPROC_* env vars are injected after
    // hashing — but every injected value derives from hash inputs (the versioned name and
    // the RestateCloudEnvironment values), so a change in any of them mints a new revision.
    let template_spec = match (&rsd.spec.template.spec, in_process_tunnel) {
        (Some(spec), Some(params)) => Some(inject_in_process_tunnel_env(
            spec.clone(),
            versioned_name,
            params,
        )?),
        (spec, _) => spec.clone(),
    };

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
                match_expressions: rsd.spec.selector.as_ref().and_then(|s| s.match_expressions.clone()),
                match_labels: Some(match_labels.clone()),
            },
            "template": {
                "metadata": template_metadata,
                "spec": template_spec,
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

/// Inject the RESTATE_INPROC_* environment variables that in-process tunnel clients
/// resolve their configuration from, into every container of the pod template —
/// including initContainers, so native sidecars (restartPolicy: Always) are covered.
/// A container that already declares one of them is an error: silently keeping the
/// user value would desync the pods from the URL the operator registers.
fn inject_in_process_tunnel_env(
    mut spec: serde_json::Value,
    tunnel_name: &str,
    params: &InProcessTunnelParams,
) -> Result<serde_json::Value> {
    let vars = [
        (INPROC_TUNNEL_NAME_ENV, tunnel_name),
        (INPROC_ENVIRONMENT_ID_ENV, params.environment_id.as_str()),
        (INPROC_CLOUD_REGION_ENV, params.region.as_str()),
        (
            INPROC_SIGNING_PUBLIC_KEY_ENV,
            params.signing_public_key.as_str(),
        ),
    ];

    // a pod spec without containers is invalid; let the api server report that
    for field in ["containers", "initContainers"] {
        let Some(containers) = spec.get_mut(field).and_then(|c| c.as_array_mut()) else {
            continue;
        };

        for container in containers.iter_mut() {
            let Some(container) = container.as_object_mut() else {
                continue;
            };
            let container_name = container
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("<unnamed>")
                .to_owned();

            let env = container
                .entry("env")
                .or_insert_with(|| serde_json::Value::Array(Vec::new()));
            let Some(env) = env.as_array_mut() else {
                return Err(Error::InvalidRestateConfig(format!(
                    "container '{container_name}' has a non-array `env` field"
                )));
            };

            for (var, value) in vars {
                if env
                    .iter()
                    .any(|entry| entry.get("name").and_then(|n| n.as_str()) == Some(var))
                {
                    return Err(Error::InvalidRestateConfig(format!(
                        "tunnelMode: in-process injects the environment variable `{var}`, but container '{container_name}' already declares it; remove it from the pod template"
                    )));
                }
                env.push(json!({"name": var, "value": value}));
            }
        }
    }

    Ok(spec)
}

pub fn pod_template_annotation(rs: &RestateDeployment) -> String {
    serde_json::to_string(&rs.spec.template).expect("PodTemplateSpec to serialize")
}

/// Generate a hash for a pod template to uniquely identify versions
pub fn generate_pod_template_hash(
    rsd: &RestateDeployment,
    pod_template: &str,
    in_process_tunnel: Option<&InProcessTunnelParams>,
) -> String {
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

    // In-process tunnel mode injects these values into the pods and bakes the versioned
    // name into the registered URL, so changing any of them must mint a new revision.
    // Folded in only when the mode is set, so existing deployments keep their hashes.
    if let Some(params) = in_process_tunnel {
        hasher.write(b"tunnel_mode=in-process");
        for value in [
            &params.environment_id,
            &params.region,
            &params.signing_public_key,
        ] {
            // length-prefixed so distinct tuples can't concatenate identically
            hasher.write(&(value.len() as u64).to_be_bytes());
            hasher.write(value.as_bytes());
        }
        // Folded in only when set, so revisions on the default URL keep their hashes.
        if let Some(proxy_base_url) = params.proxy_base_url.as_deref() {
            hasher.write(b"proxy_base_url=");
            hasher.write(&(proxy_base_url.len() as u64).to_be_bytes());
            hasher.write(proxy_base_url.as_bytes());
        }
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
pub async fn cleanup_old_replicasets<F, Fut>(
    namespace: &str,
    ctx: &Context,
    rs_api: &Api<ReplicaSet>,
    rsd_uid: &str,
    rsd: &RestateDeployment,
    mode: CleanupMode,
    deployments: &DeploymentUsageMap,
    except_rs: Option<&str>,
    confirm: F,
) -> Result<CleanupOutcome>
where
    F: FnOnce() -> Fut + Send,
    Fut: std::future::Future<Output = Result<DeploymentUsageMap>> + Send,
{
    let replicasets_cell = std::cell::Cell::new(Vec::new());

    let _ = ctx.replicasets_store.find(|rs| {
        let rs_namespace = match &rs.metadata.namespace.as_deref() {
            Some("") | None => "default",
            Some(ns) => ns,
        };
        // replicasets in the same ns
        if rs_namespace != namespace {
            return false;
        }

        // not the current version if we are actively trying to register it
        if let Some(except_rs) = except_rs {
            let rs_name = rs.name_any();

            if rs_name == except_rs {
                return false;
            }
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

    // keep track of the rs that are still in-use by restate (active services or invocations)
    let mut blocking = Vec::new();
    // versions a force deletion tore down with invocations still running
    let mut abandoned = Vec::new();
    let mut next_removal: Option<chrono::DateTime<chrono::Utc>> = None;

    /// Everything the planner needs to know about one version.
    fn facts<'a>(rs: &'a ReplicaSet, ctx: &Context, namespace: &str) -> VersionFacts<'a> {
        VersionFacts {
            recorded_id: rs
                .annotations()
                .get(RESTATE_DEPLOYMENT_ID_ANNOTATION)
                .map(String::as_str),
            remove_at: rs
                .annotations()
                .get(RESTATE_REMOVE_VERSION_AT_ANNOTATION)
                .and_then(|remove_at| {
                    chrono::DateTime::parse_from_rfc3339(remove_at)
                        .map(|t| t.to_utc())
                        .ok()
                }),
            replicas: rs.spec.as_ref().and_then(|spec| spec.replicas).unwrap_or(1),
            // a retained slot still has its HPA deleted, which is a write of its own
            has_hpa: ctx
                .hpa_store
                .get(&ObjectRef::<HorizontalPodAutoscaler>::new(&rs.name_any()).within(namespace))
                .is_some(),
        }
    }

    let facts: Vec<VersionFacts<'_>> = replicasets
        .iter()
        .map(|rs| facts(rs, ctx, namespace))
        .collect();

    // Nothing in this pass may act on a single answer that leaves a registered id out: a
    // node that has not loaded its schema answers with no rows at all (restate#6063), and
    // any answer is a snapshot that omits whatever was registered after it.
    let (plan, deployments) = planned_pass(
        &facts,
        deployments,
        mode,
        rsd.spec.revision_history_limit,
        confirm,
    )
    .await?;
    // The executor may delete a retained version's HPA only if the planner saw it: one that
    // reached the cache after planning was not confirmed, and waits for the next pass.
    let planned_hpas: Vec<bool> = facts.iter().map(|facts| facts.has_hpa).collect();

    for ((rs, planned), planned_hpa) in replicasets.into_iter().zip(plan).zip(planned_hpas) {
        let rs_name = rs.name_any();

        let rs_deployment_id = rs
            .annotations()
            .get(RESTATE_DEPLOYMENT_ID_ANNOTATION)
            .cloned();
        let usage = rs_deployment_id
            .as_deref()
            .and_then(|rs_deployment_id| deployments.get(rs_deployment_id).copied());
        // what the planner already decided; the executor re-derives nothing
        let action = planned.action;

        match action {
            VersionAction::Keep => {
                blocking.push(BlockingVersion {
                    deployment_id: rs_deployment_id,
                    name: rs_name.clone(),
                    usage: usage.expect("an active version has a row in the answer"),
                });

                // Per-version autoscaling: a non-latest version has an operator HPA
                // iff it is still active and autoscaling is configured. (See
                // `plan_active_version_hpa` for the decision; the inactive case is
                // handled unconditionally below, before scale-down.)
                match super::autoscaling::plan_active_version_hpa(
                    rsd.spec.autoscaling.is_some(),
                    mode.is_deleting(),
                ) {
                    HpaPlan::Ensure => {
                        if let Some(template) = rsd.spec.autoscaling.as_ref()
                            && let Err(err) = super::autoscaling::reconcile_version_hpa(
                                &ctx.client,
                                rsd,
                                namespace,
                                &rs_name,
                                template,
                            )
                            .await
                        {
                            // A bad autoscaling template (rejected by the apiserver at
                            // HPA-apply time) or a transient apply failure must not wedge
                            // cleanup of every other version — drain scheduling, scale-down
                            // and teardown still need to run. Surface it and carry on; this
                            // version just stays at full replicas (no worse than no
                            // autoscaling) until the template is fixed or the apply succeeds.
                            warn!(
                                "failed to apply autoscaling HPA for {rs_name} in {namespace}: {err}; \
                             continuing (other versions unaffected)"
                            );
                            let _ = ctx
                            .recorder
                            .publish(
                                &Event {
                                    type_: EventType::Warning,
                                    reason: "AutoscalingApplyFailed".into(),
                                    note: Some(format!(
                                        "Failed to apply per-version HorizontalPodAutoscaler for {rs_name}: {err}"
                                    )),
                                    action: "Reconcile".into(),
                                    secondary: None,
                                },
                                &rsd.object_ref(&()),
                            )
                            .await;
                        }
                    }
                    HpaPlan::RemoveAndRestore => {
                        // Autoscaling was disabled/removed: if we previously created an
                        // HPA for this version, drop it and restore the version to full
                        // replicas. Gate on the owned-HPA cache so we only call the API
                        // when an HPA actually exists.
                        let hpa_ref =
                            ObjectRef::<HorizontalPodAutoscaler>::new(&rs_name).within(namespace);
                        if ctx.hpa_store.get(&hpa_ref).is_some() {
                            super::autoscaling::delete_version_hpa(
                                &ctx.client,
                                namespace,
                                &rs_name,
                            )
                            .await?;
                            rs_api
                                .patch_scale(
                                    &rs_name,
                                    &PatchParams::apply("restate-operator/scale-restore").force(),
                                    &Patch::Apply(serde_json::json!({
                                        "apiVersion": Scale::api_version(&()),
                                        "kind": Scale::kind(&()),
                                        "spec": { "replicas": rsd.spec.replicas }
                                    })),
                                )
                                .await?;
                        }
                    }
                    HpaPlan::Skip => {}
                }

                // it was scheduled for removal but looks active again, so reset the timer
                unschedule_version_removal(rs_api, &rs).await?;

                continue;
            }
            VersionAction::Schedule => {
                // the deadline is computed when it is written, so the API calls above
                // cannot shorten this version's drain
                let remove_at = schedule_version_removal(
                    rs_api,
                    namespace,
                    &rs_name,
                    rsd.spec.restate.drain_delay_seconds(),
                )
                .await?;

                next_removal =
                    Some(next_removal.map_or(remove_at, |soonest| soonest.min(remove_at)));

                continue;
            }
            VersionAction::Wait(remove_at) => {
                next_removal =
                    Some(next_removal.map_or(remove_at, |soonest| soonest.min(remove_at)));

                continue;
            }
            VersionAction::Retain | VersionAction::Remove { .. } => {
                // Remove any operator HPA first: its minReplicas floor (>= 1) would fight
                // the scale to zero below, and would hold the version at the floor through
                // the revision-history retention window. Gate on the owned-HPA cache so we
                // only call the API when an HPA actually exists.
                let hpa_ref = ObjectRef::<HorizontalPodAutoscaler>::new(&rs_name).within(namespace);
                let delete_hpa = match action {
                    VersionAction::Retain => planned_hpa,
                    _ => ctx.hpa_store.get(&hpa_ref).is_some(),
                };
                if delete_hpa {
                    super::autoscaling::delete_version_hpa(&ctx.client, namespace, &rs_name)
                        .await?;
                }

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

                if action == VersionAction::Retain {
                    // the planner already counted it against the revision history
                    continue;
                }

                if let Some(usage) = usage.filter(|usage| usage.in_flight_invocations() > 0) {
                    abandoned.push(BlockingVersion {
                        deployment_id: rs_deployment_id.clone(),
                        name: rs_name.clone(),
                        usage,
                    });
                }

                if let VersionAction::Remove { deregister: true } = action {
                    let rs_deployment_id = rs_deployment_id
                        .as_deref()
                        .expect("only a listed deployment can be deregistered");

                    debug!(
                        "Force-deleting Restate deployment {rs_deployment_id} as its associated with old ReplicaSet {rs_name} in namespace {namespace}"
                    );
                    let resp = ctx
                        .request(
                            Method::DELETE,
                            &rsd.spec.restate.register,
                            &format!("/deployments/{rs_deployment_id}?force=true"),
                        )?
                        .send()
                        .await
                        .map_err(Error::AdminCallFailed)?;

                    // for idempotency we have to allow 404
                    if resp.status() != reqwest::StatusCode::NOT_FOUND {
                        crate::controllers::restatedeployment::controller::check_admin_response(
                            resp,
                        )
                        .await?;
                    }
                }

                debug!("Deleting old ReplicaSet {rs_name} in namespace {namespace}");
                rs_api.delete(&rs_name, &Default::default()).await?;

                continue;
            }
        }
    }

    // If there are active old deployments still draining but no removal is yet scheduled,
    // requeue on a short poll interval to detect drain completion promptly.
    if !blocking.is_empty() && next_removal.is_none() {
        let poll_seconds = 10;
        next_removal = Some(
            chrono::Utc::now()
                .checked_add_signed(chrono::TimeDelta::seconds(poll_seconds))
                .expect("next_removal in bounds"),
        );
    }

    Ok(CleanupOutcome {
        blocking,
        next_removal,
        abandoned,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resources::restatedeployments::{
        PodTemplateSpec, RestateAdminEndpoint, RestateDeploymentSpec, RestateSpec, TunnelMode,
    };
    use serde_json::json;

    fn test_rsd(tunnel_mode: Option<TunnelMode>) -> RestateDeployment {
        RestateDeployment::new(
            "greeter",
            RestateDeploymentSpec {
                deployment_mode: None,
                knative: None,
                replicas: 1,
                revision_history_limit: 10,
                min_ready_seconds: None,
                selector: None,
                template: PodTemplateSpec {
                    metadata: None,
                    spec: Some(json!({"containers": [{"name": "app", "image": "greeter:1"}]})),
                },
                restate: RestateSpec {
                    register: RestateAdminEndpoint {
                        cluster: None,
                        cloud: Some("my-env".into()),
                        service: None,
                        url: None,
                    },
                    service_path: None,
                    use_http11: None,
                    tunnel_mode,
                    drain_delay_seconds: None,
                    delete_policy: None,
                    drain: None,
                },
                autoscaling: None,
            },
        )
    }

    fn test_params() -> InProcessTunnelParams {
        InProcessTunnelParams {
            environment_id: "env_123".into(),
            region: "us".into(),
            signing_public_key: "publickeyv1_abc".into(),
            proxy_base_url: None,
        }
    }

    #[test]
    fn hash_unchanged_without_in_process_tunnel() {
        // the only-when-set property: deployments that don't use the mode must keep
        // their hashes across operator upgrades
        let rsd = test_rsd(None);
        let template = pod_template_annotation(&rsd);
        assert_eq!(
            generate_pod_template_hash(&rsd, &template, None),
            generate_pod_template_hash(&rsd, &template, None),
        );
    }

    #[test]
    fn hash_incorporates_in_process_tunnel_params() {
        let rsd = test_rsd(Some(TunnelMode::InProcess));
        let template = pod_template_annotation(&rsd);

        let without = generate_pod_template_hash(&rsd, &template, None);
        let with = generate_pod_template_hash(&rsd, &template, Some(&test_params()));
        assert_ne!(without, with);

        let mut repointed = test_params();
        repointed.environment_id = "env_456".into();
        assert_ne!(
            generate_pod_template_hash(&rsd, &template, Some(&repointed)),
            with
        );

        let mut rotated = test_params();
        rotated.signing_public_key = "publickeyv1_xyz".into();
        assert_ne!(
            generate_pod_template_hash(&rsd, &template, Some(&rotated)),
            with
        );

        let mut moved = test_params();
        moved.region = "eu".into();
        assert_ne!(
            generate_pod_template_hash(&rsd, &template, Some(&moved)),
            with
        );

        let mut proxied = test_params();
        proxied.proxy_base_url = Some("https://proxy.us.restate.cloud:9080".into());
        assert_ne!(
            generate_pod_template_hash(&rsd, &template, Some(&proxied)),
            with
        );

        // deterministic for equal inputs
        assert_eq!(
            generate_pod_template_hash(&rsd, &template, Some(&test_params())),
            with
        );
    }

    #[test]
    fn inject_appends_env_to_every_container() {
        let spec = json!({
            "containers": [
                {"name": "app", "image": "greeter:1", "env": [{"name": "USER_VAR", "value": "kept"}]},
                {"name": "sidecar", "image": "proxy:1"},
            ],
            "serviceAccountName": "greeter",
        });

        let injected =
            inject_in_process_tunnel_env(spec, "greeter-abc123", &test_params()).unwrap();

        // untouched fields pass through
        assert_eq!(injected["serviceAccountName"], "greeter");

        for (container, expected_len) in [(0, 5), (1, 4)] {
            let env = injected["containers"][container]["env"].as_array().unwrap();
            assert_eq!(env.len(), expected_len, "container {container}");
            let get = |name: &str| {
                env.iter()
                    .find(|e| e["name"] == name)
                    .unwrap_or_else(|| panic!("{name} missing in container {container}"))["value"]
                    .clone()
            };
            assert_eq!(get(INPROC_TUNNEL_NAME_ENV), "greeter-abc123");
            assert_eq!(get(INPROC_ENVIRONMENT_ID_ENV), "env_123");
            assert_eq!(get(INPROC_CLOUD_REGION_ENV), "us");
            assert_eq!(get(INPROC_SIGNING_PUBLIC_KEY_ENV), "publickeyv1_abc");
        }

        // user env is preserved
        assert_eq!(injected["containers"][0]["env"][0]["name"], "USER_VAR");
    }

    #[test]
    fn inject_covers_init_containers() {
        // native sidecars (initContainers with restartPolicy: Always) may host the
        // tunnel client too — they get the same env vars and the same conflict check
        let spec = json!({
            "containers": [{"name": "app"}],
            "initContainers": [{"name": "sidecar", "restartPolicy": "Always"}],
        });
        let injected =
            inject_in_process_tunnel_env(spec, "greeter-abc123", &test_params()).unwrap();
        let env = injected["initContainers"][0]["env"].as_array().unwrap();
        assert_eq!(env.len(), 4);
        assert_eq!(env[0]["name"], INPROC_TUNNEL_NAME_ENV);

        let conflicting = json!({
            "containers": [{"name": "app"}],
            "initContainers": [
                {"name": "sidecar", "env": [{"name": INPROC_ENVIRONMENT_ID_ENV, "value": "mine"}]},
            ],
        });
        let err = inject_in_process_tunnel_env(conflicting, "greeter-abc123", &test_params())
            .expect_err("user-declared RESTATE_INPROC_* in an initContainer must be rejected");
        assert!(err.to_string().contains("sidecar"));
    }

    #[test]
    fn inject_rejects_user_declared_inproc_var() {
        let spec = json!({
            "containers": [
                {"name": "app", "env": [{"name": INPROC_TUNNEL_NAME_ENV, "value": "mine"}]},
            ],
        });

        let err = inject_in_process_tunnel_env(spec, "greeter-abc123", &test_params())
            .expect_err("user-declared RESTATE_INPROC_* must be rejected");
        assert!(matches!(err, Error::InvalidRestateConfig(_)));
        assert!(err.to_string().contains(INPROC_TUNNEL_NAME_ENV));
        assert!(err.to_string().contains("app"));
    }

    #[test]
    fn inject_rejects_non_array_env() {
        let spec = json!({
            "containers": [{"name": "app", "env": "oops"}],
        });

        let err = inject_in_process_tunnel_env(spec, "greeter-abc123", &test_params())
            .expect_err("non-array env must be rejected");
        assert!(matches!(err, Error::InvalidRestateConfig(_)));
    }

    #[test]
    fn inject_passes_through_invalid_pod_specs() {
        // a pod spec without containers is invalid; the api server reports that better
        let spec = json!({"volumes": []});
        let injected =
            inject_in_process_tunnel_env(spec.clone(), "greeter-abc123", &test_params()).unwrap();
        assert_eq!(injected, spec);
    }

    // --- cleanup_old_replicasets against a mocked apiserver. The teardown *order*
    // is the invariant here, not just which calls happen, so these record every
    // request the reconciler makes and assert on the sequence. ---

    mod teardown {
        use std::any::Any;
        use std::convert::Infallible;
        use std::sync::{Arc, Mutex};

        use http::{Request, Response};
        use kube::client::Body;
        use kube::runtime::reflector;
        use kube::runtime::watcher;
        use serde_json::json;

        use super::super::*;
        use crate::controllers::State;
        use crate::controllers::restatedeployment::cleanup::DeploymentUsage;
        use crate::metrics::Metrics;
        use crate::resources::restatedeployments::RestateDeployment;

        const NAMESPACE: &str = "apps";
        const RSD_UID: &str = "uid-123";
        const VERSION: &str = "greeter-old";
        /// The version being rolled out: excluded from the sweep, but its recorded id is
        /// what dates the answer cleanup is given.
        const CURRENT: &str = "greeter-new";
        const NEVER_REGISTERED: &str = "greeter-fresh";
        const CURRENT_DEPLOYMENT_ID: &str = "dp_current";
        /// The scale patch, as the apiserver stub records it.
        fn scale_of(version: &str) -> String {
            format!("PATCH /apis/apps/v1/namespaces/{NAMESPACE}/replicasets/{version}/scale")
        }

        /// One request the reconciler made.
        #[derive(Clone)]
        struct Call {
            method: String,
            path: String,
            field_manager: Option<String>,
            /// Which kind of patch it was.
            content_type: Option<String>,
        }

        /// Every request the reconciler made, in order.
        #[derive(Clone, Default)]
        struct Calls(Arc<Mutex<Vec<Call>>>);

        impl Calls {
            /// Matching requests as "METHOD /path".
            fn matching(&self, needle: &str) -> Vec<String> {
                self.to_path(needle)
                    .into_iter()
                    .map(|call| format!("{} {}", call.method, call.path))
                    .collect()
            }

            /// The field managers of matching requests, in order.
            fn field_managers(&self, needle: &str) -> Vec<String> {
                self.to_path(needle)
                    .into_iter()
                    .filter_map(|call| call.field_manager)
                    .collect()
            }

            /// The patch types of matching requests, in order.
            fn patch_types(&self, needle: &str) -> Vec<String> {
                self.to_path(needle)
                    .into_iter()
                    .filter_map(|call| call.content_type)
                    .collect()
            }

            fn to_path(&self, needle: &str) -> Vec<Call> {
                self.0
                    .lock()
                    .expect("calls are not poisoned")
                    .iter()
                    .filter(|call| call.path.contains(needle))
                    .cloned()
                    .collect()
            }
        }

        fn field_manager_of(query: Option<&str>) -> Option<String> {
            query?
                .split('&')
                .find_map(|pair| pair.strip_prefix("fieldManager="))
                // slashes in manager names are escaped on the wire
                .map(|manager| manager.replace("%2F", "/"))
        }

        struct Harness {
            ctx: Arc<Context>,
            rs_api: Api<ReplicaSet>,
            calls: Calls,
            /// Stores read through their `Store` handles; the writers own the data.
            _writers: Vec<Box<dyn Any>>,
        }

        fn store_of<K>(objects: Vec<K>) -> (reflector::Store<K>, Box<dyn Any>)
        where
            K: reflector::Lookup + Clone + 'static,
            K::DynamicType: Default + Eq + std::hash::Hash + Clone,
        {
            let (reader, mut writer) = reflector::store::<K>();
            writer.apply_watcher_event(&watcher::Event::Init);
            for object in objects {
                writer.apply_watcher_event(&watcher::Event::InitApply(object));
            }
            writer.apply_watcher_event(&watcher::Event::InitDone);
            (reader, Box::new(writer))
        }

        fn harness(replicasets: Vec<ReplicaSet>, hpas: Vec<HorizontalPodAutoscaler>) -> Harness {
            let calls = Calls::default();
            let client = {
                let calls = calls.clone();
                let svc = tower::service_fn(move |req: Request<Body>| {
                    let calls = calls.clone();
                    async move {
                        let path = req.uri().path().to_owned();
                        let content_type = req
                            .headers()
                            .get(http::header::CONTENT_TYPE)
                            .and_then(|value| value.to_str().ok())
                            .map(str::to_owned);
                        calls.0.lock().expect("calls are not poisoned").push(Call {
                            method: req.method().to_string(),
                            path: path.clone(),
                            field_manager: field_manager_of(req.uri().query()),
                            content_type,
                        });

                        // enough of a body for kube to deserialise the return type of
                        // whichever call this was; none of it is asserted on
                        let body = if path.ends_with("/scale") {
                            json!({
                                "apiVersion": "autoscaling/v1", "kind": "Scale",
                                "metadata": { "name": VERSION, "namespace": NAMESPACE },
                                "spec": { "replicas": 0 },
                            })
                        } else if path.contains("horizontalpodautoscalers") {
                            json!({
                                "apiVersion": "autoscaling/v2", "kind": "HorizontalPodAutoscaler",
                                "metadata": { "name": VERSION, "namespace": NAMESPACE },
                            })
                        } else {
                            json!({
                                "apiVersion": "apps/v1", "kind": "ReplicaSet",
                                "metadata": { "name": VERSION, "namespace": NAMESPACE },
                            })
                        };

                        Ok::<_, Infallible>(
                            Response::builder()
                                .status(200)
                                .header("content-type", "application/json")
                                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                                .unwrap(),
                        )
                    }
                });
                kube::Client::new(svc, NAMESPACE)
            };

            let (replicasets_store, rs_writer) = store_of(replicasets);
            let (hpa_store, hpa_writer) = store_of(hpas);
            let (rce_store, rce_writer) = store_of(vec![]);
            let (secret_store, secret_writer) = store_of(vec![]);
            let (revision_store, revision_writer) = store_of(vec![]);
            let (configuration_store, configuration_writer) = store_of(vec![]);

            let ctx = Context::new(
                client.clone(),
                replicasets_store,
                rce_store,
                secret_store,
                revision_store,
                configuration_store,
                hpa_store,
                Metrics::default(),
                State::new(
                    None,
                    false,
                    None,
                    "restate-operator".into(),
                    None,
                    None,
                    "tunnel:latest".into(),
                    "cluster.local".into(),
                    "alpine:3.21".into(),
                    None,
                    None,
                ),
            );

            Harness {
                rs_api: Api::namespaced(client, NAMESPACE),
                ctx,
                calls,
                _writers: vec![
                    rs_writer,
                    hpa_writer,
                    rce_writer,
                    secret_writer,
                    revision_writer,
                    configuration_writer,
                ],
            }
        }

        /// Stand-in for the Restate admin API: records the request line into the same
        /// call log as the apiserver stub, and answers 200 to everything.
        async fn admin_stub(calls: Calls) -> url::Url {
            admin_stub_with_delay(calls, std::time::Duration::ZERO).await
        }

        async fn admin_stub_with_delay(calls: Calls, delay: std::time::Duration) -> url::Url {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("admin stub binds");
            let url = format!("http://{}/", listener.local_addr().expect("stub address"));

            tokio::spawn(async move {
                while let Ok((mut socket, _)) = listener.accept().await {
                    let calls = calls.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(delay).await;
                        let mut buf = [0u8; 2048];
                        let read = socket.read(&mut buf).await.unwrap_or(0);
                        if let Some(request_line) =
                            String::from_utf8_lossy(&buf[..read]).lines().next()
                        {
                            let mut parts = request_line.split_whitespace();
                            let method = parts.next().unwrap_or_default();
                            let path = parts.next().unwrap_or_default();
                            calls.0.lock().expect("calls are not poisoned").push(Call {
                                method: method.to_owned(),
                                path: path.to_owned(),
                                field_manager: None,
                                content_type: None,
                            });
                        }
                        let _ = socket
                            .write_all(
                                b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 2\r\n\r\n{}",
                            )
                            .await;
                    });
                }
            });

            url.parse().expect("stub url parses")
        }

        fn rsd(autoscaling: bool, deleting: bool) -> RestateDeployment {
            rsd_with_history(10, autoscaling, deleting)
        }

        fn rsd_with_history(
            revision_history_limit: i32,
            autoscaling: bool,
            deleting: bool,
        ) -> RestateDeployment {
            let spec = serde_json::from_value(json!({
                "replicas": 3,
                "revisionHistoryLimit": revision_history_limit,
                "template": {
                    "metadata": null,
                    "spec": { "containers": [{ "name": "app", "image": "greeter:v1" }] }
                },
                "restate": {
                    "register": { "cluster": null, "cloud": null, "service": null, "url": "http://restate:9070/" },
                    "servicePath": null, "useHttp11": null, "drainDelaySeconds": null
                },
                "autoscaling": autoscaling.then(|| json!({ "minReplicas": 1, "maxReplicas": 5 })),
            }))
            .expect("test RestateDeploymentSpec deserializes");

            let mut rsd = RestateDeployment::new("greeter", spec);
            rsd.metadata.uid = Some(RSD_UID.into());
            rsd.metadata.namespace = Some(NAMESPACE.into());
            if deleting {
                rsd.metadata.deletion_timestamp = Some(
                    k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(chrono::Utc::now()),
                );
            }
            rsd
        }

        const OLDER: &str = "2026-01-01T00:00:00Z";
        /// An older version, when a test needs two of them.
        const OLDER_VERSION: &str = "greeter-older";
        const NEWER: &str = "2026-01-02T00:00:00Z";

        fn version(
            deployment_id: &str,
            remove_at: Option<chrono::DateTime<chrono::Utc>>,
        ) -> ReplicaSet {
            versioned(VERSION, Some(deployment_id), remove_at, 3, OLDER)
        }

        /// A version that was never registered carries no deployment-id annotation.
        fn unregistered_version() -> ReplicaSet {
            versioned(VERSION, None, None, 3, OLDER)
        }

        fn versioned(
            name: &str,
            deployment_id: Option<&str>,
            remove_at: Option<chrono::DateTime<chrono::Utc>>,
            replicas: i32,
            created: &str,
        ) -> ReplicaSet {
            let mut annotations = serde_json::Map::new();
            if let Some(deployment_id) = deployment_id {
                annotations.insert(
                    RESTATE_DEPLOYMENT_ID_ANNOTATION.into(),
                    json!(deployment_id),
                );
            }
            if let Some(remove_at) = remove_at {
                annotations.insert(
                    RESTATE_REMOVE_VERSION_AT_ANNOTATION.into(),
                    json!(remove_at.to_rfc3339()),
                );
            }

            serde_json::from_value(json!({
                "apiVersion": "apps/v1",
                "kind": "ReplicaSet",
                "metadata": {
                    "name": name,
                    "namespace": NAMESPACE,
                    "creationTimestamp": created,
                    "annotations": annotations,
                    "ownerReferences": [{
                        "apiVersion": "restate.dev/v1beta1",
                        "kind": "RestateDeployment",
                        "name": "greeter",
                        "uid": RSD_UID,
                        "controller": true,
                    }],
                },
                "spec": { "replicas": replicas },
            }))
            .expect("test ReplicaSet deserializes")
        }

        fn version_hpa() -> HorizontalPodAutoscaler {
            serde_json::from_value(json!({
                "apiVersion": "autoscaling/v2",
                "kind": "HorizontalPodAutoscaler",
                "metadata": { "name": VERSION, "namespace": NAMESPACE },
                "spec": {
                    "scaleTargetRef": {
                        "apiVersion": "apps/v1", "kind": "ReplicaSet", "name": VERSION,
                    },
                    "minReplicas": 1, "maxReplicas": 5,
                },
            }))
            .expect("test HorizontalPodAutoscaler deserializes")
        }

        fn usage_of(deployment_id: &str, usage: DeploymentUsage) -> DeploymentUsageMap {
            [(deployment_id.to_owned(), usage)].into()
        }

        /// A second answer, counted so a test can assert whether the query was taken at all.
        struct SecondAnswer {
            answer: DeploymentUsageMap,
            asked: Arc<Mutex<usize>>,
        }

        impl SecondAnswer {
            fn empty() -> Self {
                Self::of(DeploymentUsageMap::new())
            }

            fn of(answer: DeploymentUsageMap) -> Self {
                Self {
                    answer,
                    asked: Arc::new(Mutex::new(0)),
                }
            }

            fn asked(&self) -> usize {
                *self.asked.lock().expect("the counter is not poisoned")
            }

            /// The capability the reconciler calls when something rests on an absence. No
            /// delay: the wait lives in the caller's closure, not in the rule.
            fn confirm(&self) -> impl FnOnce() -> std::future::Ready<Result<DeploymentUsageMap>> {
                let answer = self.answer.clone();
                let asked = self.asked.clone();
                move || {
                    *asked.lock().expect("the counter is not poisoned") += 1;
                    std::future::ready(Ok(answer))
                }
            }
        }

        fn answer_of(
            entries: impl IntoIterator<Item = (&'static str, DeploymentUsage)>,
        ) -> DeploymentUsageMap {
            entries
                .into_iter()
                .map(|(id, usage)| (id.to_owned(), usage))
                .collect()
        }

        fn active_row() -> DeploymentUsage {
            DeploymentUsage {
                latest_for_service: true,
                ..Default::default()
            }
        }

        fn pinned_row() -> DeploymentUsage {
            DeploymentUsage {
                pinned_invocations: 1,
                ..Default::default()
            }
        }

        /// The autoscaler comes off immediately before the scale to zero, in the same
        /// branch: its `minReplicas` floor of 1 would otherwise fight the scale-down and
        /// hold the version at that floor for the whole retention window.
        #[tokio::test(start_paused = true)]
        async fn autoscaler_is_removed_immediately_before_the_scale_to_zero() {
            let rsd = rsd(true, false);
            let harness = harness(
                vec![version(
                    "dp_gone",
                    // it drained once already, so its deadline has passed
                    Some(chrono::Utc::now() - chrono::TimeDelta::seconds(60)),
                )],
                vec![version_hpa()],
            );

            // both answers leave it out: the absence is confirmed
            let second = SecondAnswer::empty();

            let CleanupOutcome {
                blocking,
                next_removal,
                ..
            } = cleanup_old_replicasets(
                NAMESPACE,
                &harness.ctx,
                &harness.rs_api,
                RSD_UID,
                &rsd,
                CleanupMode::Rollout,
                &DeploymentUsageMap::new(),
                None,
                second.confirm(),
            )
            .await
            .expect("cleanup succeeds");

            assert_eq!(second.asked(), 1);
            assert!(blocking.is_empty());
            assert_eq!(next_removal, None);
            assert_eq!(
                harness.calls.matching(VERSION),
                vec![
                    format!(
                        "DELETE /apis/autoscaling/v2/namespaces/{NAMESPACE}/horizontalpodautoscalers/{VERSION}"
                    ),
                    scale_of(VERSION),
                ],
            );
        }

        /// ...and not before that. A version waiting out its drain deadline is still
        /// serving traffic, so it keeps the autoscaler it was given, and an answer that
        /// lists it costs no second query.
        #[tokio::test]
        async fn draining_version_keeps_its_autoscaler_until_the_deadline() {
            let rsd = rsd(true, false);
            let remove_at = chrono::Utc::now() + chrono::TimeDelta::seconds(300);
            let harness = harness(
                vec![version("dp_draining", Some(remove_at))],
                vec![version_hpa()],
            );

            let second = SecondAnswer::empty();
            let CleanupOutcome {
                blocking,
                next_removal,
                ..
            } = cleanup_old_replicasets(
                NAMESPACE,
                &harness.ctx,
                &harness.rs_api,
                RSD_UID,
                &rsd,
                CleanupMode::Rollout,
                // registered, superseded, and nothing in flight: drained but not yet due
                &usage_of("dp_draining", DeploymentUsage::default()),
                None,
                second.confirm(),
            )
            .await
            .expect("cleanup succeeds");

            assert_eq!(second.asked(), 0, "nothing rests on an absence");
            assert!(blocking.is_empty());
            assert_eq!(next_removal, Some(remove_at));
            assert_eq!(
                harness.calls.matching(VERSION),
                Vec::<String>::new(),
                "nothing is touched until the deadline passes"
            );
        }

        /// `deletePolicy: force` doesn't wait: a version with invocations in flight and a
        /// drain deadline still ahead of it is torn down on this pass, and says what it
        /// walked over.
        #[tokio::test]
        async fn force_deletion_tears_down_a_busy_version_immediately() {
            let remove_at = chrono::Utc::now() + chrono::TimeDelta::seconds(300);
            let harness = harness(
                vec![version("dp_busy", Some(remove_at))],
                vec![version_hpa()],
            );

            let mut rsd = rsd(true, true);
            rsd.spec.restate.register.url = Some(admin_stub(harness.calls.clone()).await);

            let busy = DeploymentUsage {
                latest_for_service: true,
                pinned_invocations: 4,
                unpinned_invocations: 0,
            };

            let second = SecondAnswer::empty();
            let CleanupOutcome {
                blocking,
                abandoned,
                ..
            } = cleanup_old_replicasets(
                NAMESPACE,
                &harness.ctx,
                &harness.rs_api,
                RSD_UID,
                &rsd,
                CleanupMode::ForceDeleting,
                &usage_of("dp_busy", busy),
                None,
                second.confirm(),
            )
            .await
            .expect("cleanup succeeds");

            assert_eq!(second.asked(), 0, "a force deletion confirms nothing");
            assert!(blocking.is_empty(), "nothing holds a force deletion");
            assert_eq!(
                abandoned,
                vec![BlockingVersion {
                    name: VERSION.into(),
                    deployment_id: Some("dp_busy".into()),
                    usage: busy,
                }],
            );
            assert_eq!(
                harness.calls.matching(VERSION),
                vec![
                    format!(
                        "DELETE /apis/autoscaling/v2/namespaces/{NAMESPACE}/horizontalpodautoscalers/{VERSION}"
                    ),
                    scale_of(VERSION),
                    format!("DELETE /apis/apps/v1/namespaces/{NAMESPACE}/replicasets/{VERSION}"),
                ],
                "no remove-version-at patch: the drain delay is skipped entirely"
            );
            assert_eq!(
                harness.calls.matching("/deployments/"),
                vec!["DELETE /deployments/dp_busy?force=true".to_owned()],
            );
        }

        /// The same holds while the RestateDeployment itself is being deleted: deletion
        /// puts every version through the drain, and stripping their autoscalers up front
        /// would collapse them to `spec.replicas` while they still serve invocations.
        #[tokio::test]
        async fn deletion_does_not_strip_autoscalers_up_front() {
            let rsd = rsd(true, true);
            let remove_at = chrono::Utc::now() + chrono::TimeDelta::seconds(300);
            let harness = harness(
                vec![version("dp_busy", Some(remove_at))],
                vec![version_hpa()],
            );

            let busy = DeploymentUsage {
                latest_for_service: true,
                pinned_invocations: 2,
                unpinned_invocations: 0,
            };

            let second = SecondAnswer::empty();
            let CleanupOutcome { blocking, .. } = cleanup_old_replicasets(
                NAMESPACE,
                &harness.ctx,
                &harness.rs_api,
                RSD_UID,
                &rsd,
                CleanupMode::Deleting,
                &usage_of("dp_busy", busy),
                None,
                second.confirm(),
            )
            .await
            .expect("cleanup succeeds");

            assert_eq!(second.asked(), 0);
            assert_eq!(
                blocking,
                vec![BlockingVersion {
                    name: VERSION.into(),
                    deployment_id: Some("dp_busy".into()),
                    usage: busy,
                }],
                "in-flight invocations hold the deletion, and say so"
            );
            assert_eq!(
                harness.calls.matching("horizontalpodautoscalers"),
                Vec::<String>::new(),
                "an owned HPA is garbage-collected with the RestateDeployment, not by us"
            );
        }

        /// Stamp and clear both go out under one field manager. The controller's rollback
        /// path clears through this same helper.
        #[tokio::test]
        async fn the_drain_deadline_is_stamped_and_cleared_by_one_field_manager() {
            const MANAGER: &str = "restate-operator/remove-version-at";
            let patch_of =
                |name| format!("PATCH /apis/apps/v1/namespaces/{NAMESPACE}/replicasets/{name}");
            let rsd = rsd(false, false);

            // superseded with nothing in flight, so this pass stamps a deadline
            let stamping = harness(vec![version("dp_super", None)], vec![]);
            let second = SecondAnswer::empty();
            let CleanupOutcome { next_removal, .. } = cleanup_old_replicasets(
                NAMESPACE,
                &stamping.ctx,
                &stamping.rs_api,
                RSD_UID,
                &rsd,
                CleanupMode::Rollout,
                &usage_of("dp_super", DeploymentUsage::default()),
                None,
                second.confirm(),
            )
            .await
            .expect("cleanup succeeds");

            assert_eq!(second.asked(), 0, "the row is in the answer");
            assert!(next_removal.is_some(), "the version is left to drain");
            assert_eq!(stamping.calls.matching(VERSION), vec![patch_of(VERSION)]);
            assert_eq!(stamping.calls.field_managers(VERSION), vec![MANAGER]);
            assert_eq!(
                stamping.calls.patch_types(VERSION),
                vec!["application/apply-patch+yaml"],
            );

            // a version still holding an invocation sheds its deadline, same manager
            let clearing = harness(vec![version("dp_pinned", Some(chrono::Utc::now()))], vec![]);
            let pinned = pinned_row();
            let second = SecondAnswer::empty();
            let CleanupOutcome { blocking, .. } = cleanup_old_replicasets(
                NAMESPACE,
                &clearing.ctx,
                &clearing.rs_api,
                RSD_UID,
                &rsd,
                CleanupMode::Rollout,
                &usage_of("dp_pinned", pinned),
                None,
                second.confirm(),
            )
            .await
            .expect("cleanup succeeds");

            assert_eq!(blocking.len(), 1);
            assert_eq!(clearing.calls.matching(VERSION), vec![patch_of(VERSION)]);
            assert_eq!(clearing.calls.field_managers(VERSION), vec![MANAGER]);
            assert_eq!(
                clearing.calls.patch_types(VERSION),
                vec!["application/apply-patch+yaml"],
            );
        }

        /// The incident: a node that had not loaded its schema answered `/query` with no
        /// rows. One second answer is all it takes — nothing is scheduled, scaled or deleted
        /// on the strength of the first, whether the version is live or already at zero.
        #[tokio::test(start_paused = true)]
        async fn a_first_answer_with_no_rows_removes_nothing() {
            let rsd = rsd(false, false);
            let harness = harness(
                vec![
                    version("dp_old", None),
                    // already scaled to 0 by an earlier drain, but still registered
                    versioned(OLDER_VERSION, Some("dp_zero"), None, 0, OLDER),
                ],
                vec![],
            );

            let second = SecondAnswer::of(answer_of([
                ("dp_old", active_row()),
                ("dp_zero", pinned_row()),
            ]));

            let CleanupOutcome {
                blocking,
                next_removal,
                ..
            } = cleanup_old_replicasets(
                NAMESPACE,
                &harness.ctx,
                &harness.rs_api,
                RSD_UID,
                &rsd,
                CleanupMode::Rollout,
                &DeploymentUsageMap::new(),
                None,
                second.confirm(),
            )
            .await
            .expect("cleanup succeeds");

            assert_eq!(second.asked(), 1, "the absence was confirmed");
            assert_eq!(blocking.len(), 2, "both are still needed");
            assert_eq!(
                harness.calls.matching("replicasets"),
                Vec::<String>::new(),
                "neither is scaled, deleted, or given a deadline",
            );
            assert!(next_removal.is_some(), "and they are polled for the drain");
        }

        /// A rollback re-adopts an older version as the current one, so the answer the first
        /// query ran against predates the registration of the version above it — and lists
        /// only the re-adopted id. The confirming answer lists both.
        #[tokio::test(start_paused = true)]
        async fn a_rollback_does_not_drain_the_version_registered_after_it() {
            let rsd = rsd(false, false);
            let harness = harness(
                vec![
                    versioned(CURRENT, Some(CURRENT_DEPLOYMENT_ID), None, 3, OLDER),
                    versioned(VERSION, Some("dp_new"), None, 3, NEWER),
                ],
                vec![],
            );

            let second = SecondAnswer::of(answer_of([
                (CURRENT_DEPLOYMENT_ID, DeploymentUsage::default()),
                ("dp_new", active_row()),
            ]));

            let CleanupOutcome { blocking, .. } = cleanup_old_replicasets(
                NAMESPACE,
                &harness.ctx,
                &harness.rs_api,
                RSD_UID,
                &rsd,
                CleanupMode::Rollout,
                // the snapshot predates the newer registration, so it lists only the old one
                &usage_of(CURRENT_DEPLOYMENT_ID, DeploymentUsage::default()),
                Some(CURRENT),
                second.confirm(),
            )
            .await
            .expect("cleanup succeeds");

            assert_eq!(second.asked(), 1);
            assert_eq!(blocking.len(), 1);
            assert_eq!(
                harness.calls.matching(VERSION),
                Vec::<String>::new(),
                "the newer version is neither scaled nor given a deadline",
            );
        }

        /// `drainDelaySeconds: 0` is not what protects the version: the confirming answer is.
        #[tokio::test(start_paused = true)]
        async fn a_zero_drain_delay_still_confirms_before_removing() {
            let mut rsd = rsd(false, false);
            rsd.spec.restate.drain_delay_seconds = Some(0);

            let harness = harness(vec![version("dp_old", None)], vec![]);
            let second = SecondAnswer::of(answer_of([("dp_old", active_row())]));

            let CleanupOutcome { blocking, .. } = cleanup_old_replicasets(
                NAMESPACE,
                &harness.ctx,
                &harness.rs_api,
                RSD_UID,
                &rsd,
                CleanupMode::Rollout,
                &DeploymentUsageMap::new(),
                None,
                second.confirm(),
            )
            .await
            .expect("cleanup succeeds");

            assert_eq!(second.asked(), 1);
            assert_eq!(blocking.len(), 1);
            assert_eq!(
                harness.calls.matching("replicasets"),
                Vec::<String>::new(),
                "with a zero delay the confirming answer is all that holds it",
            );
        }

        /// A version already at zero, past its deadline and kept by the revision history costs
        /// nothing this pass, so the steady state pays no second query for it.
        #[tokio::test]
        async fn a_retained_zero_replica_absence_costs_no_second_query() {
            let rsd = rsd_with_history(10, false, false);
            let harness = harness(
                vec![versioned(
                    VERSION,
                    Some("dp_gone"),
                    Some(chrono::Utc::now() - chrono::TimeDelta::seconds(60)),
                    0,
                    OLDER,
                )],
                vec![],
            );
            let second = SecondAnswer::empty();

            let CleanupOutcome {
                blocking,
                next_removal,
                ..
            } = cleanup_old_replicasets(
                NAMESPACE,
                &harness.ctx,
                &harness.rs_api,
                RSD_UID,
                &rsd,
                CleanupMode::Rollout,
                &DeploymentUsageMap::new(),
                None,
                second.confirm(),
            )
            .await
            .expect("cleanup succeeds");

            assert_eq!(
                second.asked(),
                0,
                "a retained zero-replica version is not acted on, so nothing needs confirming",
            );
            assert!(blocking.is_empty());
            assert_eq!(next_removal, None);
            assert_eq!(
                harness.calls.matching(VERSION),
                Vec::<String>::new(),
                "nothing is touched: no deadline, no scale, no delete",
            );
        }

        /// ...unless an operator HPA still targets it: executing the retention deletes that
        /// HPA, which is a write an absent id alone would explain, so it is confirmed first.
        #[tokio::test(start_paused = true)]
        async fn a_retained_absence_holding_an_hpa_confirms_before_deleting_it() {
            let rsd = rsd_with_history(10, false, false);
            let harness = harness(
                vec![versioned(
                    VERSION,
                    Some("dp_gone"),
                    Some(chrono::Utc::now() - chrono::TimeDelta::seconds(60)),
                    0,
                    OLDER,
                )],
                vec![version_hpa()],
            );
            let second = SecondAnswer::empty();

            let CleanupOutcome { blocking, .. } = cleanup_old_replicasets(
                NAMESPACE,
                &harness.ctx,
                &harness.rs_api,
                RSD_UID,
                &rsd,
                CleanupMode::Rollout,
                &DeploymentUsageMap::new(),
                None,
                second.confirm(),
            )
            .await
            .expect("cleanup succeeds");

            assert_eq!(
                second.asked(),
                1,
                "the HPA delete rests on the absence, so it is confirmed first",
            );
            assert!(blocking.is_empty());
            assert_eq!(
                harness.calls.matching(VERSION),
                vec![format!(
                    "DELETE /apis/autoscaling/v2/namespaces/{NAMESPACE}/horizontalpodautoscalers/{VERSION}"
                )],
                "the HPA goes, and the retained ReplicaSet stays",
            );
        }

        /// ...but the same version beyond the history limit would be deleted, so that does
        /// confirm first.
        #[tokio::test(start_paused = true)]
        async fn a_zero_replica_absence_past_the_history_limit_confirms_before_deleting() {
            let rsd = rsd_with_history(0, false, false);
            let harness = harness(
                vec![versioned(
                    VERSION,
                    Some("dp_gone"),
                    Some(chrono::Utc::now() - chrono::TimeDelta::seconds(60)),
                    0,
                    OLDER,
                )],
                vec![],
            );
            let second = SecondAnswer::empty();

            cleanup_old_replicasets(
                NAMESPACE,
                &harness.ctx,
                &harness.rs_api,
                RSD_UID,
                &rsd,
                CleanupMode::Rollout,
                &DeploymentUsageMap::new(),
                None,
                second.confirm(),
            )
            .await
            .expect("cleanup succeeds");

            assert_eq!(second.asked(), 1, "the delete rests on the absence");
            assert_eq!(
                harness.calls.matching(VERSION),
                vec![format!(
                    "DELETE /apis/apps/v1/namespaces/{NAMESPACE}/replicasets/{VERSION}"
                )],
                "and it is deleted, without a deregistration",
            );
        }

        /// A history slot a never-registered version fills still leaves the absent version
        /// below it due for deletion, which rests on an absence — so it is not deleted on the
        /// strength of one answer.
        #[tokio::test(start_paused = true)]
        async fn a_history_slot_filled_by_a_never_registered_version_still_confirms() {
            let rsd = rsd_with_history(1, false, false);
            let harness = harness(
                vec![
                    // newest, never registered, so it is scaled down and kept for rollback
                    versioned(NEVER_REGISTERED, None, None, 3, NEWER),
                    // older, absent from every answer, already at zero and due
                    versioned(
                        VERSION,
                        Some("dp_gone"),
                        Some(chrono::Utc::now() - chrono::TimeDelta::seconds(60)),
                        0,
                        OLDER,
                    ),
                ],
                vec![],
            );

            let second = SecondAnswer::of(answer_of([("dp_gone", active_row())]));
            let CleanupOutcome { blocking, .. } = cleanup_old_replicasets(
                NAMESPACE,
                &harness.ctx,
                &harness.rs_api,
                RSD_UID,
                &rsd,
                CleanupMode::Rollout,
                &DeploymentUsageMap::new(),
                None,
                second.confirm(),
            )
            .await
            .expect("cleanup succeeds");

            assert_eq!(
                second.asked(),
                1,
                "the delete rests on the absence, so it is confirmed first",
            );
            assert_eq!(blocking.len(), 1, "the confirming answer lists the version");
            assert_eq!(
                harness.calls.field_managers(VERSION),
                vec!["restate-operator/remove-version-at"],
                "it is not scaled or deleted: the only write clears its stale deadline",
            );
            assert_eq!(
                harness.calls.field_managers(NEVER_REGISTERED),
                vec!["restate-operator/scale-down"],
                "the never-registered one is scaled down, and kept",
            );
        }

        /// A deadline may pass while the pass is executing a later version; the plan was
        /// made before, and is executed as planned: no removal is re-decided mid-pass.
        #[tokio::test]
        async fn a_deadline_expiring_between_planning_and_execution_causes_no_removal() {
            let rsd = rsd_with_history(0, false, false);
            let soon = chrono::Utc::now() + chrono::TimeDelta::milliseconds(500);
            let harness = harness(
                vec![
                    // newest: drained and due, so it is torn down first — slowly
                    versioned(
                        NEVER_REGISTERED,
                        Some("dp_due"),
                        Some(chrono::Utc::now() - chrono::TimeDelta::seconds(60)),
                        3,
                        NEWER,
                    ),
                    // older: its deadline is still ahead when the pass is planned
                    versioned(VERSION, Some("dp_waiting"), Some(soon), 3, OLDER),
                ],
                vec![],
            );

            let mut rsd = rsd;
            rsd.spec.restate.register.url = Some(
                admin_stub_with_delay(harness.calls.clone(), std::time::Duration::from_secs(1))
                    .await,
            );

            let second = SecondAnswer::empty();
            let CleanupOutcome { next_removal, .. } = cleanup_old_replicasets(
                NAMESPACE,
                &harness.ctx,
                &harness.rs_api,
                RSD_UID,
                &rsd,
                CleanupMode::Rollout,
                &DeploymentUsageMap::from([("dp_due".to_owned(), DeploymentUsage::default())]),
                None,
                second.confirm(),
            )
            .await
            .expect("cleanup succeeds");

            assert!(
                chrono::Utc::now() > soon,
                "the deadline passed while the pass ran",
            );
            assert_eq!(second.asked(), 0);
            assert_eq!(
                harness.calls.matching(VERSION),
                Vec::<String>::new(),
                "the planned Wait is executed as it was planned: nothing is touched",
            );
            assert_eq!(next_removal, Some(soon), "it is still waited for");
        }

        /// A confirmed absence is what the drain delay exists for: the deadline goes on this
        /// pass, and the teardown happens at it without a deregistration.
        #[tokio::test(start_paused = true)]
        async fn a_confirmed_absence_drains_then_is_removed() {
            let rsd = rsd_with_history(0, false, false);
            let scheduling = harness(vec![version("dp_gone", None)], vec![]);

            // the decision clock is chrono; the pause only makes the confirmation delay instant
            let confirmed_at = Arc::new(Mutex::new(None));
            let recorded = confirmed_at.clone();
            let confirm = move || {
                *recorded.lock().expect("the clock reading is not poisoned") =
                    Some(chrono::Utc::now());
                std::future::ready(Ok(DeploymentUsageMap::new()))
            };

            let CleanupOutcome { next_removal, .. } = cleanup_old_replicasets(
                NAMESPACE,
                &scheduling.ctx,
                &scheduling.rs_api,
                RSD_UID,
                &rsd,
                CleanupMode::Rollout,
                &DeploymentUsageMap::new(),
                None,
                confirm,
            )
            .await
            .expect("cleanup succeeds");

            let confirmed_at = confirmed_at
                .lock()
                .expect("the clock reading is not poisoned")
                .expect("the pass confirmed the absence");
            let remove_at = next_removal.expect("the drained version is scheduled");
            assert!(
                remove_at >= confirmed_at + chrono::TimeDelta::seconds(300),
                "the deadline is a full drain delay after the decision, not before it",
            );
            assert_eq!(
                scheduling.calls.field_managers(VERSION),
                vec!["restate-operator/remove-version-at"],
                "a deadline, and nothing else",
            );

            // the deadline has passed, and the second answer still leaves it out
            let harness = harness(
                vec![version(
                    "dp_gone",
                    Some(chrono::Utc::now() - chrono::TimeDelta::seconds(1)),
                )],
                vec![],
            );
            let second = SecondAnswer::empty();
            let CleanupOutcome { abandoned, .. } = cleanup_old_replicasets(
                NAMESPACE,
                &harness.ctx,
                &harness.rs_api,
                RSD_UID,
                &rsd,
                CleanupMode::Rollout,
                &DeploymentUsageMap::new(),
                None,
                second.confirm(),
            )
            .await
            .expect("cleanup succeeds");

            assert!(abandoned.is_empty(), "there is no work to walk over");
            assert_eq!(
                harness.calls.matching(VERSION),
                vec![
                    scale_of(VERSION),
                    format!("DELETE /apis/apps/v1/namespaces/{NAMESPACE}/replicasets/{VERSION}"),
                ],
                "scaled down and torn down at the deadline, with no new deadline",
            );
            assert_eq!(
                harness.calls.matching("/deployments/"),
                Vec::<String>::new(),
                "a deployment the answer does not list is not deregistered",
            );
        }

        /// Only a pass that would act on an absence pays for the second query: a
        /// never-registered version is removed at once without one.
        #[tokio::test]
        async fn a_second_query_is_taken_only_when_something_depends_on_an_absence() {
            let rsd = rsd_with_history(0, false, false);
            let harness = harness(
                vec![
                    version("dp_live", None),
                    // never registered: no deployment-id annotation at all
                    versioned(NEVER_REGISTERED, None, None, 3, OLDER),
                ],
                vec![],
            );
            let second = SecondAnswer::empty();

            let CleanupOutcome { blocking, .. } = cleanup_old_replicasets(
                NAMESPACE,
                &harness.ctx,
                &harness.rs_api,
                RSD_UID,
                &rsd,
                CleanupMode::Rollout,
                &usage_of("dp_live", active_row()),
                None,
                second.confirm(),
            )
            .await
            .expect("cleanup succeeds");

            assert_eq!(
                second.asked(),
                0,
                "a never-registered version is not an absence anyone could confirm",
            );
            assert_eq!(blocking.len(), 1, "the registered version is left alone");
            assert_eq!(
                harness.calls.matching(NEVER_REGISTERED),
                vec![
                    scale_of(NEVER_REGISTERED),
                    format!(
                        "DELETE /apis/apps/v1/namespaces/{NAMESPACE}/replicasets/{NEVER_REGISTERED}"
                    ),
                ],
                "and the never-registered one is removed on this pass",
            );
        }

        /// The version moves on: a registered version whose deadline has passed and which
        /// both answers still leave out is removed without being deregistered.
        #[tokio::test]
        async fn a_version_that_was_never_registered_is_removed_at_once() {
            // no revision history to keep it for, so the teardown lands in this pass
            let rsd = rsd_with_history(0, false, false);
            let harness = harness(vec![unregistered_version()], vec![]);

            let second = SecondAnswer::empty();
            let CleanupOutcome {
                blocking,
                next_removal,
                ..
            } = cleanup_old_replicasets(
                NAMESPACE,
                &harness.ctx,
                &harness.rs_api,
                RSD_UID,
                &rsd,
                CleanupMode::Rollout,
                // not even an empty answer makes a difference: with no recorded id there is
                // nothing that could have been left out
                &DeploymentUsageMap::new(),
                None,
                second.confirm(),
            )
            .await
            .expect("cleanup succeeds");

            assert_eq!(second.asked(), 0);
            assert!(blocking.is_empty());
            assert_eq!(next_removal, None, "no drain delay is waited out");
            assert_eq!(
                harness.calls.matching(VERSION),
                vec![
                    scale_of(VERSION),
                    format!("DELETE /apis/apps/v1/namespaces/{NAMESPACE}/replicasets/{VERSION}"),
                ],
            );
            assert_eq!(
                harness.calls.matching("/deployments/"),
                Vec::<String>::new(),
                "there is no registration to deregister",
            );
        }
    }
}
