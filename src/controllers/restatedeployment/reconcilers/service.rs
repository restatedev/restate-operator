use std::collections::BTreeMap;

use k8s_openapi::api::apps::v1::ReplicaSet;
use k8s_openapi::api::core::v1::{Service, ServicePort, ServiceSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;

use kube::api::{Api, Patch, PatchParams};
use kube::{Resource, ResourceExt};
use tracing::*;

use crate::{Error, Result};

// Label keys used to track and identify resources
const APP_INSTANCE_LABEL: &str = "app.kubernetes.io/instance";
const APP_NAME_LABEL: &str = "app.kubernetes.io/name";
const APP_MANAGED_BY_LABEL: &str = "app.kubernetes.io/managed-by";

// Default port for Restate services
const DEFAULT_RESTATE_PORT: i32 = 9080;

/// Create or update a Service for a specific version of the RestateDeployment
pub async fn reconcile_service(
    namespace: &str,
    svc_api: &Api<Service>,
    versioned_name: &str,
    selector: BTreeMap<String, String>,
    rs: &ReplicaSet,
) -> Result<Service> {
    // Create service labels
    let mut labels = BTreeMap::new();
    labels.insert(APP_INSTANCE_LABEL.to_string(), rs.name_any());
    labels.insert(APP_NAME_LABEL.to_string(), "restate-service".to_string());
    labels.insert(
        APP_MANAGED_BY_LABEL.to_string(),
        "restate-operator".to_string(),
    );

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
    // TODO; support custom service annotations
    let metadata = ObjectMeta {
        name: Some(versioned_name.to_owned()),
        namespace: Some(namespace.to_owned()),
        labels: Some(labels),
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
    info!("Creating/updating Service {versioned_name} in namespace {namespace}",);

    apply_service(namespace, svc_api, &service).await?;

    Ok(service)
}

async fn apply_service(
    namespace: &str,
    svc_api: &Api<Service>,
    svc: &Service,
) -> Result<(), Error> {
    let name = svc.metadata.name.as_ref().unwrap();
    let params: PatchParams = PatchParams::apply("restate-operator").force();
    debug!("Applying Service {} in namespace {}", name, namespace);
    svc_api.patch(name, &params, &Patch::Apply(svc)).await?;
    Ok(())
}

/// Find the appropriate service port by examining containers
fn find_service_port(pod_template: Option<&k8s_openapi::api::core::v1::PodSpec>) -> i32 {
    // First look for a port named 'restate'
    if let Some(spec) = &pod_template {
        for container in &spec.containers {
            if let Some(ports) = &container.ports {
                if let Some(port) = ports
                    .iter()
                    .find(|port| port.name.as_deref() == Some("restate"))
                {
                    return port.container_port;
                }
            }
        }

        // If no restate container found, use the first container's first port
        if let Some(container) = spec.containers.first() {
            if let Some(ports) = &container.ports {
                if !ports.is_empty() {
                    return ports[0].container_port;
                }
            }
        }
    }

    // Default to standard Restate SDK port if nothing found
    DEFAULT_RESTATE_PORT
}
