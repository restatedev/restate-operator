use std::collections::{BTreeMap, BTreeSet};

use k8s_openapi::api::apps::v1::ReplicaSet;
use k8s_openapi::api::core::v1::{PodSpec, Service, ServicePort, ServiceSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;

use kube::api::{Api, DeleteParams, Patch, PatchParams};
use kube::runtime::events::{Event, EventType};
use kube::runtime::reflector::ObjectRef;
use kube::{Resource, ResourceExt};
use tracing::*;

use crate::controllers::restatedeployment::controller::{Context, validate_replica_set_status};
use crate::resources::restatedeployments::{DeploymentMode, RestateDeployment};
use crate::{Error, Result};

// Default port for Restate services
const DEFAULT_RESTATE_PORT: i32 = 9080;

/// Create or update a Service for a specific version of the RestateDeployment
pub async fn reconcile_service(
    namespace: &str,
    svc_api: &Api<Service>,
    versioned_name: &str,
    selector: BTreeMap<String, String>,
    labels: BTreeMap<String, String>,
    annotations: BTreeMap<String, String>,
    rs: &ReplicaSet,
) -> Result<Service> {
    // Determine the port to expose
    let port = find_service_port(
        rs.spec
            .as_ref()
            .and_then(|s| s.template.as_ref())
            .and_then(|t| t.spec.as_ref()),
    );

    // Create service ports
    let service_ports = vec![ServicePort {
        name: Some("restate".to_string()),
        port: 9080, // always expose 9080, irrelevant of the target
        protocol: Some("TCP".to_string()),
        target_port: Some(IntOrString::Int(port)),
        ..Default::default()
    }];

    // Create service spec
    let service_spec = ServiceSpec {
        selector: Some(selector),
        ports: Some(service_ports),
        type_: Some("ClusterIP".to_string()),
        ..Default::default()
    };

    // Create service ownership reference (owned by the replicaset)
    let owner_reference = rs.controller_owner_ref(&()).unwrap();

    // Create service metadata
    let metadata = ObjectMeta {
        name: Some(versioned_name.to_owned()),
        namespace: Some(namespace.to_owned()),
        // propagate labels and annotations from the owning rsd
        labels: Some(labels),
        annotations: Some(annotations),
        owner_references: Some(vec![owner_reference]),
        ..Default::default()
    };

    // Create the service object
    let service = Service {
        metadata,
        spec: Some(service_spec),
        status: None,
    };

    // Apply the service
    debug!("Applying Service {versioned_name} in namespace {namespace}",);

    let params: PatchParams = PatchParams::apply("restate-operator").force();
    svc_api
        .patch(versioned_name, &params, &Patch::Apply(&service))
        .await?;

    Ok(service)
}

/// Find the appropriate service port by examining containers
fn find_service_port(pod_spec: Option<&k8s_openapi::api::core::v1::PodSpec>) -> i32 {
    let mut all_ports = pod_spec
        .iter()
        .flat_map(|t| t.containers.iter())
        .flat_map(|c| c.ports.iter())
        .flat_map(|p| p.iter());

    let Some(first_port) = all_ports.next() else {
        // default to 9080 if there are no ports
        return DEFAULT_RESTATE_PORT;
    };

    if first_port.name.as_deref() == Some("restate") {
        return first_port.container_port;
    }

    if let Some(restate_port) = all_ports.find(|port| port.name.as_deref() == Some("restate")) {
        return restate_port.container_port;
    }

    // default to the first port if none are named restate
    first_port.container_port
}

/// Name of the operator-managed Service that routes to the latest version.
pub fn latest_service_name(rsd_name: &str) -> String {
    format!("{rsd_name}-latest")
}

/// What to do with the `<name>-latest` Service this reconcile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LatestServicePlan {
    /// Leave it as it is (absent, or still selecting the previous version).
    Leave,
    /// Apply it with this selector.
    Apply(BTreeMap<String, String>),
    /// Delete it; the feature was disabled.
    Delete,
}

/// Decide what to do with the `<name>-latest` Service.
///
/// Called only once Restate has confirmed `latest_selector`'s version as the latest
/// deployment. The selector switches to it only when its ReplicaSet is also ready; until
/// then the Service keeps selecting the previous version. A Service that already selects
/// the latest version is re-applied regardless, to fix drift.
pub fn plan_latest_service(
    rsd: &RestateDeployment,
    existing: Option<&Service>,
    latest_selector: &BTreeMap<String, String>,
    latest_ready: bool,
) -> Result<LatestServicePlan> {
    let rsd_uid = rsd.uid();
    let owned = existing.map(|svc| {
        svc.owner_references()
            .iter()
            .any(|r| Some(&r.uid) == rsd_uid.as_ref() && r.kind == RestateDeployment::kind(&()))
    });

    let enabled = rsd.spec.latest_service.as_ref().is_some_and(|s| s.enabled)
        && !matches!(rsd.spec.deployment_mode, Some(DeploymentMode::Knative));

    if !enabled {
        return Ok(match owned {
            Some(true) => LatestServicePlan::Delete,
            _ => LatestServicePlan::Leave,
        });
    }

    if owned == Some(false) {
        return Err(Error::InvalidRestateConfig(format!(
            "Service {} already exists and is not owned by this RestateDeployment; \
             rename or delete it, or disable spec.latestService",
            latest_service_name(&rsd.name_any()),
        )));
    }

    let current_selector = existing
        .and_then(|svc| svc.spec.as_ref())
        .and_then(|spec| spec.selector.as_ref());

    if latest_ready || current_selector == Some(latest_selector) {
        Ok(LatestServicePlan::Apply(latest_selector.clone()))
    } else {
        Ok(LatestServicePlan::Leave)
    }
}

/// Every container port of the pod template, so traffic that bypasses Restate can reach
/// the latest version on any port. Falls back to the Restate port when none are declared.
pub fn latest_service_ports(pod_spec: Option<&PodSpec>) -> Vec<ServicePort> {
    let mut seen = BTreeSet::new();
    let mut ports = Vec::new();

    for port in pod_spec
        .iter()
        .flat_map(|s| s.containers.iter())
        .flat_map(|c| c.ports.iter().flatten())
    {
        let protocol = port.protocol.clone().unwrap_or_else(|| "TCP".to_owned());
        if !seen.insert((port.container_port, protocol.clone())) {
            continue;
        }
        let name = port
            .name
            .clone()
            .unwrap_or_else(|| format!("{}-{}", protocol.to_lowercase(), port.container_port));
        ports.push(ServicePort {
            name: Some(name),
            port: port.container_port,
            protocol: Some(protocol),
            target_port: Some(IntOrString::Int(port.container_port)),
            ..Default::default()
        });
    }

    if ports.is_empty() {
        ports.push(ServicePort {
            name: Some("restate".to_owned()),
            port: DEFAULT_RESTATE_PORT,
            protocol: Some("TCP".to_owned()),
            target_port: Some(IntOrString::Int(DEFAULT_RESTATE_PORT)),
            ..Default::default()
        });
    }

    ports
}

/// Create, update or delete the `<name>-latest` Service, given that Restate has confirmed
/// `latest_rs` as the latest version.
///
/// Failures are reported as a Warning event but do not fail the reconcile, so a problem
/// with this Service never stalls a rollout or the drain of old versions.
#[allow(clippy::too_many_arguments)]
pub async fn reconcile_latest_service(
    ctx: &Context,
    rsd: &RestateDeployment,
    namespace: &str,
    latest_rs: &ReplicaSet,
    latest_selector: &BTreeMap<String, String>,
    labels: &BTreeMap<String, String>,
    annotations: &BTreeMap<String, String>,
) -> Result<()> {
    let name = latest_service_name(&rsd.name_any());
    if let Err(err) = try_reconcile_latest_service(
        ctx,
        rsd,
        namespace,
        &name,
        latest_rs,
        latest_selector,
        labels,
        annotations,
    )
    .await
    {
        warn!("failed to reconcile Service {name} in {namespace}: {err}");
        ctx.recorder
            .publish(
                &Event {
                    type_: EventType::Warning,
                    reason: "LatestServiceFailed".into(),
                    note: Some(format!("Failed to reconcile Service {name}: {err}")),
                    action: "Reconcile".into(),
                    secondary: None,
                },
                &rsd.object_ref(&()),
            )
            .await?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn try_reconcile_latest_service(
    ctx: &Context,
    rsd: &RestateDeployment,
    namespace: &str,
    name: &str,
    latest_rs: &ReplicaSet,
    latest_selector: &BTreeMap<String, String>,
    labels: &BTreeMap<String, String>,
    annotations: &BTreeMap<String, String>,
) -> Result<()> {
    let svc_api = Api::<Service>::namespaced(ctx.client.clone(), namespace);
    let enabled = rsd.spec.latest_service.as_ref().is_some_and(|s| s.enabled);

    // The store only holds operator-labelled Services; when enabled, look a missing one
    // up directly so a user's Service of the same name is never taken over.
    let existing = match ctx
        .services_store
        .get(&ObjectRef::new(name).within(namespace))
    {
        Some(svc) => Some((*svc).clone()),
        None if enabled => svc_api.get_opt(name).await?,
        None => None,
    };

    let latest_ready =
        validate_replica_set_status(latest_rs.status.as_ref(), rsd.spec.replicas).is_ok();

    match plan_latest_service(rsd, existing.as_ref(), latest_selector, latest_ready)? {
        LatestServicePlan::Leave => {}
        LatestServicePlan::Delete => {
            debug!("Deleting Service {name} in namespace {namespace}");
            match svc_api.delete(name, &DeleteParams::default()).await {
                Ok(_) => {}
                Err(kube::Error::Api(err)) if err.code == 404 => {}
                Err(err) => return Err(err.into()),
            }
        }
        LatestServicePlan::Apply(selector) => {
            let previous = existing
                .as_ref()
                .and_then(|svc| svc.spec.as_ref())
                .and_then(|spec| spec.selector.clone());
            if previous.as_ref() != Some(&selector) {
                info!(
                    "Pointing Service {name} in namespace {namespace} at {}",
                    latest_rs.name_any()
                );
            }

            let service = Service {
                metadata: ObjectMeta {
                    name: Some(name.to_owned()),
                    namespace: Some(namespace.to_owned()),
                    labels: Some(labels.clone()),
                    annotations: Some(annotations.clone()),
                    owner_references: Some(vec![rsd.controller_owner_ref(&()).unwrap()]),
                    ..Default::default()
                },
                spec: Some(ServiceSpec {
                    selector: Some(selector),
                    ports: Some(latest_service_ports(
                        latest_rs
                            .spec
                            .as_ref()
                            .and_then(|s| s.template.as_ref())
                            .and_then(|t| t.spec.as_ref()),
                    )),
                    type_: Some("ClusterIP".to_string()),
                    ..Default::default()
                }),
                status: None,
            };

            svc_api
                .patch(
                    name,
                    &PatchParams::apply("restate-operator/latest-service").force(),
                    &Patch::Apply(&service),
                )
                .await?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controllers::restatedeployment::reconcilers::replicaset::{
        generate_pod_template_hash, pod_template_annotation,
    };
    use serde_json::json;

    fn rsd(mode: Option<&str>, latest_service: serde_json::Value) -> RestateDeployment {
        let spec = serde_json::from_value(json!({
            "deploymentMode": mode,
            "replicas": 2,
            "revisionHistoryLimit": 10,
            "selector": null,
            "template": {
                "metadata": null,
                "spec": { "containers": [{ "name": "main", "image": "greeter:1" }] }
            },
            "restate": { "register": { "url": "http://localhost:9070/" } },
            "latestService": latest_service,
        }))
        .expect("test RestateDeploymentSpec deserializes");
        let mut rsd = RestateDeployment::new("greeter", spec);
        rsd.metadata.uid = Some("rsd-uid".into());
        rsd
    }

    fn enabled() -> RestateDeployment {
        rsd(None, json!({ "enabled": true }))
    }

    fn selector(hash: &str) -> BTreeMap<String, String> {
        BTreeMap::from([("pod-template-hash".to_owned(), hash.to_owned())])
    }

    fn service(hash: &str, owner_uid: &str) -> Service {
        serde_json::from_value(json!({
            "metadata": {
                "name": "greeter-latest",
                "ownerReferences": [{
                    "apiVersion": "restate.dev/v1beta1", "kind": "RestateDeployment",
                    "name": "greeter", "uid": owner_uid, "controller": true
                }]
            },
            "spec": { "selector": { "pod-template-hash": hash } }
        }))
        .unwrap()
    }

    #[test]
    fn created_only_once_the_latest_version_is_ready() {
        let plan = plan_latest_service(&enabled(), None, &selector("v1"), false).unwrap();
        assert_eq!(plan, LatestServicePlan::Leave);

        let plan = plan_latest_service(&enabled(), None, &selector("v1"), true).unwrap();
        assert_eq!(plan, LatestServicePlan::Apply(selector("v1")));
    }

    #[test]
    fn selector_switches_to_a_new_version_only_once_it_is_ready() {
        let existing = service("v1", "rsd-uid");
        let plan =
            plan_latest_service(&enabled(), Some(&existing), &selector("v2"), false).unwrap();
        assert_eq!(plan, LatestServicePlan::Leave);

        let plan = plan_latest_service(&enabled(), Some(&existing), &selector("v2"), true).unwrap();
        assert_eq!(plan, LatestServicePlan::Apply(selector("v2")));
    }

    #[test]
    fn rollback_switches_the_selector_back() {
        // v2 had replaced v1; the spec was rolled back and Restate confirmed v1 again
        let existing = service("v2", "rsd-uid");
        let plan = plan_latest_service(&enabled(), Some(&existing), &selector("v1"), true).unwrap();
        assert_eq!(plan, LatestServicePlan::Apply(selector("v1")));
    }

    #[test]
    fn a_service_on_the_latest_version_is_reapplied_to_fix_drift() {
        let existing = service("v2", "rsd-uid");
        let plan =
            plan_latest_service(&enabled(), Some(&existing), &selector("v2"), false).unwrap();
        assert_eq!(plan, LatestServicePlan::Apply(selector("v2")));
    }

    #[test]
    fn disabled_deletes_only_an_owned_service() {
        for disabled in [
            rsd(None, json!(null)),
            rsd(None, json!({ "enabled": false })),
        ] {
            let owned = service("v1", "rsd-uid");
            let plan = plan_latest_service(&disabled, Some(&owned), &selector("v1"), true).unwrap();
            assert_eq!(plan, LatestServicePlan::Delete);

            let foreign = service("v1", "other-uid");
            let plan =
                plan_latest_service(&disabled, Some(&foreign), &selector("v1"), true).unwrap();
            assert_eq!(plan, LatestServicePlan::Leave);

            let plan = plan_latest_service(&disabled, None, &selector("v1"), true).unwrap();
            assert_eq!(plan, LatestServicePlan::Leave);
        }
    }

    #[test]
    fn knative_mode_is_a_no_op() {
        let knative = rsd(Some("knative"), json!({ "enabled": true }));
        let plan = plan_latest_service(&knative, None, &selector("v1"), true).unwrap();
        assert_eq!(plan, LatestServicePlan::Leave);
    }

    #[test]
    fn a_foreign_service_of_the_same_name_is_not_taken_over() {
        let foreign = service("v1", "other-uid");
        let err = plan_latest_service(&enabled(), Some(&foreign), &selector("v2"), true);
        assert!(
            matches!(err, Err(Error::InvalidRestateConfig(_))),
            "{err:?}"
        );

        let unowned: Service = serde_json::from_value(json!({
            "metadata": { "name": "greeter-latest" },
            "spec": { "selector": { "app": "greeter" } }
        }))
        .unwrap();
        let err = plan_latest_service(&enabled(), Some(&unowned), &selector("v2"), true);
        assert!(
            matches!(err, Err(Error::InvalidRestateConfig(_))),
            "{err:?}"
        );
    }

    #[test]
    fn ports_cover_every_container_port() {
        let pod_spec: PodSpec = serde_json::from_value(json!({
            "containers": [
                { "name": "app", "ports": [
                    { "name": "restate", "containerPort": 9080 },
                    { "name": "http", "containerPort": 8080 },
                    { "containerPort": 5353, "protocol": "UDP" }
                ]},
                { "name": "sidecar", "ports": [
                    { "containerPort": 9090 },
                    { "name": "http-again", "containerPort": 8080 }
                ]}
            ]
        }))
        .unwrap();

        let ports: Vec<_> = latest_service_ports(Some(&pod_spec))
            .into_iter()
            .map(|p| {
                (
                    p.name.unwrap(),
                    p.port,
                    p.target_port.unwrap(),
                    p.protocol.unwrap(),
                )
            })
            .collect();
        assert_eq!(
            ports,
            vec![
                ("restate".into(), 9080, IntOrString::Int(9080), "TCP".into()),
                ("http".into(), 8080, IntOrString::Int(8080), "TCP".into()),
                (
                    "udp-5353".into(),
                    5353,
                    IntOrString::Int(5353),
                    "UDP".into()
                ),
                (
                    "tcp-9090".into(),
                    9090,
                    IntOrString::Int(9090),
                    "TCP".into()
                ),
            ]
        );
    }

    #[test]
    fn ports_default_to_the_restate_port() {
        let ports = latest_service_ports(None);
        assert_eq!(ports.len(), 1);
        assert_eq!(ports[0].name.as_deref(), Some("restate"));
        assert_eq!(ports[0].port, DEFAULT_RESTATE_PORT);
    }

    #[test]
    fn enabling_the_latest_service_does_not_change_the_version_hash() {
        let hash = |rsd: &RestateDeployment| {
            generate_pod_template_hash(rsd, &pod_template_annotation(rsd), None)
        };
        assert_eq!(hash(&rsd(None, json!(null))), hash(&enabled()));
        assert_eq!(
            hash(&rsd(None, json!(null))),
            hash(&rsd(None, json!({ "enabled": false })))
        );
    }
}
