use std::collections::BTreeMap;
use std::time::Duration;

use kube::api::{Api, DeleteParams, Patch, PatchParams, PropagationPolicy};
use kube::{Resource, ResourceExt};
use serde_json::json;
use tracing::*;

use crate::controllers::restatedeployment::controller::{
    Context, RESTATE_DEPLOYMENT_ID_ANNOTATION,
};
use crate::controllers::restatedeployment::reconcilers::replicaset::generate_pod_template_hash;
use crate::resources::knative::{Configuration, Revision, Route};
use crate::resources::restatedeployments::{KnativeDeploymentStatus, RestateDeployment};
use crate::{Error, Result};

const RESTATE_TAG_ANNOTATION: &str = "restate.dev/tag";
const RESTATE_REGISTERED_AT_ANNOTATION: &str = "restate.dev/registered-at";
const RESTATE_DEPLOYMENT_ANNOTATION: &str = "restate.dev/deployment";

/// Main Knative reconciliation function
/// Updates status in-place and returns the next_removal time for requeue logic
pub async fn reconcile_knative(
    ctx: &Context,
    rsd: &RestateDeployment,
    status: &mut crate::resources::restatedeployments::RestateDeploymentStatus,
) -> Result<Option<chrono::DateTime<chrono::Utc>>> {
    let namespace = rsd.namespace().ok_or_else(|| {
        Error::InvalidRestateDeployment("RestateDeployment must have a namespace".into())
    })?;

    info!(
        namespace = %namespace,
        name = %rsd.name_any(),
        "Reconciling Knative deployment"
    );

    // Step 1: Determine current tag
    let current_tag = determine_tag(rsd)?;
    info!(tag = %current_tag, "Determined deployment tag");

    // Step 2: Reconcile Configuration for current tag
    let config = reconcile_configuration(ctx, rsd, &namespace, &current_tag).await?;
    debug!(configuration = %config.name_any(), "Configuration reconciled");

    // Step 3: Reconcile Route for current tag
    let route = reconcile_route(ctx, rsd, &namespace, &current_tag, &config).await?;
    debug!(route = %route.name_any(), "Route reconciled");

    // Step 4: Get the latest created revision (observe rollout eagerly)
    let latest_revision = config
        .status
        .as_ref()
        .and_then(|s| s.latest_created_revision_name.as_ref())
        .ok_or_else(|| Error::ConfigurationNotReady {
            message: format!(
                "Configuration {} does not have latestCreatedRevisionName in status",
                config.name_any()
            ),
            reason: "RevisionNotCreated".into(),
            requeue_after: Some(Duration::from_secs(5)),
        })?
        .clone();
    debug!(revision = %latest_revision, "Latest revision created");

    // Fetch the full Revision object for replica counts
    let revision_api: Api<Revision> = Api::namespaced(ctx.client.clone(), &namespace);
    let revision = revision_api.get(&latest_revision).await?;

    // Step 4.5: Wait for Revision to be ready before registration
    check_revision_ready(&revision)?;
    debug!(revision = %revision.name_any(), "Revision is ready");

    // Step 4.6: Wait for Route to be ready before registration
    check_route_ready(&route)?;
    debug!(route = %route.name_any(), "Route is ready");

    // Step 5: Register or lookup deployment
    let deployment_id =
        register_or_lookup_deployment(ctx, rsd, &namespace, &config, &route).await?;
    info!(deployment_id = %deployment_id, "Deployment registered/looked up");

    // Step 6: Annotate Configuration with deployment metadata
    annotate_configuration(ctx, &namespace, &config, &deployment_id, &current_tag).await?;

    // Step 7: Update RestateDeployment status in-place
    update_status(
        status,
        &current_tag,
        &config,
        &route,
        &revision,
        &deployment_id,
    );

    // Step 8: Cleanup old Configurations (mirrors ReplicaSet cleanup pattern)
    let deployments = rsd.list_deployments(ctx).await?;
    let rsd_uid = rsd
        .uid()
        .ok_or_else(|| Error::InvalidRestateDeployment("RestateDeployment must have UID".into()))?;

    let (_, next_removal) = cleanup_old_configurations(
        &namespace,
        ctx,
        &rsd_uid,
        rsd,
        &deployments,
        Some(&current_tag),
    )
    .await?;

    Ok(next_removal)
}

/// Determine the tag for this deployment
/// If tag is explicitly specified, use it (DNS-safe)
/// Otherwise, generate from template hash
fn determine_tag(rsd: &RestateDeployment) -> Result<String> {
    if let Some(tag) = rsd.spec.knative.as_ref().and_then(|k| k.tag.as_ref()) {
        // User-specified tag (enables in-place updates)
        Ok(dns_safe_tag(tag))
    } else {
        // Default: template hash (enables versioned updates)
        let pod_template = serde_json::to_string(&rsd.spec.template)?;
        Ok(generate_pod_template_hash(rsd, &pod_template))
    }
}

/// Convert tag to DNS-safe format (replace dots with hyphens)
fn dns_safe_tag(tag: &str) -> String {
    tag.replace('.', "-")
}

/// Reconcile Knative Configuration resource
async fn reconcile_configuration(
    ctx: &Context,
    rsd: &RestateDeployment,
    namespace: &str,
    tag: &str,
) -> Result<Configuration> {
    let config_name = format!("{}-{}", rsd.name_any(), tag);

    // Build Configuration spec
    let config_spec = build_configuration_spec(rsd, &config_name, namespace, tag)?;

    info!(
        configuration_name = %config_name,
        namespace = %namespace,
        tag = %tag,
        "Applying Knative Configuration"
    );

    // Apply Configuration using server-side apply
    let config_api: Api<Configuration> = Api::namespaced(ctx.client.clone(), namespace);
    let params = PatchParams::apply("restate-operator").force();

    let config = config_api
        .patch(&config_name, &params, &Patch::Apply(&config_spec))
        .await?;

    Ok(config)
}

/// Build Configuration resource specification
fn build_configuration_spec(
    rsd: &RestateDeployment,
    name: &str,
    namespace: &str,
    _tag: &str,
) -> Result<serde_json::Value> {
    let knative_spec = rsd
        .spec
        .knative
        .as_ref()
        .ok_or_else(|| Error::InvalidRestateDeployment("Missing knative spec".into()))?;

    // Build revision template annotations
    let mut annotations = BTreeMap::new();

    // Step 1: Start with user-provided revision annotations
    if let Some(user_annotations) = &knative_spec.revision_annotations {
        annotations.extend(user_annotations.clone());
    }

    // Step 2: Apply operator-managed autoscaling annotations (overriding user values)
    if let Some(min) = knative_spec.min_scale {
        annotations.insert(
            "autoscaling.knative.dev/min-scale".to_string(),
            min.to_string(),
        );
    } else {
        // Default to scale-to-zero
        annotations.insert(
            "autoscaling.knative.dev/min-scale".to_string(),
            "0".to_string(),
        );
    }

    if let Some(max) = knative_spec.max_scale {
        annotations.insert(
            "autoscaling.knative.dev/max-scale".to_string(),
            max.to_string(),
        );
    }

    if let Some(target) = knative_spec.target {
        annotations.insert(
            "autoscaling.knative.dev/target".to_string(),
            target.to_string(),
        );
    }

    // Step 3: Always set parent deployment tracking (operator-managed)
    annotations.insert(RESTATE_DEPLOYMENT_ANNOTATION.to_string(), rsd.name_any());

    // Get container spec from template
    let containers = rsd
        .spec
        .template
        .spec
        .as_ref()
        .and_then(|spec| spec.get("containers"))
        .ok_or_else(|| Error::InvalidRestateDeployment("Missing containers in template".into()))?;

    // Ensure Restate port and validate for Knative compatibility
    let mut containers_array: Vec<serde_json::Value> = serde_json::from_value(containers.clone())?;
    validate_knative_containers(&containers_array)?;
    ensure_restate_port(&mut containers_array)?;
    ensure_readiness_probe(&mut containers_array)?;

    // Create owner reference
    let owner_reference = rsd.controller_owner_ref(&()).unwrap();

    let mut config_annotations = BTreeMap::new();
    config_annotations.insert(RESTATE_DEPLOYMENT_ANNOTATION.to_string(), rsd.name_any());

    Ok(json!({
        "apiVersion": "serving.knative.dev/v1",
        "kind": "Configuration",
        "metadata": {
            "name": name,
            "namespace": namespace,
            "ownerReferences": [owner_reference],
            "annotations": config_annotations,
            "labels": {
                "app.kubernetes.io/managed-by": "restate-operator"
            }
        },
        "spec": {
            "template": {
                "metadata": {
                    "annotations": annotations,
                    "labels": {
                        "app.kubernetes.io/managed-by": "restate-operator"
                    }
                },
                "spec": {
                    "containers": containers_array,
                }
            }
        }
    }))
}

/// Validate containers for Knative compatibility
/// Knative has strict requirements that differ from standard Kubernetes
fn validate_knative_containers(containers: &[serde_json::Value]) -> Result<()> {
    for (idx, container) in containers.iter().enumerate() {
        // Validate port names
        // Knative only allows port names: empty, "h2c", or "http1"
        if let Some(ports) = container.get("ports").and_then(|p| p.as_array()) {
            for port in ports {
                if let Some(port_name) = port.get("name").and_then(|n| n.as_str()) {
                    if port_name != "h2c" && port_name != "http1" {
                        return Err(Error::InvalidRestateDeployment(
                            format!(
                                "Container {} has invalid port name '{}'. Knative only allows port names: 'h2c' or 'http1'. \
                                Please update the port name in spec.template.spec.containers[{}].ports",
                                container.get("name").and_then(|n| n.as_str()).unwrap_or("unknown"),
                                port_name,
                                idx
                            )
                        ));
                    }
                }
            }
        }
    }

    Ok(())
}

/// Ensure containers have Restate port 9080 with h2c protocol
/// If port 9080 doesn't exist, add it. Otherwise, leave ports as-is.
fn ensure_restate_port(containers: &mut [serde_json::Value]) -> Result<()> {
    for container in containers.iter_mut() {
        let ports = container.get_mut("ports").and_then(|p| p.as_array_mut());

        if let Some(ports_array) = ports {
            // Check if port 9080 already exists
            let has_restate_port = ports_array.iter().any(|port| {
                port.get("containerPort")
                    .and_then(|p| p.as_i64())
                    .map(|p| p == 9080)
                    .unwrap_or(false)
            });

            if !has_restate_port {
                // Add Restate port with h2c protocol
                ports_array.push(json!({
                    "name": "h2c",
                    "containerPort": 9080,
                    "protocol": "TCP"
                }));
            }
        } else {
            // No ports defined, create array with Restate port
            container.as_object_mut().unwrap().insert(
                "ports".to_string(),
                json!([{
                    "name": "h2c",
                    "containerPort": 9080,
                    "protocol": "TCP"
                }]),
            );
        }
    }

    Ok(())
}

/// Ensure containers have a readiness probe configured
/// If no readiness probe exists, inject a TCP probe on port 9080 (Restate ingress port)
/// with quick timing parameters suitable for fast-starting Restate SDK services.
/// Preserves user-specified probes without modification.
fn ensure_readiness_probe(containers: &mut [serde_json::Value]) -> Result<()> {
    for container in containers.iter_mut() {
        // Check if readiness probe already exists
        if container.get("readinessProbe").is_some() {
            // User has explicitly configured a probe, preserve it
            continue;
        }

        // Inject default TCP probe on port 9080 (the Restate ingress port)
        // These timing parameters are optimized for fast-starting Restate SDK services:
        // - initialDelaySeconds: 2 - Lightweight services start quickly
        // - periodSeconds: 5 - Quick feedback during startup
        container.as_object_mut().unwrap().insert(
            "readinessProbe".to_string(),
            json!({
                "tcpSocket": {
                    "port": 9080
                },
                "initialDelaySeconds": 2,
                "periodSeconds": 5,
                "timeoutSeconds": 1,
                "successThreshold": 1,
                "failureThreshold": 3
            }),
        );
    }

    Ok(())
}

/// Reconcile Knative Route resource
async fn reconcile_route(
    ctx: &Context,
    rsd: &RestateDeployment,
    namespace: &str,
    tag: &str,
    config: &Configuration,
) -> Result<Route> {
    let route_name = format!("{}-{}", rsd.name_any(), tag);
    let config_name = config.name_any();

    // Create owner reference
    // Set Configuration as owner to ensure cascading deletion
    // If Configuration is deleted, Route will be garbage collected
    let owner_reference = config.controller_owner_ref(&()).unwrap();

    // Build Route annotations
    let mut route_annotations = BTreeMap::new();

    // Start with user-provided route annotations
    if let Some(knative_spec) = &rsd.spec.knative {
        if let Some(user_annotations) = &knative_spec.route_annotations {
            route_annotations.extend(user_annotations.clone());
        }
    }

    // Operator-managed route annotations can override if needed
    route_annotations.insert(RESTATE_DEPLOYMENT_ANNOTATION.to_string(), rsd.name_any());

    let route_spec = json!({
        "apiVersion": "serving.knative.dev/v1",
        "kind": "Route",
        "metadata": {
            "name": route_name,
            "namespace": namespace,
            "ownerReferences": [owner_reference],
            "labels": {
                "app.kubernetes.io/managed-by": "restate-operator",
                "networking.knative.dev/visibility": "cluster-local"
            },
            "annotations": route_annotations
        },
        "spec": {
            "traffic": [{
                "configurationName": config_name,
                "latestRevision": true,
                "percent": 100,
            }]
        }
    });

    info!(
        route_name = %route_name,
        namespace = %namespace,
        tag = %tag,
        configuration = %config_name,
        "Applying Knative Route"
    );

    // Apply Route using server-side apply
    let route_api: Api<Route> = Api::namespaced(ctx.client.clone(), namespace);
    let params = PatchParams::apply("restate-operator").force();

    let route = route_api
        .patch(&route_name, &params, &Patch::Apply(&route_spec))
        .await?;

    Ok(route)
}

/// Check if Route is ready to serve traffic
fn check_route_ready(route: &Route) -> Result<()> {
    // Check if Route has Ready=True condition
    if let Some(conditions) = route.status.as_ref().and_then(|s| s.conditions.as_ref()) {
        if let Some(ready_condition) = conditions.iter().find(|c| c.type_ == "Ready") {
            if ready_condition.status == "True" {
                return Ok(());
            }

            return Err(Error::RouteNotReady {
                message: format!(
                    "Route {} is not ready: {}",
                    route.name_any(),
                    ready_condition.message
                ),
                reason: ready_condition.reason.clone(),
                requeue_after: Some(Duration::from_secs(5)),
            });
        }
    }

    // No conditions found - Route not reconciled yet
    Err(Error::RouteNotReady {
        message: format!("Route {} has no status conditions", route.name_any()),
        reason: "NoConditions".into(),
        requeue_after: Some(Duration::from_secs(5)),
    })
}

/// Check if Revision is ready to serve traffic
fn check_revision_ready(revision: &Revision) -> Result<()> {
    // Check if Revision has Ready=True condition
    if let Some(conditions) = revision.status.as_ref().and_then(|s| s.conditions.as_ref()) {
        if let Some(ready_condition) = conditions.iter().find(|c| c.type_ == "Ready") {
            if ready_condition.status == "True" {
                return Ok(());
            }

            return Err(Error::ConfigurationNotReady {
                message: format!(
                    "Revision {} is not ready: {}",
                    revision.name_any(),
                    ready_condition.message
                ),
                reason: ready_condition.reason.clone(),
                requeue_after: Some(Duration::from_secs(5)),
            });
        }
    }

    // No conditions found - Revision not reconciled yet
    Err(Error::ConfigurationNotReady {
        message: format!("Revision {} has no status conditions", revision.name_any()),
        reason: "NoConditions".into(),
        requeue_after: Some(Duration::from_secs(5)),
    })
}

/// Register deployment with Restate or lookup existing deployment ID
async fn register_or_lookup_deployment(
    ctx: &Context,
    rsd: &RestateDeployment,
    _namespace: &str,
    config: &Configuration,
    route: &Route,
) -> Result<String> {
    // Check if Configuration already has deployment-id annotation
    if let Some(annotations) = &config.metadata.annotations {
        if let Some(deployment_id) = annotations.get(RESTATE_DEPLOYMENT_ID_ANNOTATION) {
            info!(
                deployment_id = %deployment_id,
                "Found existing deployment ID in Configuration annotation"
            );
            return Ok(deployment_id.clone());
        }
    }

    // Build endpoint URL from Route default URL
    let url = route
        .status
        .as_ref()
        .and_then(|s| s.url.as_ref())
        .ok_or_else(|| Error::RouteNotReady {
            message: format!("Route {} does not have URL in status", route.name_any()),
            reason: "RouteURLNotReady".into(),
            requeue_after: Some(Duration::from_secs(5)),
        })?;

    info!(url = %url, "Registering new deployment with Restate");

    // Get admin URL
    let admin_url = rsd.spec.restate.register.admin_url(&ctx.rce_store)?;

    // Get bearer token if needed
    let bearer_token = rsd.spec.restate.register.bearer_token(
        &ctx.rce_store,
        &ctx.secret_store,
        &ctx.operator_namespace,
    )?;

    // Build registration request
    let mut request = ctx
        .http_client
        .request(reqwest::Method::POST, admin_url.join("/deployments")?)
        .json(&json!({
            "uri": url,
            "use_http_11": rsd.spec.restate.use_http11.unwrap_or(false),
        }));

    if let Some(token) = bearer_token {
        request = request.bearer_auth(token);
    }

    let response = request.send().await.map_err(Error::AdminCallFailed)?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.map_err(Error::AdminCallFailed)?;
        return Err(Error::RestateRegistrationFailed(format!(
            "Failed to register deployment: {} - {}",
            status, body
        )));
    }

    let response_json: serde_json::Value = response.json().await.map_err(Error::AdminCallFailed)?;
    let deployment_id = response_json
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::RestateRegistrationFailed("No deployment ID in response".into()))?
        .to_string();

    info!(
        deployment_id = %deployment_id,
        "Successfully registered deployment"
    );

    Ok(deployment_id)
}

/// Annotate Configuration with deployment metadata
async fn annotate_configuration(
    ctx: &Context,
    namespace: &str,
    config: &Configuration,
    deployment_id: &str,
    tag: &str,
) -> Result<()> {
    let config_name = config.name_any();

    let config_api: Api<Configuration> = Api::namespaced(ctx.client.clone(), namespace);
    let params = PatchParams::apply("restate-operator/deployment-registration").force();

    // Use patch_metadata to update only the metadata (annotations)
    // without affecting the spec or other fields.
    config_api
        .patch_metadata(
            &config_name,
            &params,
            &Patch::Apply(json!({
                "apiVersion": "serving.knative.dev/v1",
                "kind": "Configuration",
                "metadata": {
                    "annotations": {
                        RESTATE_DEPLOYMENT_ID_ANNOTATION: deployment_id,
                        RESTATE_TAG_ANNOTATION: tag,
                        RESTATE_REGISTERED_AT_ANNOTATION: chrono::Utc::now().to_rfc3339(),
                    }
                }
            })),
        )
        .await?;

    Ok(())
}

/// Update RestateDeployment status in-place
/// This populates the status fields without patching to avoid race conditions
/// The controller will apply the complete status in a single patch
fn update_status(
    status: &mut crate::resources::restatedeployments::RestateDeploymentStatus,
    current_tag: &str,
    config: &Configuration,
    route: &Route,
    revision: &Revision,
    deployment_id: &str,
) {
    let url = route.status.as_ref().and_then(|s| s.url.clone());

    let knative_status = KnativeDeploymentStatus {
        current_tag: Some(current_tag.to_string()),
        configuration_name: Some(config.name_any()),
        route_name: Some(route.name_any()),
        url,
        latest_revision: Some(revision.name_any()),
        deployment_id: Some(deployment_id.to_string()),
    };

    // Extract replica counts from Revision status
    // Note: When scaled to zero, Knative may omit these fields
    let (desired, actual, ready_replicas, available_replicas, unavailable_replicas) =
        if let Some(rev_status) = &revision.status {
            let desired = rev_status.desired_replicas.unwrap_or(0);
            let actual = rev_status.actual_replicas.unwrap_or(0);
            (
                desired,
                actual,
                Some(actual),
                Some(actual),
                Some((desired - actual).max(0)),
            )
        } else {
            // Revision has no status yet
            (0, 0, Some(0), Some(0), Some(0))
        };

    // Update status fields in-place
    status.knative = Some(knative_status);
    status.replicas = actual;
    status.desired_replicas = Some(desired);
    status.ready_replicas = ready_replicas;
    status.available_replicas = available_replicas;
    status.unavailable_replicas = unavailable_replicas;
    // Knative mode doesn't use label selectors (Knative manages pods directly)
    status.label_selector = None;
}

const RESTATE_REMOVE_VERSION_AT_ANNOTATION: &str = "restate.dev/remove-version-at";

/// Delete Configurations that are no longer needed
/// Mirrors the pattern from cleanup_old_replicasets() in replicaset.rs:156-399
#[allow(clippy::too_many_arguments)]
pub async fn cleanup_old_configurations(
    namespace: &str,
    ctx: &Context,
    rsd_uid: &str,
    rsd: &RestateDeployment,
    deployments: &std::collections::HashMap<String, bool>,
    current_tag: Option<&str>,
) -> Result<(i32, Option<chrono::DateTime<chrono::Utc>>)> {
    // List all Configurations in the namespace
    let config_api: Api<Configuration> = Api::namespaced(ctx.client.clone(), namespace);
    let all_configs = config_api.list(&Default::default()).await?;

    // Filter to Configurations owned by this RestateDeployment with tags != current_tag
    let mut configurations: Vec<Configuration> = all_configs
        .items
        .into_iter()
        .filter(|config| {
            // Skip if no tag annotation
            let Some(tag) = get_configuration_tag(config) else {
                return false;
            };

            // Skip current version if a current_tag is provided and matches
            if let Some(current) = current_tag {
                if tag == current {
                    return false;
                }
            }

            // Skip if already being deleted
            if config.metadata.deletion_timestamp.is_some() {
                return false;
            }

            // Must be owned by this RestateDeployment
            config.owner_references().iter().any(|reference| {
                reference.uid == rsd_uid && reference.kind == RestateDeployment::kind(&())
            })
        })
        .collect();

    // Sort configurations by creation time (newest first)
    configurations.sort_by(|a, b| {
        b.metadata
            .creation_timestamp
            .cmp(&a.metadata.creation_timestamp)
    });

    // keep track of how many configurations there are that are still in-use by restate (active services or invocations)
    let mut active_count = 0;
    // Keep track of how many zero-scaled configurations there are (for revision history limit)
    let mut historic_count = 0;
    let mut next_removal = None;

    let now = chrono::Utc::now();

    for config in configurations {
        let config_name = config.name_any();

        let config_deployment_id = config
            .metadata
            .annotations
            .as_ref()
            .and_then(|a| a.get(RESTATE_DEPLOYMENT_ID_ANNOTATION));

        // Skip active deployments
        let deployment = config_deployment_id
            .and_then(|config_deployment_id| deployments.get(config_deployment_id).cloned());
        let deployment_exists = deployment.is_some();
        let deployment_active = deployment.unwrap_or(false);

        if deployment_active {
            active_count += 1;

            if config
                .metadata
                .annotations
                .as_ref()
                .and_then(|a| a.get(RESTATE_REMOVE_VERSION_AT_ANNOTATION))
                .is_none()
            {
                // not scheduled for removal; all good.
                debug!(
                    "Keeping active Configuration {} in namespace {namespace}",
                    config_name,
                );
                continue;
            }

            debug!(
                "Unscheduling removal of active Configuration {} in namespace {namespace}",
                config_name,
            );

            // if we previously scheduled it for removal, but it now seems active, reset the timer by removing the annotation
            let config_api: Api<Configuration> = Api::namespaced(ctx.client.clone(), namespace);
            let params: PatchParams =
                PatchParams::apply("restate-operator/remove-version-at").force();
            config_api
                .patch_metadata(
                    &config_name,
                    &params,
                    &Patch::Apply(json!({
                        "apiVersion": Configuration::api_version(&()),
                        "kind": Configuration::kind(&()),
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

        let current_remove_at = config
            .metadata
            .annotations
            .as_ref()
            .and_then(|a| a.get(RESTATE_REMOVE_VERSION_AT_ANNOTATION))
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
                // we are past the remove at time, or the endpoint was removed by other means; can now delete it

                // Knative automatically scales down, so we skip the scale-to-zero step

                // If we are here, there is a zero-sized configuration which should be subject to the history limit
                if historic_count < rsd.spec.revision_history_limit {
                    historic_count += 1;
                    debug!(
                        "Keeping old Configuration {} in namespace {namespace} (within revision history limit: {}/{})",
                        config_name,
                        historic_count,
                        rsd.spec.revision_history_limit
                    );
                    // we haven't hit that limit yet, so we don't need to delete this configuration
                    continue;
                }

                if deployment_exists {
                    let config_deployment_id = config_deployment_id.unwrap();

                    debug!("Force-deleting Restate deployment {config_deployment_id} as its associated with old Configuration {config_name} in namespace {namespace}");

                    // Get admin URL and bearer token
                    let admin_url = rsd.spec.restate.register.admin_url(&ctx.rce_store)?;
                    let bearer_token = rsd.spec.restate.register.bearer_token(
                        &ctx.rce_store,
                        &ctx.secret_store,
                        &ctx.operator_namespace,
                    )?;

                    let mut request = ctx.http_client.request(
                        reqwest::Method::DELETE,
                        admin_url
                            .join(&format!("/deployments/{}?force=true", config_deployment_id))?,
                    );

                    if let Some(token) = bearer_token {
                        request = request.bearer_auth(token);
                    }

                    let resp = request.send().await.map_err(Error::AdminCallFailed)?;

                    // for idempotency we have to allow 404
                    if resp.status() != reqwest::StatusCode::NOT_FOUND {
                        let _ = resp.error_for_status().map_err(Error::AdminCallFailed)?;
                    }
                }

                // Delete Configuration and its associated Route
                delete_configuration(ctx, namespace, &config_name).await?;

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
                info!(
                    configuration = %config_name,
                    namespace = %namespace,
                    drain_delay = "5 minutes",
                    "Scheduling removal of old Configuration (after drain delay)"
                );

                let remove_at = chrono::Utc::now()
                    .checked_add_signed(chrono::TimeDelta::minutes(5)) // Same as ReplicaSet cleanup
                    .expect("remove_version_at in bounds");

                let config_api: Api<Configuration> = Api::namespaced(ctx.client.clone(), namespace);
                let params = PatchParams::apply("restate-operator/remove-version-at").force();

                config_api
                    .patch_metadata(
                        &config_name,
                        &params,
                        &Patch::Apply(json!({
                            "apiVersion": Configuration::api_version(&()),
                            "kind": Configuration::kind(&()),
                            "metadata": {
                                "annotations": {
                                    RESTATE_REMOVE_VERSION_AT_ANNOTATION: remove_at.to_rfc3339(),
                                }
                            }
                        })),
                    )
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

/// Get tag from Configuration annotation
fn get_configuration_tag(config: &Configuration) -> Option<String> {
    config
        .metadata
        .annotations
        .as_ref()
        .and_then(|a| a.get(RESTATE_TAG_ANNOTATION))
        .cloned()
}

/// Delete Configuration using Foreground cascading deletion
/// This ensures the dependent Route is fully cleaned up before the Configuration is removed
async fn delete_configuration(ctx: &Context, namespace: &str, config_name: &str) -> Result<()> {
    debug!(
        configuration = %config_name,
        namespace = %namespace,
        "Deleting old Configuration (Foreground Cascading)"
    );
    let config_api: Api<Configuration> = Api::namespaced(ctx.client.clone(), namespace);

    // Use Foreground cascading deletion to ensure Route is cleaned up first
    let dp = DeleteParams {
        propagation_policy: Some(PropagationPolicy::Foreground),
        ..Default::default()
    };

    config_api.delete(config_name, &dp).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dns_safe_tag() {
        assert_eq!(dns_safe_tag("v1.0"), "v1-0");
        assert_eq!(dns_safe_tag("v1.2.3"), "v1-2-3");
        assert_eq!(dns_safe_tag("stable"), "stable");
    }
}
