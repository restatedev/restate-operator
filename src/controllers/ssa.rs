//! Deciding whether a server-side apply would actually change anything.
//!
//! We re-apply the objects we own every reconcile, which ought to be free. It isn't: a no-op apply
//! still seems to come back with a bumped resourceVersion (the ReplicaSet scale subresource is the
//! prime suspect), and the owned-object watch turns that into another reconcile. Hence #138. So the
//! applies are conditional now, and this is the condition.
//!
//! Watch out for pruning. An unconditional apply drops fields the manager owns but no longer lists,
//! which is how a label removed from a RestateDeployment got removed downstream too. Skip the apply
//! and we have to spot that ourselves, which is what [`owned_keys`] is for.

use std::collections::BTreeMap;

use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

/// Whether applying `desired` as `manager` would change the annotations: one is missing or stale,
/// or `manager` still owns a key `desired` has dropped.
///
/// Subset comparison rather than equality, because other field managers add their own: we write
/// restate.dev/deployment-id under a separate manager, and kubectl and friends add theirs.
pub(crate) fn annotations_need_apply(
    meta: &ObjectMeta,
    manager: &str,
    desired: &BTreeMap<String, String>,
) -> bool {
    fields_need_apply(meta, manager, "f:annotations", &meta.annotations, desired)
}

/// As [`annotations_need_apply`], for labels.
pub(crate) fn labels_need_apply(
    meta: &ObjectMeta,
    manager: &str,
    desired: &BTreeMap<String, String>,
) -> bool {
    fields_need_apply(meta, manager, "f:labels", &meta.labels, desired)
}

fn fields_need_apply(
    meta: &ObjectMeta,
    manager: &str,
    fields_key: &str,
    existing: &Option<BTreeMap<String, String>>,
    desired: &BTreeMap<String, String>,
) -> bool {
    let empty = BTreeMap::new();
    let existing = existing.as_ref().unwrap_or(&empty);

    if desired
        .iter()
        .any(|(key, value)| existing.get(key) != Some(value))
    {
        return true;
    }

    owned_keys(meta, manager, fields_key).any(|key| !desired.contains_key(&key))
}

/// The keys under `metadata.<fields_key>` that `manager` owns, read back out of its `managedFields`
/// entry.
///
/// Subresource entries don't count; owning spec.replicas through the scale subresource says nothing
/// about who owns metadata on the parent.
fn owned_keys<'a>(
    meta: &'a ObjectMeta,
    manager: &'a str,
    fields_key: &'a str,
) -> impl Iterator<Item = String> + 'a {
    meta.managed_fields
        .iter()
        .flatten()
        .filter(move |entry| {
            entry.manager.as_deref() == Some(manager) && entry.subresource.is_none()
        })
        .filter_map(|entry| entry.fields_v1.as_ref())
        .filter_map(move |fields| fields.0.get("f:metadata")?.get(fields_key)?.as_object())
        .flat_map(|fields| fields.keys())
        .filter_map(|key| key.strip_prefix("f:").map(str::to_owned))
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{FieldsV1, ManagedFieldsEntry};
    use serde_json::json;

    const MANAGER: &str = "restate-operator/propagate-annotations";

    fn meta(annotations: &[(&str, &str)], owned: &[&str]) -> ObjectMeta {
        let owned_fields: serde_json::Map<String, serde_json::Value> = owned
            .iter()
            .map(|key| (format!("f:{key}"), json!({})))
            .collect();

        ObjectMeta {
            annotations: Some(
                annotations
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            ),
            managed_fields: Some(vec![
                ManagedFieldsEntry {
                    manager: Some(MANAGER.into()),
                    fields_v1: Some(FieldsV1(json!({
                        "f:metadata": { "f:annotations": owned_fields },
                    }))),
                    ..Default::default()
                },
                // another manager owning the same shape must not be read as ours
                ManagedFieldsEntry {
                    manager: Some("restate-operator/deployment-id".into()),
                    fields_v1: Some(FieldsV1(json!({
                        "f:metadata": { "f:annotations": { "f:restate.dev/deployment-id": {} } },
                    }))),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        }
    }

    fn desired(annotations: &[(&str, &str)]) -> BTreeMap<String, String> {
        annotations
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn no_apply_when_every_desired_annotation_is_already_set() {
        let meta = meta(&[("a", "1"), ("restate.dev/deployment-id", "dp_1")], &["a"]);
        assert!(!annotations_need_apply(
            &meta,
            MANAGER,
            &desired(&[("a", "1")])
        ));
    }

    #[test]
    fn apply_when_an_annotation_is_missing_or_stale() {
        let meta = meta(&[("a", "1")], &["a"]);
        assert!(annotations_need_apply(
            &meta,
            MANAGER,
            &desired(&[("a", "1"), ("b", "2")])
        ));
        assert!(annotations_need_apply(
            &meta,
            MANAGER,
            &desired(&[("a", "2")])
        ));
    }

    // the pruning case: b was propagated from the rsd and has since been removed from it. nothing
    // is stale, but the apply still has to run to drop it.
    #[test]
    fn apply_when_an_owned_annotation_is_no_longer_desired() {
        let meta = meta(&[("a", "1"), ("b", "2")], &["a", "b"]);
        assert!(annotations_need_apply(
            &meta,
            MANAGER,
            &desired(&[("a", "1")])
        ));
    }

    // an annotation another manager owns isn't ours to prune, so it mustn't force an apply
    #[test]
    fn no_apply_for_annotations_owned_by_another_manager() {
        let meta = meta(&[("restate.dev/deployment-id", "dp_1")], &[]);
        assert!(!annotations_need_apply(&meta, MANAGER, &desired(&[])));
    }

    // reading a subresource entry as ours would prune metadata on the strength of who owns
    // spec.replicas
    #[test]
    fn subresource_entries_are_ignored() {
        let mut meta = meta(&[], &[]);
        meta.managed_fields
            .as_mut()
            .unwrap()
            .push(ManagedFieldsEntry {
                manager: Some(MANAGER.into()),
                subresource: Some("scale".into()),
                fields_v1: Some(FieldsV1(json!({
                    "f:metadata": { "f:annotations": { "f:ghost": {} } },
                }))),
                ..Default::default()
            });

        assert!(!annotations_need_apply(&meta, MANAGER, &desired(&[])));
    }

    #[test]
    fn labels_follow_the_same_rules() {
        let mut meta = ObjectMeta {
            labels: Some(desired(&[("app", "greeter"), ("extra", "kept")])),
            managed_fields: Some(vec![ManagedFieldsEntry {
                manager: Some(MANAGER.into()),
                fields_v1: Some(FieldsV1(json!({
                    "f:metadata": { "f:labels": { "f:app": {} } },
                }))),
                ..Default::default()
            }]),
            ..Default::default()
        };

        assert!(!labels_need_apply(
            &meta,
            MANAGER,
            &desired(&[("app", "greeter")])
        ));

        meta.labels = Some(desired(&[("app", "other")]));
        assert!(labels_need_apply(
            &meta,
            MANAGER,
            &desired(&[("app", "greeter")])
        ));
    }
}
