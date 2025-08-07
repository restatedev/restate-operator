use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::sync::RwLock;
use url::Url;

pub mod restatecloudenvironment;
pub mod restatecluster;
pub mod restatedeployment;

/// Diagnostics to be exposed by the web server
#[derive(Clone, Serialize)]
pub struct Diagnostics {
    #[serde(deserialize_with = "from_ts")]
    pub last_event: DateTime<Utc>,
}

impl Default for Diagnostics {
    fn default() -> Self {
        Self {
            last_event: Utc::now(),
        }
    }
}

/// State shared between the controller and the web server
#[derive(Clone)]
pub struct State {
    /// Diagnostics populated by the reconciler
    pub diagnostics: Arc<RwLock<Diagnostics>>,
    /// Metrics registry
    pub registry: prometheus::Registry,
    /// If set, watch AWS PodIdentityAssociation resources, and if requested create them against this cluster
    aws_pod_identity_association_cluster: Option<String>,

    /// Our namespace, needed for network policies and reading secrets
    operator_namespace: String,
    /// The name of a label that can select the operator, needed to support the case where restate clusters need to be reached by the operator
    operator_label_name: Option<String>,
    /// The value of the label named operator_label_name that will select the operator, needed to support the case where restate clusters need to be reached by the operator
    operator_label_value: Option<String>,

    /// The default image to use for tunnel client pods
    tunnel_client_default_image: String,
}

/// State wrapper around the controller outputs for the web server
impl State {
    pub fn new(
        aws_pod_identity_association_cluster: Option<String>,
        operator_namespace: String,
        operator_label_name: Option<String>,
        operator_label_value: Option<String>,
        tunnel_client_default_image: String,
    ) -> Self {
        Self {
            diagnostics: Arc::new(RwLock::new(Diagnostics::default())),
            registry: prometheus::Registry::default(),
            aws_pod_identity_association_cluster,
            operator_namespace,
            operator_label_name,
            operator_label_value,
            tunnel_client_default_image,
        }
    }

    /// Metrics getter
    pub fn metrics(&self) -> Vec<prometheus::proto::MetricFamily> {
        self.registry.gather()
    }

    /// State getter
    pub async fn diagnostics(&self) -> Diagnostics {
        self.diagnostics.read().await.clone()
    }
}

pub fn service_url(
    service_name: &str,
    service_namespace: &str,
    port: i32,
    path: Option<&str>,
) -> Result<Url, url::ParseError> {
    let mut url = Url::parse(&format!(
        "http://{service_name}.{service_namespace}.svc.cluster.local:{port}",
    ))?;

    if let Some(path) = path {
        url.set_path(path)
    }

    Ok(url)
}
