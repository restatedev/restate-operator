//! A small strategic-merge implementation for the pod template overlay.
//!
//! `RestateKafkaIntegration` generates its own pod template and lets the user overlay
//! `spec.template` on top of it. A plain deep merge would be wrong for the lists Kubernetes
//! treats as maps -- overriding a container's image would drop every other container, and
//! setting one environment variable would drop the rest -- so lists whose Kubernetes merge
//! key we know are merged entry by entry instead.
//!
//! This deliberately implements only the subset of strategic-merge-patch semantics that a
//! pod template needs: recursive object merge, `null` deletes a key, known lists merge by
//! their merge key, every other list replaces. `$patch` directives are not supported.

use serde_json::{Map, Value};

/// The Kubernetes merge key for the lists we merge entry by entry, keyed by field name.
///
/// Anything absent from this table is an "atomic" list in Kubernetes terms and is replaced
/// wholesale by the overlay, which is also the safer default for a list we do not recognise.
fn merge_key(field: &str) -> Option<&'static str> {
    match field {
        "containers"
        | "initContainers"
        | "ephemeralContainers"
        | "volumes"
        | "imagePullSecrets"
        | "env" => Some("name"),
        "volumeMounts" => Some("mountPath"),
        "ports" => Some("containerPort"),
        "hostAliases" => Some("ip"),
        _ => None,
    }
}

/// Merge `overlay` onto `base`, returning the result.
///
/// - two objects merge recursively, key by key
/// - a `null` in the overlay removes the key from the base
/// - two lists merge entry by entry when [`merge_key`] knows the field's merge key, matching
///   entries by that key and appending overlay-only entries; otherwise the overlay replaces
/// - anything else: the overlay replaces the base
pub fn strategic_merge(base: Value, overlay: &Value) -> Value {
    match (base, overlay) {
        (Value::Object(base), Value::Object(overlay)) => {
            Value::Object(merge_objects(base, overlay))
        }
        // Not both objects, so there is nothing structural to preserve.
        (_, overlay) => overlay.clone(),
    }
}

fn merge_objects(mut base: Map<String, Value>, overlay: &Map<String, Value>) -> Map<String, Value> {
    for (key, overlay_value) in overlay {
        if overlay_value.is_null() {
            // An explicit null removes a generated field, rather than setting it to null:
            // that is what lets a user drop, say, the operator's securityContext.
            base.remove(key);
            continue;
        }

        let merged = match (base.remove(key), overlay_value) {
            (Some(Value::Object(base_value)), Value::Object(overlay_value)) => {
                Value::Object(merge_objects(base_value, overlay_value))
            }
            (Some(Value::Array(base_items)), Value::Array(overlay_items)) => match merge_key(key) {
                Some(merge_key) => Value::Array(merge_lists(base_items, overlay_items, merge_key)),
                None => Value::Array(overlay_items.clone()),
            },
            (_, overlay_value) => overlay_value.clone(),
        };

        base.insert(key.clone(), merged);
    }

    base
}

/// Merge two lists whose entries are identified by `merge_key`.
///
/// Base order is preserved and overlay-only entries are appended, so an overlay that adds a
/// sidecar does not reorder the operator's own container.
fn merge_lists(mut base: Vec<Value>, overlay: &[Value], merge_key: &str) -> Vec<Value> {
    for overlay_item in overlay {
        let key_value = overlay_item.get(merge_key);

        // An entry without the merge key cannot be matched up, so it can only be appended.
        let existing = key_value.and_then(|key_value| {
            base.iter_mut()
                .find(|base_item| base_item.get(merge_key) == Some(key_value))
        });

        match existing {
            Some(base_item) => {
                *base_item = strategic_merge(base_item.take(), overlay_item);
            }
            None => base.push(overlay_item.clone()),
        }
    }

    base
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn scalars_and_missing_keys_are_taken_from_the_overlay() {
        let merged = strategic_merge(
            json!({"replicas": 1, "paused": false}),
            &json!({"replicas": 3, "minReadySeconds": 5}),
        );
        assert_eq!(
            merged,
            json!({"replicas": 3, "paused": false, "minReadySeconds": 5})
        );
    }

    #[test]
    fn objects_merge_recursively() {
        let merged = strategic_merge(
            json!({"securityContext": {"runAsUser": 1000, "fsGroup": 2000}}),
            &json!({"securityContext": {"runAsUser": 65534}}),
        );
        assert_eq!(
            merged,
            json!({"securityContext": {"runAsUser": 65534, "fsGroup": 2000}})
        );
    }

    #[test]
    fn null_removes_a_generated_field() {
        let merged = strategic_merge(
            json!({"automountServiceAccountToken": false, "securityContext": {"runAsUser": 1000}}),
            &json!({"securityContext": null}),
        );
        assert_eq!(merged, json!({"automountServiceAccountToken": false}));
    }

    #[test]
    fn containers_merge_by_name_without_dropping_the_others() {
        let base = json!({"containers": [
            {"name": "kafka-integration", "image": "upstream:1", "ports": [{"containerPort": 9464, "name": "metrics"}]}
        ]});
        let merged = strategic_merge(
            base,
            &json!({"containers": [
                {"name": "kafka-integration", "image": "mine:2"},
                {"name": "sidecar", "image": "sidecar:1"}
            ]}),
        );
        assert_eq!(
            merged,
            json!({"containers": [
                {"name": "kafka-integration", "image": "mine:2", "ports": [{"containerPort": 9464, "name": "metrics"}]},
                {"name": "sidecar", "image": "sidecar:1"}
            ]})
        );
    }

    #[test]
    fn env_merges_by_name() {
        let base = json!({"containers": [{"name": "c", "env": [
            {"name": "RESTATE_INGRESS_URL", "value": "http://restate:8080/"},
            {"name": "CONFIG_FILE", "value": "/etc/config.properties"}
        ]}]});
        let merged = strategic_merge(
            base,
            &json!({"containers": [{"name": "c", "env": [
                {"name": "CONFIG_FILE", "value": "/somewhere/else.properties"},
                {"name": "JDK_JAVA_OPTIONS", "value": "-Xmx512m"}
            ]}]}),
        );
        assert_eq!(
            merged,
            json!({"containers": [{"name": "c", "env": [
                {"name": "RESTATE_INGRESS_URL", "value": "http://restate:8080/"},
                {"name": "CONFIG_FILE", "value": "/somewhere/else.properties"},
                {"name": "JDK_JAVA_OPTIONS", "value": "-Xmx512m"}
            ]}]})
        );
    }

    #[test]
    fn volumes_and_mounts_merge_by_their_own_keys() {
        let base = json!({
            "volumes": [{"name": "config", "configMap": {"name": "generated"}}],
            "containers": [{"name": "c", "volumeMounts": [
                {"name": "config", "mountPath": "/etc/restate-kafka/config.properties", "readOnly": true}
            ]}]
        });
        let merged = strategic_merge(
            base,
            &json!({
                "volumes": [{"name": "extra-libs", "emptyDir": {}}],
                "containers": [{"name": "c", "volumeMounts": [
                    {"mountPath": "/app/extra-libs", "name": "extra-libs"}
                ]}]
            }),
        );
        assert_eq!(
            merged,
            json!({
                "volumes": [
                    {"name": "config", "configMap": {"name": "generated"}},
                    {"name": "extra-libs", "emptyDir": {}}
                ],
                "containers": [{"name": "c", "volumeMounts": [
                    {"name": "config", "mountPath": "/etc/restate-kafka/config.properties", "readOnly": true},
                    {"mountPath": "/app/extra-libs", "name": "extra-libs"}
                ]}]
            })
        );
    }

    #[test]
    fn unknown_lists_are_replaced_wholesale() {
        let merged = strategic_merge(
            json!({"tolerations": [{"key": "a", "operator": "Exists"}]}),
            &json!({"tolerations": [{"key": "b", "operator": "Exists"}]}),
        );
        assert_eq!(
            merged,
            json!({"tolerations": [{"key": "b", "operator": "Exists"}]})
        );
    }

    #[test]
    fn a_list_entry_without_the_merge_key_is_appended() {
        let merged = strategic_merge(
            json!({"containers": [{"name": "c", "image": "i"}]}),
            &json!({"containers": [{"image": "anonymous"}]}),
        );
        assert_eq!(
            merged,
            json!({"containers": [{"name": "c", "image": "i"}, {"image": "anonymous"}]})
        );
    }

    #[test]
    fn a_list_replacing_a_scalar_just_wins() {
        let merged = strategic_merge(json!({"env": "nonsense"}), &json!({"env": []}));
        assert_eq!(merged, json!({"env": []}));
    }
}
