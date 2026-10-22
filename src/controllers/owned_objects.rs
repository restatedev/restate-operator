use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::fmt::Debug;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};

use futures::FutureExt;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::Resource;
use kube::api::{Api, DeleteParams, Patch, PatchParams};
use kube::runtime::reflector::{ObjectRef, Store};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tracing::{debug, trace};

/// Server-side applies and deletes of one kind of object, skipping the ones the watch cache of
/// that kind shows would change nothing.
///
/// An apply is skipped, and the cached object returned in its place, when this process last
/// applied the very same desired object and the cached object is still the one that apply
/// produced. Anything else applies: a different desired object, a cache miss, or any write by
/// anyone else to the object's spec, labels, annotations or owner references (or, for kinds
/// without a `metadata.generation`, to any part of it). Out-of-band drift is therefore repaired
/// as soon as the watch delivers it. Status writes don't count for kinds with a generation, so a
/// busy status doesn't trigger re-applies. Nothing is persisted, so the first apply of each
/// object after a restart always goes to the apiserver.
///
/// A delete is skipped when the cache is synced and holds no such object.
///
/// Without a store, because the kind isn't watched, every write goes to the apiserver.
pub struct OwnedObjects<K: Resource<DynamicType = ()> + 'static> {
    store: Option<Store<K>>,
    /// What this process last applied to each object, and what it looked like afterwards.
    /// Entries for objects deleted by someone else linger until reused; they are tiny.
    applied: Mutex<HashMap<ObjectRef<K>, Applied>>,
}

#[derive(PartialEq, Eq)]
struct Applied {
    desired: u64,
    observed: u64,
}

impl<K: Resource<DynamicType = ()> + 'static> Default for OwnedObjects<K> {
    fn default() -> Self {
        Self {
            store: None,
            applied: Mutex::default(),
        }
    }
}

impl<K> OwnedObjects<K>
where
    K: Resource<DynamicType = ()> + Clone + Debug + Serialize + DeserializeOwned + 'static,
{
    /// `store` must be fed by a watch that sees every object this is used to write.
    pub fn watched(store: Store<K>) -> Self {
        Self {
            store: Some(store),
            applied: Mutex::default(),
        }
    }

    /// The object as the watch cache last saw it.
    pub fn cached(&self, api: &Api<K>, name: &str) -> Option<Arc<K>> {
        self.store.as_ref()?.get(&object_ref(api, name))
    }

    /// Server-side apply `desired` under the `restate-operator` field manager, forcing
    /// conflicts, and return the object as it now stands.
    pub async fn apply(&self, api: &Api<K>, desired: &K) -> Result<Arc<K>, kube::Error> {
        let name = desired
            .meta()
            .name
            .as_deref()
            .expect("an applied object to have a name");
        let key = object_ref(api, name);
        let desired_hash = hash_serialized(desired);

        if let Some(cached) = self.cached(api, name) {
            let unchanged = Applied {
                desired: desired_hash,
                observed: observed(cached.meta()),
            };
            if self.applied.lock().unwrap().get(&key) == Some(&unchanged) {
                trace!("{key} is as last applied; skipping apply");
                return Ok(cached);
            }
        }

        debug!("Applying {key}");
        let params = PatchParams::apply("restate-operator").force();
        let applied = api.patch(name, &params, &Patch::Apply(desired)).await?;
        self.applied.lock().unwrap().insert(
            key,
            Applied {
                desired: desired_hash,
                observed: observed(applied.meta()),
            },
        );
        Ok(Arc::new(applied))
    }

    /// Delete the named object; one that is already gone is not an error.
    pub async fn delete(
        &self,
        api: &Api<K>,
        name: &str,
        params: &DeleteParams,
    ) -> Result<(), kube::Error> {
        let key = object_ref(api, name);
        self.applied.lock().unwrap().remove(&key);

        if let Some(store) = &self.store
            && is_synced(store)
            && store.get(&key).is_none()
        {
            trace!("{key} is not in the cache; skipping delete");
            return Ok(());
        }

        debug!("Ensuring {key} does not exist");
        match api.delete(name, params).await {
            Err(kube::Error::Api(kube::error::ErrorResponse { code: 404, .. })) => Ok(()),
            Err(err) => Err(err),
            Ok(_) => Ok(()),
        }
    }
}

fn object_ref<K: Resource<DynamicType = ()>>(api: &Api<K>, name: &str) -> ObjectRef<K> {
    match api.namespace() {
        Some(namespace) => ObjectRef::new(name).within(namespace),
        None => ObjectRef::new(name),
    }
}

/// Before its initial list completes, a store's misses say nothing about the apiserver.
fn is_synced<K>(store: &Store<K>) -> bool
where
    K: Resource + Clone + 'static,
    K::DynamicType: Eq + Hash + Clone,
{
    store
        .wait_until_ready()
        .now_or_never()
        .is_some_and(|ready| ready.is_ok())
}

/// Changes whenever someone else writes what an apply would overwrite.
fn observed(meta: &ObjectMeta) -> u64 {
    let mut hasher = DefaultHasher::new();
    meta.uid.hash(&mut hasher);
    match meta.generation {
        // The generation moves with the spec but not the status; metadata it doesn't cover.
        Some(generation) => {
            generation.hash(&mut hasher);
            meta.labels.hash(&mut hasher);
            meta.annotations.hash(&mut hasher);
            hash_serialized(&meta.owner_references).hash(&mut hasher);
        }
        None => meta.resource_version.hash(&mut hasher),
    }
    hasher.finish()
}

fn hash_serialized(value: &impl Serialize) -> u64 {
    let mut hasher = DefaultHasher::new();
    serde_json::to_vec(value)
        .expect("Kubernetes objects to serialize")
        .hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::sync::atomic::{AtomicU64, Ordering};

    use http::{Request, Response};
    use k8s_openapi::api::core::v1::ConfigMap;
    use kube::client::Body;
    use kube::runtime::{reflector, watcher};
    use serde_json::json;

    use super::*;

    fn configmap(value: &str, resource_version: Option<u64>) -> ConfigMap {
        serde_json::from_value(json!({
            "apiVersion": "v1", "kind": "ConfigMap",
            "metadata": {
                "name": "config", "namespace": "ns", "uid": "uid",
                "resourceVersion": resource_version.map(|v| v.to_string()),
            },
            "data": { "key": value },
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn writes_only_what_the_cache_says_would_change() {
        let methods = Arc::new(Mutex::new(Vec::new()));
        let resource_version = Arc::new(AtomicU64::new(0));
        let client = {
            let methods = methods.clone();
            kube::Client::new(
                tower::service_fn(move |req: Request<Body>| {
                    methods.lock().unwrap().push(req.method().to_string());
                    let version = resource_version.fetch_add(1, Ordering::SeqCst) + 1;
                    let body = serde_json::to_vec(&configmap("ours", Some(version))).unwrap();
                    async move {
                        Ok::<_, Infallible>(
                            Response::builder()
                                .header("content-type", "application/json")
                                .body(Body::from(body))
                                .unwrap(),
                        )
                    }
                }),
                "ns",
            )
        };
        let api = Api::<ConfigMap>::namespaced(client, "ns");
        let (store, mut writer) = reflector::store();
        writer.apply_watcher_event(&watcher::Event::Init);
        writer.apply_watcher_event(&watcher::Event::InitDone);
        let objects = OwnedObjects::watched(store);
        let desired = configmap("ours", None);

        // not in the cache yet
        objects.apply(&api, &desired).await.unwrap();
        // the watch delivers what that apply produced
        writer.apply_watcher_event(&watcher::Event::Apply(configmap("ours", Some(1))));
        objects.apply(&api, &desired).await.unwrap();
        // someone else edits it, and the next apply repairs that
        writer.apply_watcher_event(&watcher::Event::Apply(configmap("theirs", Some(7))));
        objects.apply(&api, &desired).await.unwrap();
        writer.apply_watcher_event(&watcher::Event::Apply(configmap("ours", Some(2))));
        objects.apply(&api, &desired).await.unwrap();
        // what we want changes
        objects.apply(&api, &configmap("new", None)).await.unwrap();

        objects
            .delete(&api, "absent", &DeleteParams::default())
            .await
            .unwrap();
        objects
            .delete(&api, "config", &DeleteParams::default())
            .await
            .unwrap();

        assert_eq!(
            *methods.lock().unwrap(),
            ["PATCH", "PATCH", "PATCH", "DELETE"]
        );
    }
}
