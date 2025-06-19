use k8s_openapi::api::core::v1::Secret;
use kube::{
    runtime::reflector::{ObjectRef, Store},
    CELSchema, CustomResource,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub static RESTATE_CLUSTER_FINALIZER: &str = "clusters.restate.dev";

/// Represents the configuration of a Restate Cluster
#[derive(CustomResource, CELSchema, Deserialize, Serialize, Clone, Debug)]
#[kube(
    kind = "RestateCloudCluster",
    group = "restate.dev",
    version = "v1beta1"
)]
#[kube(shortname = "rcc")]
#[serde(rename_all = "camelCase")]
pub struct RestateCloudClusterSpec {
    /// The environment ID of your cluster, which begins `env_`
    pub environment_id: String,
    /// The short region identifier of your cluster, eg `us`, `eu`.
    pub region: String,
    /// Where to get credentials for communication with the Cloud cluster
    pub authentication: RestateCloudClusterAuthentication,
}

impl RestateCloudCluster {
    pub fn admin_url(&self) -> String {
        let unprefixed_env = self
            .spec
            .environment_id
            .strip_prefix("env_")
            // if there is no env_ prefix just use it as is
            .unwrap_or(self.spec.environment_id.as_str());

        format!(
            "https://{}.env.{}.restate.cloud:9070",
            unprefixed_env, self.spec.region
        )
    }

    pub fn bearer_token(
        &self,
        secret_store: &Store<Secret>,
        operator_namespace: &str,
    ) -> Result<String, crate::Error> {
        let secret = secret_store
            .get(&ObjectRef::new(&self.spec.authentication.secret.name).within(operator_namespace))
            .ok_or(crate::Error::SecretNotFound(
                self.spec.authentication.secret.name.clone(),
            ))?;
        let bytes = secret
            .data
            .as_ref()
            .and_then(|data| data.get(&self.spec.authentication.secret.key))
            // we trim because secrets very regularly have trailing newlines
            .map(|token| token.0.trim_ascii().to_vec())
            .ok_or_else(|| {
                crate::Error::SecretKeyNotFound(
                    self.spec.authentication.secret.key.clone(),
                    self.spec.authentication.secret.name.clone(),
                )
            })?;

        match String::from_utf8(bytes) {
            Ok(token) => Ok(token),
            Err(_) => Err(crate::Error::InvalidBearerToken),
        }
    }
}

/// Configuration for authentication to the Cloud cluster. Currently, only secret references are supported and one must be provided.
#[derive(Deserialize, Serialize, Clone, Debug, JsonSchema)]
pub struct RestateCloudClusterAuthentication {
    secret: SecretReference,
}

/// Configured a reference to a secret in the same namespace as the operator
#[derive(Deserialize, Serialize, Clone, Debug, JsonSchema)]
pub struct SecretReference {
    /// The name of the referenced secret. It must be in the same namespace as the operator.
    name: String,
    /// The key to read from the referenced Secret
    key: String,
}
