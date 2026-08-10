//! Change predicates for the controllers' watch streams.
//!
//! Every owned stream wants one. Without a predicate we re-enqueue the owner on any watch event,
//! including the bare resourceVersion bumps our own writes produce, which is a write -> watch ->
//! reconcile -> write loop (#138). Hash the fields the reconciler actually reads, nothing else.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use k8s_openapi::api::apps::v1::{ReplicaSet, ReplicaSetStatus, StatefulSet, StatefulSetStatus};
use k8s_openapi::api::core::v1::{ConfigMap, Service, ServiceSpec};
use kube::core::object::HasStatus;
use kube::{Resource, ResourceExt};
use serde::Serialize;

// deletion apparently doesn't lead to any change in metadata otherwise, which means the
// changed_predicate would drop them.
pub(crate) fn ensure_deletion_change<K: Resource, E>(
    mut event: Result<kube::runtime::watcher::Event<K>, E>,
) -> Result<kube::runtime::watcher::Event<K>, E> {
    if let Ok(kube::runtime::watcher::Event::Delete(ref mut object)) = event {
        let meta = object.meta_mut();
        meta.generation = match meta.generation {
            Some(val) => Some(val + 1),
            None => Some(0),
        }
    }
    event
}

pub(crate) fn changed_predicate<K: Resource>(obj: &K) -> Option<u64> {
    let mut hasher = DefaultHasher::new();
    if let Some(g) = obj.meta().generation {
        // covers spec but not metadata or status
        g.hash(&mut hasher)
    }
    obj.labels().hash(&mut hasher);
    obj.annotations().hash(&mut hasher);
    // ignore status
    Some(hasher.finish())
}

pub(crate) fn status_predicate<K: Resource + HasStatus>(obj: &K) -> Option<u64>
where
    K::Status: Hash,
{
    let mut hasher = DefaultHasher::new();
    if let Some(s) = obj.status() {
        s.hash(&mut hasher)
    }
    Some(hasher.finish())
}

/// `HasStatus` for the k8s-openapi types, which don't implement kube's version of it.
pub(crate) trait HasStatusField {
    type Status;

    fn status(&self) -> Option<&Self::Status>;
}

impl HasStatusField for StatefulSet {
    type Status = StatefulSetStatus;

    fn status(&self) -> Option<&Self::Status> {
        self.status.as_ref()
    }
}

impl HasStatusField for ReplicaSet {
    type Status = ReplicaSetStatus;

    fn status(&self) -> Option<&Self::Status> {
        self.status.as_ref()
    }
}

pub(crate) fn status_predicate_serde<K: Resource + HasStatusField>(obj: &K) -> Option<u64>
where
    K::Status: Serialize,
{
    let mut hasher = DefaultHasher::new();
    if let Some(s) = obj.status() {
        serde_hashkey::to_key(s)
            .expect("serde_hashkey never to return an error")
            .hash(&mut hasher);
    }
    Some(hasher.finish())
}

pub(crate) trait HasSpecField {
    type Spec;

    fn spec(&self) -> &Self::Spec;
}

impl HasSpecField for Service {
    type Spec = Option<ServiceSpec>;

    fn spec(&self) -> &Self::Spec {
        &self.spec
    }
}

impl HasSpecField for ConfigMap {
    type Spec = Option<std::collections::BTreeMap<String, String>>;

    fn spec(&self) -> &Self::Spec {
        &self.data
    }
}

pub(crate) fn spec_predicate<K: Resource + HasSpecField>(obj: &K) -> Option<u64>
where
    K::Spec: Hash,
{
    let mut hasher = DefaultHasher::new();
    obj.spec().hash(&mut hasher);
    Some(hasher.finish())
}

pub(crate) fn spec_predicate_serde<K: Resource + HasSpecField>(obj: &K) -> Option<u64>
where
    K::Spec: Serialize,
{
    let mut hasher = DefaultHasher::new();
    serde_hashkey::to_key(obj.spec())
        .expect("serde_hashkey never to return an error")
        .hash(&mut hasher);
    Some(hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ManagedFieldsEntry, ObjectMeta, Time};
    use kube::runtime::Predicate;

    fn replicaset(resource_version: &str, ready: i32) -> ReplicaSet {
        ReplicaSet {
            metadata: ObjectMeta {
                name: Some("greeter-abc123".into()),
                namespace: Some("default".into()),
                generation: Some(1),
                resource_version: Some(resource_version.into()),
                managed_fields: Some(vec![ManagedFieldsEntry {
                    manager: Some("restate-operator/propagate-replicas".into()),
                    time: Some(Time(chrono::Utc::now())),
                    ..Default::default()
                }]),
                ..Default::default()
            },
            spec: None,
            status: Some(ReplicaSetStatus {
                replicas: 2,
                ready_replicas: Some(ready),
                available_replicas: Some(ready),
                ..Default::default()
            }),
        }
    }

    // #138: the replicaset we write comes back with nothing changed but resourceVersion (and, on
    // the scale subresource, reshuffled managedFields). that must not count as a change, or our
    // own writes re-trigger us forever.
    #[test]
    fn replicaset_resource_version_bump_alone_is_not_a_change() {
        let predicate = changed_predicate.combine(status_predicate_serde);

        let before = predicate.hash_property(&replicaset("1000", 2));
        let after = predicate.hash_property(&replicaset("1001", 2));

        assert_eq!(before, after);
    }

    // but we read readyReplicas/availableReplicas to decide whether a version can be registered,
    // so a real status change has to get through
    #[test]
    fn replicaset_status_change_is_a_change() {
        let predicate = changed_predicate.combine(status_predicate_serde);

        let unready = predicate.hash_property(&replicaset("1000", 0));
        let ready = predicate.hash_property(&replicaset("1000", 2));

        assert_ne!(unready, ready);
    }

    // annotations carry the deployment id and the removal schedule, so they're in the hash even
    // though they're metadata
    #[test]
    fn replicaset_annotation_change_is_a_change() {
        let predicate = changed_predicate.combine(status_predicate_serde);

        let mut annotated = replicaset("1000", 2);
        let before = predicate.hash_property(&annotated);

        annotated.metadata.annotations =
            Some([("restate.dev/deployment-id".to_owned(), "dp_1".to_owned())].into());

        assert_ne!(before, predicate.hash_property(&annotated));
    }

    // a delete leaves metadata untouched, so without the fixup it hashes the same as the last
    // update and the owner never learns its replicaset is gone
    #[test]
    fn deletion_is_forced_to_look_like_a_change() {
        let predicate = changed_predicate.combine(status_predicate_serde);
        let rs = replicaset("1000", 2);
        let before = predicate.hash_property(&rs);

        let event: Result<_, std::convert::Infallible> =
            Ok(kube::runtime::watcher::Event::Delete(rs));
        let Ok(kube::runtime::watcher::Event::Delete(deleted)) = ensure_deletion_change(event)
        else {
            panic!("expected a Delete event");
        };

        assert_ne!(before, predicate.hash_property(&deleted));
    }
}
