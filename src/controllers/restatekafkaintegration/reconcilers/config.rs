use k8s_openapi::api::core::v1::ConfigMap;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{Api, Patch, PatchParams};
use sha2::Digest;
use std::collections::BTreeMap;
use tracing::debug;

use super::object_meta;
use crate::Error;
use crate::controllers::restatekafkaintegration::controller::Context;
use crate::resources::restatekafkaintegrations::{CONFIG_FILE_KEY, RESTATE_CONFIG_KEY};

/// The annotation carrying a digest of the ConfigMap the operator owns.
///
/// Kubernetes does not restart pods when a mounted ConfigMap changes, so without this an edit
/// to the resolved ingress URL or to the inline `spec.config` would only reach the pods on
/// their next unrelated restart. Putting the digest on the pod template makes such a change a
/// template change, which rolls them. `spec.configRefs` are not covered: the operator does not
/// read them, so it cannot digest their contents.
pub const CONFIG_HASH_ANNOTATION: &str = "restate.dev/config-hash";

/// The contents of the ConfigMap the operator owns: the resolved Restate ingress location under
/// `restate.properties`, plus the inline `spec.config` under `config.properties` if there is
/// any.
///
/// This is the exact data [`apply_config_map`] writes and [`config_hash`] digests, so an edit
/// to either the ingress URL or the inline config changes the hash and rolls the pods.
pub fn managed_config_data(ingress_url: &str, inline: Option<&str>) -> BTreeMap<String, String> {
    let mut data = BTreeMap::from_iter([(
        RESTATE_CONFIG_KEY.to_owned(),
        format!("restate.ingress.url={ingress_url}\n"),
    )]);

    if let Some(inline) = inline {
        data.insert(CONFIG_FILE_KEY.to_owned(), inline.to_owned());
    }

    data
}

pub fn config_hash(data: &BTreeMap<String, String>) -> String {
    let mut hasher = sha2::Sha256::new();
    // Length-prefixed so that, e.g., moving a `=` between key and value cannot collide. The
    // BTreeMap iterates in key order, so the digest is stable across reconciles.
    for (key, value) in data {
        hasher.update((key.len() as u64).to_le_bytes());
        hasher.update(key.as_bytes());
        hasher.update((value.len() as u64).to_le_bytes());
        hasher.update(value.as_bytes());
    }
    // Truncated to 8 bytes: this only has to distinguish revisions of one object's config,
    // and it keeps the annotation readable. Same idiom as the RestateCluster config hash.
    let digest = u64::from_le_bytes(hasher.finalize()[..8].try_into().unwrap());
    format!("{digest:016x}")
}

fn config_map(base_metadata: &ObjectMeta, data: BTreeMap<String, String>) -> ConfigMap {
    ConfigMap {
        metadata: object_meta(base_metadata),
        data: Some(data),
        ..Default::default()
    }
}

/// Apply the ConfigMap the operator owns.
///
/// Always present, because it always carries at least the resolved `restate.ingress.url`, so
/// there is nothing to delete when `spec.config` is empty - server-side apply removes the
/// inline key if it went away.
pub async fn apply_config_map(
    ctx: &Context,
    namespace: &str,
    base_metadata: &ObjectMeta,
    ingress_url: &str,
    inline: Option<&str>,
) -> Result<(), Error> {
    let name = base_metadata.name.as_ref().unwrap();
    let data = managed_config_data(ingress_url, inline);
    let cm_api: Api<ConfigMap> = Api::namespaced(ctx.client.clone(), namespace);
    let params: PatchParams = PatchParams::apply("restate-operator").force();

    debug!("Applying ConfigMap {name} in namespace {namespace}");
    cm_api
        .patch(
            name,
            &params,
            &Patch::Apply(&config_map(base_metadata, data)),
        )
        .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_data_carries_the_ingress_url_and_inline_config() {
        let with_inline = managed_config_data("http://restate:8080/", Some("group.id=orders\n"));
        assert_eq!(
            with_inline.get("restate.properties").map(String::as_str),
            Some("restate.ingress.url=http://restate:8080/\n")
        );
        assert_eq!(
            with_inline.get("config.properties").map(String::as_str),
            Some("group.id=orders\n")
        );

        // with no inline config, only the ingress file is present
        let without_inline = managed_config_data("http://restate:8080/", None);
        assert_eq!(without_inline.len(), 1);
        assert!(without_inline.contains_key("restate.properties"));
    }

    #[test]
    fn config_hash_is_stable_and_content_addressed() {
        let a = managed_config_data("http://restate:8080/", Some("a=b\n"));
        assert_eq!(config_hash(&a), config_hash(&a.clone()));
        // a different ingress URL changes the hash
        let b = managed_config_data("http://other:8080/", Some("a=b\n"));
        assert_ne!(config_hash(&a), config_hash(&b));
        // a different inline config changes the hash
        let c = managed_config_data("http://restate:8080/", Some("a=c\n"));
        assert_ne!(config_hash(&a), config_hash(&c));
        assert_eq!(config_hash(&a).len(), 16);
    }

    #[test]
    fn config_map_carries_the_managed_data() {
        let meta = ObjectMeta {
            name: Some("orders".into()),
            namespace: Some("apps".into()),
            ..Default::default()
        };
        let data = managed_config_data("http://restate:8080/", Some("group.id=orders\n"));
        let cm = config_map(&meta, data);
        assert_eq!(cm.metadata.name.as_deref(), Some("orders"));
        let cm_data = cm.data.unwrap();
        assert_eq!(
            cm_data.get("restate.properties").map(String::as_str),
            Some("restate.ingress.url=http://restate:8080/\n")
        );
        assert_eq!(
            cm_data.get("config.properties").map(String::as_str),
            Some("group.id=orders\n")
        );
    }
}
