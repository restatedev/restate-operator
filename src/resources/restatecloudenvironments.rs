use std::{borrow::Cow, collections::BTreeMap};

use k8s_openapi::api::core::v1::{
    Affinity, EnvVar, PodDNSConfig, ResourceRequirements, Secret, Toleration,
};
use kube::{
    runtime::reflector::{ObjectRef, Store},
    CELSchema, CustomResource,
};
use schemars::{
    schema::{Schema, SchemaObject},
    JsonSchema,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use url::Url;

pub static RESTATE_CLOUD_ENVIRONMENT_FINALIZER: &str = "cloudenvironments.restate.dev";

/// Represents the configuration of a Restate Cloud environment
#[derive(CustomResource, CELSchema, Deserialize, Serialize, Clone, Debug)]
#[kube(
    kind = "RestateCloudEnvironment",
    group = "restate.dev",
    version = "v1beta1",
    schema = "manual"
)]
#[kube(shortname = "rce")]
#[serde(rename_all = "camelCase")]
pub struct RestateCloudEnvironmentSpec {
    /// The ID of your environment, which begins `env_`
    pub environment_id: String,
    /// The short region identifier of your environment, eg `us`, `eu`.
    pub region: String,
    /// The request signing public key of your environment, which begins `publickeyv1_`. It is not a secret.
    pub signing_public_key: String,
    /// Where to get credentials for communication with the Cloud environment
    pub authentication: RestateCloudEnvironmentAuthentication,
    /// Optional configuration for the deployment of tunnel pods
    pub tunnel: Option<TunnelSpec>,
}

// Hoisted from the derived implementation so that we can restrict names to be valid Service names
impl schemars::JsonSchema for RestateCloudEnvironment {
    fn schema_name() -> String {
        "RestateCloudEnvironment".to_owned()
    }
    fn schema_id() -> Cow<'static, str> {
        "restate_operator::resources::restatecloudenvironments::RestateCloudEnvironment".into()
    }
    fn json_schema(gen: &mut schemars::gen::SchemaGenerator) -> Schema {
        {
            let mut schema_object = SchemaObject {
                instance_type: Some(
                    schemars::schema::InstanceType::Object.into(),
                ),
                metadata: Some(Box::new(schemars::schema::Metadata {
                    description: Some(
                        "RestateCloudEnvironment configures a connection between this Kubernetes cluster and a Restate Cloud Environment."
                            .to_owned(),
                    ),
                    ..Default::default()
                })),
                ..Default::default()
            };
            let object_validation = schema_object.object();

            object_validation
                .properties
                .insert(
                    "metadata".to_owned(),
                    serde_json::from_value(json!({
                                "type": "object",
                                "properties": {
                                    "name": {
                                        "type": "string",
                                        "minLength": 1,
                                        "maxLength": (63 - "tunnel-".len()),
                                        "pattern": "^[a-z0-9]([-a-z0-9]*[a-z0-9])?(\\.[a-z0-9]([-a-z0-9]*[a-z0-9])?)*$",
                                    }
                                }
                            })).unwrap(),
                );
            object_validation.required.insert("metadata".to_owned());

            object_validation.properties.insert(
                "spec".to_owned(),
                gen.subschema_for::<RestateCloudEnvironmentSpec>(),
            );
            object_validation.required.insert("spec".to_owned());

            Schema::Object(schema_object)
        }
    }
}

impl RestateCloudEnvironment {
    pub fn admin_url(&self) -> Result<Url, url::ParseError> {
        let unprefixed_env = self
            .spec
            .environment_id
            .strip_prefix("env_")
            // if there is no env_ prefix just use it as is
            .unwrap_or(self.spec.environment_id.as_str());

        Url::parse(&format!(
            "https://{}.env.{}.restate.cloud:9070",
            unprefixed_env, self.spec.region
        ))
    }

    pub fn tunnel_url(&self, service_url: Url) -> Result<Url, url::ParseError> {
        let unprefixed_env = self
            .spec
            .environment_id
            .strip_prefix("env_")
            // if there is no env_ prefix just use it as is
            .unwrap_or(self.spec.environment_id.as_str());

        let tunnel_name = self
            .metadata
            .uid
            .as_deref()
            .expect("RestateCloudEnvironment should have a uid");

        let port = service_url
            .port_or_known_default()
            .expect("service url should have a port");

        let host = service_url
            .host_str()
            .expect("service url should have a host");

        let mut url = Url::parse(&format!(
            "https://tunnel.{}.restate.cloud:9080/{unprefixed_env}/{tunnel_name}/{}/{}/{}/",
            self.spec.region,
            service_url.scheme(),
            host,
            port,
        ))?
        .join(service_url.path().trim_start_matches("/"))?;
        url.set_query(service_url.query());
        url.set_fragment(service_url.fragment());

        Ok(url)
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

/// Configuration for authentication to the Cloud environment. A secret reference is currently required, but
/// a CSI Secret Store provider can be used to sync this secret.
#[derive(Deserialize, Serialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RestateCloudEnvironmentAuthentication {
    /// A reference to a secret that will be used for registration, as well as being mounted to the tunnel pods
    /// unless a secretProvider is also specified.
    pub secret: SecretReference,
    /// A reference to a SecretProviderClass that should be mounted to the tunnel pods to authenticate the tunnel.
    /// A Kubernetes Secret (synced by the Secret Store CSI Driver) is still necessary
    /// for the operator to register services.
    pub secret_provider: Option<SecretProviderReference>,
}

/// Configured a reference to a secret in the same namespace as the operator
#[derive(Deserialize, Serialize, Clone, Debug, JsonSchema)]
pub struct SecretReference {
    /// The name of the referenced secret. It must be in the same namespace as the operator.
    pub name: String,
    /// The key to read from the referenced Secret
    pub key: String,
}

#[derive(Deserialize, Serialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SecretProviderReference {
    /// The name of the referenced SecretProviderClass. It must be in the same namespace as the operator.
    pub secret_provider_class: String,
    /// The path that the token will be available inside the secret volume
    pub path: String,
}

#[derive(Deserialize, Serialize, Clone, Debug, CELSchema)]
#[serde(rename_all = "camelCase")]
pub struct TunnelSpec {
    /// If true, the tunnel pods will expose unauthenticated access to the Restate Cloud environment on ports 8080 (ingress) and 9070 (admin).
    /// Care should be taken to restrict inbound access to the tunnel pods if this is set. Defaults to false.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_proxy: Option<bool>,
    /// replicas is the desired number of tunnel pods. If unspecified, defaults to 1.
    pub replicas: Option<i32>,
    /// Container image name. Defaults to a suggested version of the ghcr.io/restatedev/restate-cloud-tunnel-client
    pub image: Option<String>,
    /// Image pull policy. One of Always, Never, IfNotPresent. Defaults to Always if :latest tag is specified, or IfNotPresent otherwise. More info: https://kubernetes.io/docs/concepts/containers/images#updating-images
    pub image_pull_policy: Option<String>,
    /// List of environment variables to set in the container; these may override defaults
    #[schemars(default, schema_with = "env_schema")]
    pub env: Option<Vec<EnvVar>>,
    /// Compute Resources for the tunnel pods. More info: https://kubernetes.io/docs/concepts/configuration/manage-resources-containers/
    pub resources: Option<ResourceRequirements>,
    /// Specifies the DNS parameters of the tunnel pod. Parameters specified here will be merged to the generated DNS configuration based on DNSPolicy.
    pub dns_config: Option<PodDNSConfig>,
    /// Set DNS policy for the pod. Defaults to "ClusterFirst". Valid values are 'ClusterFirstWithHostNet', 'ClusterFirst', 'Default' or 'None'. DNS parameters given in DNSConfig will be merged with the policy selected with DNSPolicy.
    pub dns_policy: Option<String>,
    /// If specified, the pod's tolerations.
    pub tolerations: Option<Vec<Toleration>>,
    /// If specified, a node selector for the pod
    #[schemars(default, schema_with = "node_selector_schema")]
    pub node_selector: Option<BTreeMap<String, String>>,
    /// If specified, pod affinity. Defaults to zone anti-affinity, provide {} to disable all affinity
    pub affinity: Option<Affinity>,
}

fn env_schema(g: &mut schemars::gen::SchemaGenerator) -> Schema {
    serde_json::from_value(json!({
        "items": EnvVar::json_schema(g),
        "nullable": true,
        "type": "array",
        "x-kubernetes-list-map-keys": ["name"],
        "x-kubernetes-list-type": "map"
    }))
    .unwrap()
}

fn node_selector_schema(_g: &mut schemars::gen::SchemaGenerator) -> Schema {
    serde_json::from_value(json!({
        "description": "If specified, a node selector for the pod",
        "additionalProperties": {
            "type": "string"
        },
        "type": "object",
        "x-kubernetes-map-type": "atomic"
    }))
    .unwrap()
}
