use k8s_openapi::api::core::v1::ConfigMap;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{Api, DeleteParams, Patch, PatchParams};
use sha2::Digest;
use std::collections::BTreeMap;
use tracing::debug;

use super::object_meta;
use crate::Error;
use crate::controllers::restatekafkaintegration::controller::Context;
use crate::resources::restatekafkaintegrations::CONFIG_FILE_KEY;

/// The annotation carrying a digest of the inline `spec.config`.
///
/// Kubernetes does not restart pods when a mounted ConfigMap changes, so without this an
/// edit to `spec.config` would only reach the pods on their next unrelated restart. Putting
/// the digest on the pod template makes an edit a template change, which rolls them.
pub const CONFIG_HASH_ANNOTATION: &str = "restate.dev/config-hash";

pub fn config_hash(config: &str) -> String {
    let mut hasher = sha2::Sha256::new();
    hasher.update(config.as_bytes());
    // Truncated to 8 bytes: this only has to distinguish revisions of one object's config,
    // and it keeps the annotation readable. Same idiom as the RestateCluster config hash.
    let digest = u64::from_le_bytes(hasher.finalize()[..8].try_into().unwrap());
    format!("{digest:016x}")
}

fn config_map(base_metadata: &ObjectMeta, config: &str) -> ConfigMap {
    ConfigMap {
        metadata: object_meta(base_metadata),
        data: Some(BTreeMap::from_iter([(
            CONFIG_FILE_KEY.to_owned(),
            config.to_owned(),
        )])),
        ..Default::default()
    }
}

/// Apply the ConfigMap backing an inline `spec.config`.
pub async fn apply_config_map(
    ctx: &Context,
    namespace: &str,
    base_metadata: &ObjectMeta,
    config: &str,
) -> Result<(), Error> {
    let name = base_metadata.name.as_ref().unwrap();
    let cm_api: Api<ConfigMap> = Api::namespaced(ctx.client.clone(), namespace);
    let params: PatchParams = PatchParams::apply("restate-operator").force();

    debug!("Applying ConfigMap {name} in namespace {namespace}");
    cm_api
        .patch(
            name,
            &params,
            &Patch::Apply(&config_map(base_metadata, config)),
        )
        .await?;

    Ok(())
}

/// Remove the ConfigMap backing an inline `spec.config`, if there is one.
///
/// Called when `spec.config` is unset, so that switching to `spec.configFrom` does not leave
/// a stale object behind. Deleting the RestateKafkaIntegration itself is handled by the owner
/// reference, not here.
pub async fn delete_config_map(
    ctx: &Context,
    namespace: &str,
    base_metadata: &ObjectMeta,
) -> Result<(), Error> {
    let name = base_metadata.name.as_ref().unwrap();
    let cm_api: Api<ConfigMap> = Api::namespaced(ctx.client.clone(), namespace);

    debug!("Ensuring ConfigMap {name} in namespace {namespace} is absent");
    match cm_api.delete(name, &DeleteParams::default()).await {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(err)) if err.code == 404 => Ok(()),
        Err(err) => Err(err.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_hash_is_stable_and_content_addressed() {
        let a = config_hash("bootstrap.servers=broker:9092\n");
        assert_eq!(a, config_hash("bootstrap.servers=broker:9092\n"));
        assert_ne!(a, config_hash("bootstrap.servers=broker:9093\n"));
        assert_eq!(a.len(), 16);
    }

    #[test]
    fn config_map_carries_the_config_under_the_well_known_key() {
        let meta = ObjectMeta {
            name: Some("orders".into()),
            namespace: Some("apps".into()),
            ..Default::default()
        };
        let cm = config_map(&meta, "group.id=orders\n");
        assert_eq!(cm.metadata.name.as_deref(), Some("orders"));
        assert_eq!(
            cm.data
                .unwrap()
                .get("config.properties")
                .map(String::as_str),
            Some("group.id=orders\n")
        );
    }
}
