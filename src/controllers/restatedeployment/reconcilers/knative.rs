use std::collections::BTreeMap;
use std::time::Duration;

use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::api::{Api, DeleteParams, Patch, PatchParams, PropagationPolicy};
use kube::runtime::reflector::ObjectRef;
use kube::{Resource, ResourceExt};
use serde_json::json;
use tracing::*;
use url::Url;

use crate::controllers::restatedeployment::controller::{
    Context, RESTATE_DEPLOYMENT_ID_ANNOTATION,
};
use crate::controllers::restatedeployment::reconcilers::replicaset::generate_pod_template_hash;
use crate::resources::knative::{
    Configuration, ConfigurationSpec, ConfigurationTemplate, ConfigurationTemplateMetadata,
    ConfigurationTemplateSpec, ConfigurationTemplateSpecContainers,
    ConfigurationTemplateSpecContainersPorts, ConfigurationTemplateSpecContainersReadinessProbe,
    ConfigurationTemplateSpecContainersReadinessProbeTcpSocket, Revision, Route, RouteSpec,
    RouteTraffic,
};

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
    namespace: &str,
    status: &mut crate::resources::restatedeployments::RestateDeploymentStatus,
) -> Result<Option<chrono::DateTime<chrono::Utc>>> {
    debug!(
        namespace = %namespace,
        name = %rsd.name_any(),
        "Reconciling Knative deployment"
    );

    // Determine new tag for this deployment and hence the Configuration and Route names
    let tag = determine_tag(rsd)?;
    trace!(tag = %tag, "Determined deployment tag");

    // Early status update based on tag change
    let config_name = format!("{}-{}", rsd.name_any(), &tag);
    let route_name = format!("{}-{}", rsd.name_any(), &tag);

    if status.knative.is_none() {
        status.knative = Some(KnativeDeploymentStatus::default());
    }

    let current_config_name = status
        .knative
        .as_ref()
        .and_then(|s| s.configuration_name.as_ref())
        .cloned(); // .cloned() creates an Option<String> from Option<&String>

    // If a new tag is detected, immediately update status fields to reflect the change
    // This prevents showing stale information from the previous generation
    if current_config_name
        .as_ref()
        .map_or(true, |name| name != &config_name)
    {
        info!(
            old_config_name = ?current_config_name.as_ref(),
            new_config_name = %config_name,
            "New tag detected, starting a new Restate deployment."
        );
        status.knative = Some(KnativeDeploymentStatus {
            configuration_name: Some(config_name.clone()),
            route_name: Some(route_name.clone()),
            latest_revision: None,
            url: None,
        });
        status.deployment_id = None;
        status.replicas = 0;
        status.desired_replicas = Some(0);
        status.ready_replicas = Some(0);
        status.available_replicas = Some(0);
        status.unavailable_replicas = Some(0);
    }

    // Apply Configuration for current tag
    let config = reconcile_configuration(ctx, rsd, &config_name, namespace, &tag).await?;
    debug!(configuration = %config.name_any(), "Configuration reconciled");

    // Apply Route for current tag
    let route = reconcile_route(ctx, rsd, &route_name, namespace, &config).await?;
    debug!(route = %route.name_any(), "Route reconciled");

    // Check for up-to-date status information
    if config.metadata.generation.unwrap_or(0)
        != config
            .status
            .as_ref()
            .and_then(|s| s.observed_generation)
            .unwrap_or(0)
    {
        return Err(Error::ConfigurationNotReady {
            message: format!(
                "Configuration {} observed generation mismatch",
                config.name_any()
            ),
            reason: "ObservedGenerationMismatch".into(),
            requeue_after: Some(Duration::from_millis(500)),
        });
    }

    // Get the latest created revision (to observe the rollout)
    let latest_revision = config
        .status
        .as_ref()
        .and_then(|s| s.latest_created_revision_name.as_ref())
        .ok_or_else(|| Error::ConfigurationNotReady {
            message: format!("Configuration {} has no revision(s) yet", config.name_any()),
            reason: "RevisionNotCreated".into(),
            requeue_after: Some(Duration::from_secs(5)),
        })?
        .clone();
    debug!(revision = %latest_revision, "Latest revision created");

    // Update status with latest_revision from Configuration
    if let Some(knative_status) = status.knative.as_mut() {
        knative_status.latest_revision = Some(latest_revision.clone());
    }

    // Fetch the full Revision object for replica counts
    let revision = ctx
        .revision_store
        .get(&ObjectRef::new(&latest_revision).within(namespace))
        .ok_or_else(|| Error::ConfigurationNotReady {
            message: format!("Revision {} not found in store", latest_revision),
            reason: "RevisionNotFound".into(),
            requeue_after: Some(Duration::from_secs(1)),
        })?;

    // Update status with replica counts and other revision details
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
            // Revision has no status yet, or scaled to zero
            (0, 0, Some(0), Some(0), Some(0))
        };

    status.replicas = actual;
    status.desired_replicas = Some(desired);
    status.ready_replicas = ready_replicas;
    status.available_replicas = available_replicas;
    status.unavailable_replicas = unavailable_replicas;

    // Wait for Revision to be ready before registration
    check_revision_ready(&revision)?;
    debug!(revision = %revision.name_any(), "Revision is ready");

    // Wait for Route to be ready before registration
    check_route_ready(&route, &latest_revision)?;
    debug!(route = %route.name_any(), "Route is ready");

    // Update status with URL from Route
    if let Some(knative_status) = status.knative.as_mut() {
        knative_status.url = route.status.as_ref().and_then(|s| s.url.clone());
    }

    // Register or lookup deployment
    let deployment_id = register_or_lookup_deployment(ctx, rsd, namespace, &config, &route).await?;
    debug!(deployment_id = %deployment_id, "Deployment registered/looked up");

    // Update status with deployment ID
    status.deployment_id = Some(deployment_id.clone());

    // Annotate Configuration with deployment metadata
    annotate_configuration(ctx, namespace, &config, &deployment_id).await?;

    // Cleanup old Configurations (mirrors ReplicaSet cleanup pattern)
    let deployments = rsd.list_deployments(ctx).await?;
    let rsd_uid = rsd
        .uid()
        .ok_or_else(|| Error::InvalidRestateConfig("RestateDeployment must have UID".into()))?;

    let (_, next_removal) =
        cleanup_old_configurations(namespace, ctx, &rsd_uid, rsd, &deployments, Some(&tag)).await?;

    Ok(next_removal)
}

/// Determine the tag for this deployment
/// If tag is explicitly specified, use it (DNS-safe)
/// Otherwise, generate from template hash
fn determine_tag(rsd: &RestateDeployment) -> Result<String> {
    if let Some(tag) = rsd.spec.knative.as_ref().and_then(|k| k.tag.as_ref()) {
        // User-specified tag (enables in-place updates)
        Ok(tag.clone())
    } else {
        // Default: template hash (enables versioned updates)
        let pod_template = serde_json::to_string(&rsd.spec.template)?;
        Ok(generate_pod_template_hash(rsd, &pod_template))
    }
}

/// Reconcile Knative Configuration resource
async fn reconcile_configuration(
    ctx: &Context,
    rsd: &RestateDeployment,
    name: &str,
    namespace: &str,
    tag: &str,
) -> Result<Configuration> {
    // Build Configuration spec
    let config_spec = build_configuration_spec(rsd, name, namespace, tag)?;

    debug!(
        configuration_name = %name,
        namespace = %namespace,
        tag = %tag,
        "Applying Knative Configuration"
    );

    // Apply Configuration using server-side apply
    let config_api: Api<Configuration> = Api::namespaced(ctx.client.clone(), namespace);
    let params = PatchParams::apply("restate-operator").force();

    let config = config_api
        .patch(name, &params, &Patch::Apply(&config_spec))
        .await?;

    Ok(config)
}

/// Build Configuration resource specification
fn build_configuration_spec(
    rsd: &RestateDeployment,
    name: &str,
    namespace: &str,
    _tag: &str,
) -> Result<Configuration> {
    let knative_spec = rsd
        .spec
        .knative
        .as_ref()
        .ok_or_else(|| Error::InvalidRestateConfig("Missing knative spec".into()))?;

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

    // Deserialize the PodTemplateSpec.spec into ConfigurationTemplateSpec
    // This allows users to set any field in the Knative Revision template (e.g. timeoutSeconds, serviceAccountName)
    let mut configuration_template_spec: ConfigurationTemplateSpec =
        if let Some(spec) = &rsd.spec.template.spec {
            serde_json::from_value(spec.clone()).map_err(|e| {
                Error::InvalidRestateConfig(format!("Failed to parse pod template spec: {}", e))
            })?
        } else {
            ConfigurationTemplateSpec::default()
        };

    // Ensure Restate port and validate for Knative compatibility
    validate_knative_containers(&configuration_template_spec.containers)?;
    ensure_restate_port(&mut configuration_template_spec.containers)?;
    ensure_readiness_probe(&mut configuration_template_spec.containers)?;

    // Create owner reference
    let owner_reference = rsd.controller_owner_ref(&()).unwrap();

    let mut config_annotations = BTreeMap::new();
    config_annotations.insert(RESTATE_DEPLOYMENT_ANNOTATION.to_string(), rsd.name_any());
    config_annotations.insert(RESTATE_TAG_ANNOTATION.to_string(), _tag.to_string());

    let configuration_metadata = ObjectMeta {
        name: Some(name.to_string()),
        namespace: Some(namespace.to_string()),
        owner_references: Some(vec![owner_reference]),
        annotations: Some(config_annotations),
        labels: Some(BTreeMap::from([(
            "app.kubernetes.io/managed-by".to_string(),
            "restate-operator".to_string(),
        )])),
        ..Default::default()
    };

    let configuration_template_metadata = ConfigurationTemplateMetadata {
        annotations: Some(annotations),
        labels: Some(BTreeMap::from([(
            "app.kubernetes.io/managed-by".to_string(),
            "restate-operator".to_string(),
        )])),
        ..Default::default()
    };

    let configuration_spec = ConfigurationSpec {
        template: Some(ConfigurationTemplate {
            metadata: Some(configuration_template_metadata),
            spec: Some(configuration_template_spec),
        }),
    };

    Ok(Configuration {
        metadata: configuration_metadata,
        spec: configuration_spec,
        status: None,
    })
}

/// Validate containers for Knative compatibility
/// Knative has strict requirements that differ from standard Kubernetes
fn validate_knative_containers(containers: &[ConfigurationTemplateSpecContainers]) -> Result<()> {
    for (idx, container) in containers.iter().enumerate() {
        // Validate port names
        // Knative only allows port names: empty, "h2c", or "http1"
        if let Some(ports) = &container.ports {
            for port in ports {
                if let Some(port_name) = &port.name {
                    if port_name != "h2c" && port_name != "http1" {
                        return Err(Error::InvalidRestateConfig(format!(
                            "Container {} has invalid port name '{}'. Knative only allows port names: 'h2c' or 'http1'. \
                                Please update the port name in spec.template.spec.containers[{}].ports",
                            container.name.as_deref().unwrap_or("unknown"),
                            port_name,
                            idx
                        )));
                    }
                }
            }
        }
    }

    Ok(())
}

/// Ensure containers have Restate port 9080 with h2c protocol
/// If port 9080 doesn't exist, add it. Otherwise, leave ports as-is.
fn ensure_restate_port(containers: &mut Vec<ConfigurationTemplateSpecContainers>) -> Result<()> {
    for container in containers.iter_mut() {
        let has_restate_port = if let Some(ports) = &container.ports {
            ports.iter().any(|p| p.container_port == 9080)
        } else {
            false
        };

        if !has_restate_port {
            let restate_port = ConfigurationTemplateSpecContainersPorts {
                name: Some("h2c".to_string()),
                container_port: 9080,
                protocol: Some("TCP".to_string()),
            };

            if let Some(ports) = &mut container.ports {
                ports.push(restate_port);
            } else {
                container.ports = Some(vec![restate_port]);
            }
        }
    }

    Ok(())
}

/// Ensure containers have a readiness probe configured
/// If no readiness probe exists, inject a TCP probe on port 9080 (Restate ingress port)
/// with quick timing parameters suitable for fast-starting Restate SDK services.
/// Preserves user-specified probes without modification.
fn ensure_readiness_probe(containers: &mut Vec<ConfigurationTemplateSpecContainers>) -> Result<()> {
    for container in containers.iter_mut() {
        // Check if readiness probe already exists
        if container.readiness_probe.is_some() {
            // User has explicitly configured a probe, preserve it
            continue;
        }

        // Inject default TCP probe on port 9080 (the Restate ingress port)
        // These timing parameters are optimized for fast-starting Restate SDK services:
        // - initialDelaySeconds: 2 - Lightweight services start quickly
        // - periodSeconds: 5 - Quick feedback during startup
        container.readiness_probe = Some(ConfigurationTemplateSpecContainersReadinessProbe {
            tcp_socket: Some(ConfigurationTemplateSpecContainersReadinessProbeTcpSocket {
                port: Some(IntOrString::Int(9080)),
                ..Default::default()
            }),
            initial_delay_seconds: Some(2),
            period_seconds: Some(5),
            timeout_seconds: Some(1),
            success_threshold: Some(1),
            failure_threshold: Some(3),
            ..Default::default()
        });
    }

    Ok(())
}

/// Reconcile Knative Route resource
async fn reconcile_route(
    ctx: &Context,
    rsd: &RestateDeployment,
    name: &str,
    namespace: &str,
    config: &Configuration,
) -> Result<Route> {
    let config_name = config.name_any();

    debug!(
        route_name = %name,
        namespace = %namespace,
        configuration = %config_name,
        "Applying Knative Route"
    );

    // Build Route spec
    let route_obj = build_route_spec(rsd, name, namespace, &config_name, config)?;

    // Apply Route using server-side apply
    let route_api: Api<Route> = Api::namespaced(ctx.client.clone(), namespace);
    let params = PatchParams::apply("restate-operator").force();

    let route = route_api
        .patch(name, &params, &Patch::Apply(&route_obj))
        .await?;

    Ok(route)
}

/// Build Route resource specification
fn build_route_spec(
    rsd: &RestateDeployment,
    name: &str,
    namespace: &str,
    config_name: &str,
    config: &Configuration,
) -> Result<Route> {
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

    let route_metadata = ObjectMeta {
        name: Some(name.to_string()),
        namespace: Some(namespace.to_string()),
        owner_references: Some(vec![owner_reference]),
        annotations: Some(route_annotations),
        labels: Some(BTreeMap::from([
            (
                "app.kubernetes.io/managed-by".to_string(),
                "restate-operator".to_string(),
            ),
            (
                "networking.knative.dev/visibility".to_string(),
                "cluster-local".to_string(),
            ),
        ])),
        ..Default::default()
    };

    let traffic_target = RouteTraffic {
        configuration_name: Some(config_name.to_string()),
        latest_revision: Some(true),
        percent: Some(100),
        ..Default::default()
    };

    let route_spec = RouteSpec {
        traffic: Some(vec![traffic_target]),
    };

    Ok(Route {
        metadata: route_metadata,
        spec: route_spec,
        status: None,
    })
}

/// Check if Route is ready to serve traffic
fn check_route_ready(route: &Route, expected_revision: &str) -> Result<()> {
    // Check if Route has Ready=True condition
    if let Some(conditions) = route.status.as_ref().and_then(|s| s.conditions.as_ref()) {
        if let Some(ready_condition) = conditions.iter().find(|c| c.type_ == "Ready") {
            if ready_condition.status != "True" {
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
    }

    // Check if the expected revision is in the traffic block
    let traffic_rolled_out = route
        .status
        .as_ref()
        .and_then(|s| s.traffic.as_ref())
        .map(|traffic| {
            traffic.iter().any(|t| {
                t.revision_name.as_deref() == Some(expected_revision)
                // We could also check percent here, but existence is a strong enough signal for now
                // given we only configure 100% traffic to one revision
            })
        })
        .unwrap_or(false);

    if !traffic_rolled_out {
        return Err(Error::RouteNotReady {
            message: format!(
                "Route {} is not routing traffic to revision {}",
                route.name_any(),
                expected_revision
            ),
            reason: "TrafficNotRolledOut".into(),
            requeue_after: Some(Duration::from_secs(2)),
        });
    }

    Ok(())
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
            debug!(
                deployment_id = %deployment_id,
                "Found existing deployment ID in Configuration annotation"
            );
            return Ok(deployment_id.clone());
        }
    }

    // Build endpoint URL from Route default URL
    let url_str = route
        .status
        .as_ref()
        .and_then(|s| s.url.as_ref())
        .ok_or_else(|| Error::RouteNotReady {
            message: format!("Route {} does not have URL in status", route.name_any()),
            reason: "RouteURLNotReady".into(),
            requeue_after: Some(Duration::from_secs(5)),
        })?;

    let url = Url::parse(url_str)?;

    let deployment_id = rsd
        .register_service_with_restate(ctx, &url, rsd.spec.restate.use_http11.as_ref().cloned())
        .await?;

    Ok(deployment_id)
}

/// Annotate Configuration with deployment metadata
async fn annotate_configuration(
    ctx: &Context,
    namespace: &str,
    config: &Configuration,
    deployment_id: &str,
) -> Result<()> {
    // Check if the configuration already has the correct annotations
    if let Some(annotations) = &config.metadata.annotations {
        let current_id = annotations.get(RESTATE_DEPLOYMENT_ID_ANNOTATION);

        if current_id == Some(&deployment_id.to_string()) {
            debug!("Configuration already annotated with deployment ID, skipping patch");
            return Ok(());
        }
    }

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
                        RESTATE_REGISTERED_AT_ANNOTATION: chrono::Utc::now().to_rfc3339(),
                    }
                }
            })),
        )
        .await?;

    Ok(())
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
    active_tag: Option<&str>,
) -> Result<(i32, Option<chrono::DateTime<chrono::Utc>>)> {
    // List all Configurations in the namespace
    let config_api: Api<Configuration> = Api::namespaced(ctx.client.clone(), namespace);
    let all_configs = config_api.list(&Default::default()).await?;

    // Filter to Configurations owned by this RestateDeployment with tags != active_tag
    let mut configurations: Vec<Configuration> = all_configs
        .items
        .into_iter()
        .filter(|config| {
            // Skip if no tag annotation
            let Some(tag) = get_configuration_tag(config) else {
                return false;
            };

            // Skip current version if a active_tag is provided and matches
            if let Some(active) = active_tag {
                if tag == active {
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
