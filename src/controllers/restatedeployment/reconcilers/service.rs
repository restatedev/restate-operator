use std::collections::BTreeMap;

use k8s_openapi::api::apps::v1::ReplicaSet;
use k8s_openapi::api::core::v1::{Service, ServicePort, ServiceSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;

use kube::api::{Api, Patch, PatchParams};
use kube::{Resource, ResourceExt};
use tracing::*;

use crate::Result;
use crate::controllers::ssa;

// Default port for Restate services
const DEFAULT_RESTATE_PORT: i32 = 9080;

// needs_apply reads back what this manager owns, so keep the two in step
const FIELD_MANAGER: &str = "restate-operator";

/// Create or update a Service for a specific version of the RestateDeployment
///
/// `existing` is whatever the controller's cache last saw, so we can skip a no-op apply.
#[allow(clippy::too_many_arguments)]
pub async fn reconcile_service(
    namespace: &str,
    svc_api: &Api<Service>,
    versioned_name: &str,
    selector: BTreeMap<String, String>,
    labels: BTreeMap<String, String>,
    annotations: BTreeMap<String, String>,
    rs: &ReplicaSet,
    existing: Option<&Service>,
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

    if !needs_apply(existing, &service) {
        return Ok(service);
    }

    // Apply the service
    debug!("Applying Service {versioned_name} in namespace {namespace}",);

    let params: PatchParams = PatchParams::apply(FIELD_MANAGER).force();
    svc_api
        .patch(versioned_name, &params, &Patch::Apply(&service))
        .await?;

    Ok(service)
}

/// Whether applying `desired` would change the live Service.
///
/// Only compares the fields we set. The apiserver defaults the rest (clusterIP, ipFamilies, a port's
/// nodePort) and those aren't ours to reconcile. Owner refs and annotations are subset checks,
/// because other writers add their own, including our separate deployment-id apply.
fn needs_apply(existing: Option<&Service>, desired: &Service) -> bool {
    let Some(existing) = existing else {
        return true;
    };

    if ssa::labels_need_apply(&existing.metadata, FIELD_MANAGER, desired.labels())
        || ssa::annotations_need_apply(&existing.metadata, FIELD_MANAGER, desired.annotations())
    {
        return true;
    }

    let contains_all_owner_refs =
        desired
            .metadata
            .owner_references
            .iter()
            .flatten()
            .all(|desired_ref| {
                existing
                    .metadata
                    .owner_references
                    .iter()
                    .flatten()
                    .any(|existing_ref| existing_ref == desired_ref)
            });
    if !contains_all_owner_refs {
        return true;
    }

    let (Some(existing_spec), Some(desired_spec)) = (existing.spec.as_ref(), desired.spec.as_ref())
    else {
        return true;
    };

    if existing_spec.selector != desired_spec.selector || existing_spec.type_ != desired_spec.type_
    {
        return true;
    }

    !desired_spec.ports.iter().flatten().all(|desired_port| {
        existing_spec.ports.iter().flatten().any(|existing_port| {
            existing_port.name == desired_port.name
                && existing_port.port == desired_port.port
                && existing_port.protocol == desired_port.protocol
                && existing_port.target_port == desired_port.target_port
        })
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{FieldsV1, ManagedFieldsEntry};
    use serde_json::json;

    fn desired_service() -> Service {
        Service {
            metadata: ObjectMeta {
                name: Some("greeter-abc123".into()),
                namespace: Some("default".into()),
                labels: Some([("app".to_owned(), "greeter".to_owned())].into()),
                annotations: Some([("team".to_owned(), "core".to_owned())].into()),
                ..Default::default()
            },
            spec: Some(ServiceSpec {
                selector: Some([("pod-template-hash".to_owned(), "abc123".to_owned())].into()),
                ports: Some(vec![ServicePort {
                    name: Some("restate".to_string()),
                    port: 9080,
                    protocol: Some("TCP".to_string()),
                    target_port: Some(IntOrString::Int(9080)),
                    ..Default::default()
                }]),
                type_: Some("ClusterIP".to_string()),
                ..Default::default()
            }),
            status: None,
        }
    }

    // as the apiserver hands it back: fields it defaulted that we never set, plus the deployment id
    // from the other field manager
    fn live_service(desired: &Service) -> Service {
        let mut live = desired.clone();
        let metadata = &mut live.metadata;
        metadata
            .annotations
            .as_mut()
            .unwrap()
            .insert("restate.dev/deployment-id".into(), "dp_1".into());
        metadata.managed_fields = Some(vec![ManagedFieldsEntry {
            manager: Some(FIELD_MANAGER.into()),
            fields_v1: Some(FieldsV1(json!({
                "f:metadata": {
                    "f:labels": { "f:app": {} },
                    "f:annotations": { "f:team": {} },
                },
            }))),
            ..Default::default()
        }]);

        let spec = live.spec.as_mut().unwrap();
        spec.cluster_ip = Some("10.0.0.1".into());
        spec.ip_families = Some(vec!["IPv4".into()]);
        spec.ports.as_mut().unwrap()[0].node_port = None;
        live
    }

    #[test]
    fn no_apply_when_the_live_service_already_matches() {
        let desired = desired_service();
        assert!(!needs_apply(Some(&live_service(&desired)), &desired));
    }

    #[test]
    fn apply_when_the_service_does_not_exist_yet() {
        assert!(needs_apply(None, &desired_service()));
    }

    #[test]
    fn apply_when_the_selector_drifts() {
        let desired = desired_service();
        let mut live = live_service(&desired);
        live.spec.as_mut().unwrap().selector =
            Some([("pod-template-hash".to_owned(), "stale".to_owned())].into());
        assert!(needs_apply(Some(&live), &desired));
    }

    #[test]
    fn apply_when_the_target_port_drifts() {
        let desired = desired_service();
        let mut live = live_service(&desired);
        live.spec.as_mut().unwrap().ports.as_mut().unwrap()[0].target_port =
            Some(IntOrString::Int(8080));
        assert!(needs_apply(Some(&live), &desired));
    }

    #[test]
    fn apply_when_a_propagated_label_was_removed_from_the_deployment() {
        let mut desired = desired_service();
        let live = live_service(&desired);
        // app is gone from the rsd, so the apply has to run to prune it
        desired.metadata.labels = Some(Default::default());
        assert!(needs_apply(Some(&live), &desired));
    }
}
