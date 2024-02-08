use std::collections::BTreeMap;

use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta, OwnerReference};

pub mod compute;
pub mod network_policies;

// resource_labels returns labels to apply to all created resources
// it is not safe to change these; statefulset volume template labels are immutable
pub fn resource_labels(instance: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("app.kubernetes.io/name".into(), "restate".into()),
        ("app.kubernetes.io/instance".into(), instance.into()),
    ])
}

pub fn label_selector(instance: &str) -> LabelSelector {
    LabelSelector {
        match_labels: Some(resource_labels(instance)),
        match_expressions: None,
    }
}

pub fn object_meta(oref: &OwnerReference, name: impl Into<String>) -> ObjectMeta {
    ObjectMeta {
        name: Some(name.into()),
        labels: Some(resource_labels(&oref.name)),
        owner_references: Some(vec![oref.clone()]),
        ..Default::default()
    }
}
