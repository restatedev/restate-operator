use std::collections::BTreeMap;

use k8s_openapi::api::apps::v1::ReplicaSet;
use k8s_openapi::api::core::v1::{Service, ServicePort, ServiceSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;

use kube::Resource;
use kube::api::{Api, Patch, PatchParams};
use tracing::*;

use crate::Result;

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
