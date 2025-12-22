use std::collections::BTreeMap;

use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta};

pub mod compute;
pub mod network_policies;
pub mod provisioning;
mod quantity_parser;
pub mod signing_key;

// resource_labels returns labels to apply to all created resources on top of the RestateCluster labels
// it is not safe to change these; statefulset volume template labels are immutable
pub fn mandatory_labels(base_metadata: &ObjectMeta) -> BTreeMap<String, String> {
    BTreeMap::from_iter([
        ("app.kubernetes.io/name".into(), "restate".into()),
        (
            "app.kubernetes.io/instance".into(),
            base_metadata.name.clone().unwrap(),
        ),
    ])
}

pub fn label_selector(base_metadata: &ObjectMeta) -> LabelSelector {
    LabelSelector {
        match_labels: Some(mandatory_labels(base_metadata)),
        match_expressions: None,
    }
}

pub fn object_meta(base_metadata: &ObjectMeta, name: impl Into<String>) -> ObjectMeta {
    let mut meta = base_metadata.clone();
    meta.name = Some(name.into());
    meta.labels
        .get_or_insert_with(Default::default)
        .extend(mandatory_labels(base_metadata));
    meta
}
