use std::collections::BTreeMap;

use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta};

pub mod config;
pub mod deployment;

/// The name of the container the operator generates. Users target it by name from
/// `spec.template` to override its fields.
pub const CONTAINER_NAME: &str = "kafka-integration";

/// The name of the volume the `.properties` configuration is projected through.
pub const CONFIG_VOLUME_NAME: &str = "config";

/// The port the image documents for its Prometheus scrape endpoint
/// (`restate.metrics.port`, default 9464). Declared on the container so the port has a name;
/// the container decides whether it actually listens.
pub const METRICS_PORT: i32 = 9464;

/// The labels that identify a RestateKafkaIntegration's pods.
///
/// These end up in the Deployment's `spec.selector`, which is immutable once created, so
/// nothing may be added here without a migration.
pub fn selector_labels(base_metadata: &ObjectMeta) -> BTreeMap<String, String> {
    BTreeMap::from_iter([
        (
            "app.kubernetes.io/name".into(),
            "restate-kafka-integration".into(),
        ),
        (
            "app.kubernetes.io/instance".into(),
            base_metadata.name.clone().unwrap(),
        ),
    ])
}

/// Labels applied to every object the operator creates for a RestateKafkaIntegration, on top
/// of the labels copied from the RestateKafkaIntegration itself.
pub fn mandatory_labels(base_metadata: &ObjectMeta) -> BTreeMap<String, String> {
    let mut labels = selector_labels(base_metadata);
    // This is what the controller's `owns` watch filters on, so a child object without it is
    // invisible to the controller until the next periodic requeue.
    labels.insert(
        "app.kubernetes.io/managed-by".into(),
        "restate-operator".into(),
    );
    labels
}

pub fn label_selector(base_metadata: &ObjectMeta) -> LabelSelector {
    LabelSelector {
        match_labels: Some(selector_labels(base_metadata)),
        match_expressions: None,
    }
}

pub fn object_meta(base_metadata: &ObjectMeta) -> ObjectMeta {
    let mut meta = base_metadata.clone();
    meta.labels
        .get_or_insert_with(Default::default)
        .extend(mandatory_labels(base_metadata));
    meta
}
