use std::collections::BTreeMap;
use std::iter;

use itertools::{Either, Itertools};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta, OwnerReference};

pub mod compute;
pub mod network_policies;

pub fn resource_labels(instance: &str) -> Option<BTreeMap<String, String>> {
    Some(BTreeMap::from([
        ("app.kubernetes.io/name".into(), "restate".into()),
        ("app.kubernetes.io/instance".into(), instance.into()),
    ]))
}

pub fn label_selector(instance: &str) -> LabelSelector {
    LabelSelector {
        match_labels: Some(BTreeMap::from([
            ("app.kubernetes.io/name".into(), "restate".into()),
            ("app.kubernetes.io/instance".into(), instance.into()),
        ])),
        match_expressions: None,
    }
}

fn label_selector_string(ls: LabelSelector) -> String {
    let selectors = iter::empty();
    let selectors = if let Some(labels) = ls.match_labels {
        // environment=production
        Either::Left(selectors.chain(labels.into_iter().map(|(k, v)| format!("{k}={v}"))))
    } else {
        Either::Right(selectors)
    };

    let mut selectors = if let Some(expressions) = ls.match_expressions {
        Either::Left(selectors.chain(expressions.into_iter().filter_map(|e| {
            match e.operator.as_str() {
                "In" => Some(format!(
                    "{} in ({})", // environment in (production, qa)
                    e.key,
                    e.values.unwrap_or_default().join(",")
                )),
                "NotIn" => Some(format!(
                    "{} notin ({})", // tier notin (frontend, backend)
                    e.key,
                    e.values.unwrap_or_default().join(",")
                )),
                "Exists" => Some(e.key),                       // partition
                "DoesNotExist" => Some(format!("!{}", e.key)), // !partition
                _ => None,                                     // invalid operator, skip it
            }
        })))
    } else {
        Either::Right(selectors)
    };

    selectors.join(",")
}

pub fn object_meta(oref: &OwnerReference, name: &str) -> ObjectMeta {
    ObjectMeta {
        name: Some(name.into()),
        labels: resource_labels(&oref.name),
        owner_references: Some(vec![oref.clone()]),
        ..Default::default()
    }
}
