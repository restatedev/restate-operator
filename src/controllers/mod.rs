use std::sync::Arc;

use chrono::{DateTime, Utc};
use futures::StreamExt;
use kube::runtime::WatchStreamExt;
use kube::runtime::{reflector, watcher};
use kube::Resource;
use serde::Serialize;
use tokio::sync::RwLock;
use tracing::debug;
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

/// Creates a pre-warmed reflector stream that can be passed to controller methods.
///
/// This function:
/// 1. Creates a reflector from the given writer and watcher
/// 2. Polls the reflector until the store is ready (pre-warming)
/// 3. Returns the reflector stream ready to be passed to owns_stream() or watches_stream()
///
/// The store must be created with `store_shared()` for this pattern to work correctly.
pub async fn prewarmed_reflector<K>(
    store: reflector::Store<K>,
    writer: reflector::store::Writer<K>,
    watch_stream: impl futures::Stream<Item = Result<watcher::Event<K>, watcher::Error>>
        + Send
        + 'static,
) -> impl futures::Stream<Item = Result<K, watcher::Error>>
where
    K: Clone + std::fmt::Debug + Send + Sync + 'static,
    K: Resource<DynamicType = ()>,
    K::DynamicType: Eq + std::hash::Hash + Clone + Default,
{
    let kind = K::kind(&()).to_string();

    debug!("Waiting for {} store to sync...", kind);

    let mut stream = reflector(writer, watch_stream)
        .touched_objects()
        .default_backoff()
        .boxed();

    let mut store_ready = std::pin::pin!(store.wait_until_ready());

    loop {
        tokio::select! {
            _ = stream.next() => {},
            ready = &mut store_ready => {
                ready.unwrap_or_else(|_| panic!("{} store failed to sync unexpectedly", kind));
                break
            }
        }
    }

    debug!("{} store ready", kind);

    stream
}
