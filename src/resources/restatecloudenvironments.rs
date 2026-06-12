use std::{borrow::Cow, collections::BTreeMap};

use k8s_openapi::api::core::v1::{
    Affinity, EnvVar, PodDNSConfig, ResourceRequirements, Secret, Toleration,
};
use kube::{
    CustomResource, KubeSchema,
    runtime::reflector::{ObjectRef, Store},
};
use schemars::{JsonSchema, Schema};
use serde::{Deserialize, Serialize};
use url::Url;

pub static RESTATE_CLOUD_ENVIRONMENT_FINALIZER: &str = "cloudenvironments.restate.dev";

/// Represents the configuration of a Restate Cloud environment
#[derive(CustomResource, KubeSchema, Deserialize, Serialize, Clone, Debug)]
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
    fn schema_name() -> Cow<'static, str> {
        "RestateCloudEnvironment".into()
    }
    fn schema_id() -> Cow<'static, str> {
        "restate_operator::resources::restatecloudenvironments::RestateCloudEnvironment".into()
    }
    fn json_schema(generator: &mut schemars::SchemaGenerator) -> Schema {
        let spec_schema = generator.subschema_for::<RestateCloudEnvironmentSpec>();

        schemars::json_schema!({
            "type": "object",
            "description": "RestateCloudEnvironment configures a connection between this Kubernetes cluster and a Restate Cloud Environment.",
            "properties": {
                "metadata": {
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "minLength": 1,
                            "maxLength": (63 - "tunnel-".len()),
                            "pattern": "^[a-z0-9]([-a-z0-9]*[a-z0-9])?(\\.[a-z0-9]([-a-z0-9]*[a-z0-9])?)*$"
                        }
                    }
                },
                "spec": spec_schema
            },
            "required": ["metadata", "spec"]
        })
    }
}

impl RestateCloudEnvironment {
    pub fn unprefixed_environment_id(&self) -> &str {
        self.spec
            .environment_id
            .strip_prefix("env_")
            // if there is no env_ prefix just use it as is
            .unwrap_or(self.spec.environment_id.as_str())
    }

    pub fn admin_url(&self) -> Result<Url, url::ParseError> {
        Url::parse(&format!(
            "https://{}.env.{}.restate.cloud:9070",
            self.unprefixed_environment_id(),
            self.spec.region
        ))
    }

    pub fn tunnel_url(&self, service_url: Url) -> Result<Url, url::ParseError> {
        let unprefixed_env = self.unprefixed_environment_id();

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

/// The RestateCloudEnvironment-derived values a `tunnelMode: in-process` deployment
/// needs. They serve three purposes that must stay consistent within one revision:
/// they are injected into the pods as `RESTATE_INPROC_*` environment variables, folded
/// into the revision hash (so a change mints a new revision), and used to build the
/// tunnel URL the revision is registered under.
#[derive(Clone, Debug)]
pub struct InProcessTunnelParams {
    pub environment_id: String,
    pub region: String,
    pub signing_public_key: String,
}

impl InProcessTunnelParams {
    /// Resolve and validate the values from a RestateCloudEnvironment. In-process
    /// tunnel clients (e.g. @restatedev/restate-sdk-tunnel) validate these strictly,
    /// so an RCE field that deviates from its documented shape must fail the
    /// reconcile here — the alternative is pods that crash-loop on the injected
    /// values while the RestateDeployment shows nothing but "not ready".
    pub fn from_rce(rce: &RestateCloudEnvironment) -> Result<Self, crate::Error> {
        let rce_name = rce.metadata.name.as_deref().unwrap_or("<unnamed>");

        // Normalized to the env_-prefixed spelling: the RCE tolerates an unprefixed
        // id, but the injected value and the tunnel handshake use the prefixed form.
        let unprefixed = rce.unprefixed_environment_id();
        if unprefixed.is_empty()
            || !unprefixed
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(crate::Error::InvalidRestateConfig(format!(
                "tunnelMode: in-process requires RestateCloudEnvironment '{rce_name}' to have an environmentId of the form env_<alphanumerics>, got {:?}",
                rce.spec.environment_id,
            )));
        }
        let environment_id = format!("env_{unprefixed}");

        // The region becomes DNS labels in `tunnel.{region}.restate.cloud`, so it
        // may be multi-label — e.g. a BYOC region like
        // "inl4edhpbxasp9yuz1n0yvvkme.byoc". Allow lowercase letters, digits, '-'
        // and '.', but reject empty labels (leading/trailing or consecutive dots)
        // which would yield an invalid tunnel hostname.
        let region = rce.spec.region.clone();
        let region_ok = !region.is_empty()
            && !region.starts_with('.')
            && !region.ends_with('.')
            && !region.contains("..")
            && region
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.');
        if !region_ok {
            return Err(crate::Error::InvalidRestateConfig(format!(
                "tunnelMode: in-process requires RestateCloudEnvironment '{rce_name}' to have a lowercase DNS-style region (letters, digits, '-', '.'), got {:?}",
                rce.spec.region,
            )));
        }

        let signing_public_key = rce.spec.signing_public_key.clone();
        if !signing_public_key.starts_with("publickeyv1_") {
            return Err(crate::Error::InvalidRestateConfig(format!(
                "tunnelMode: in-process requires RestateCloudEnvironment '{rce_name}' to have a signingPublicKey starting with publickeyv1_, got {:?}",
                rce.spec.signing_public_key,
            )));
        }

        Ok(Self {
            environment_id,
            region,
            signing_public_key,
        })
    }

    pub fn unprefixed_environment_id(&self) -> &str {
        self.environment_id
            .strip_prefix("env_")
            // if there is no env_ prefix just use it as is
            .unwrap_or(self.environment_id.as_str())
    }

    /// The URL to register for a deployment whose pods hold their own tunnel
    /// connections under `tunnel_name`. The tunnel server routes purely by the
    /// `{environment}/{tunnel_name}` key; the `/http/in-process/9080/` destination
    /// segment is a constant shared with in-process tunnel clients (e.g.
    /// @restatedev/restate-sdk-tunnel), which strip it without ever dialing it.
    pub fn tunnel_url(
        &self,
        tunnel_name: &str,
        service_path: Option<&str>,
    ) -> Result<Url, crate::Error> {
        let url = Url::parse(&format!(
            "https://tunnel.{}.restate.cloud:9080/{}/{tunnel_name}/http/in-process/9080/",
            self.region,
            self.unprefixed_environment_id(),
        ))?;

        match service_path {
            Some(path) => {
                let joined = url.join(path.trim_start_matches('/'))?;
                // Url::join resolves dot-segments and absolute references, so a
                // hostile-but-CRD-valid servicePath ("http://elsewhere", "../..")
                // could silently rewrite the base — and with it the rendezvous
                // key the pods actually hold. The path may only append.
                if !joined.as_str().starts_with(url.as_str()) {
                    return Err(crate::Error::InvalidRestateConfig(format!(
                        "spec.restate.servicePath {path:?} escapes the tunnel URL; it must be a plain path to append"
                    )));
                }
                Ok(joined)
            }
            None => Ok(url),
        }
    }
}

/// Configuration for authentication to the Cloud environment. Currently, only secret references are supported and one must be provided.
#[derive(Deserialize, Serialize, Clone, Debug, JsonSchema)]
pub struct RestateCloudEnvironmentAuthentication {
    pub secret: SecretReference,
}

/// Configured a reference to a secret in the same namespace as the operator
#[derive(Deserialize, Serialize, Clone, Debug, JsonSchema)]
pub struct SecretReference {
    /// The name of the referenced secret. It must be in the same namespace as the operator.
    pub name: String,
    /// The key to read from the referenced Secret
    pub key: String,
}

#[derive(Deserialize, Serialize, Clone, Debug, KubeSchema)]
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

fn env_schema(g: &mut schemars::SchemaGenerator) -> Schema {
    let env_var_schema = g.subschema_for::<EnvVar>();
    schemars::json_schema!({
        "items": env_var_schema,
        "nullable": true,
        "type": "array",
        "x-kubernetes-list-map-keys": ["name"],
        "x-kubernetes-list-type": "map"
    })
}

fn node_selector_schema(_g: &mut schemars::SchemaGenerator) -> Schema {
    schemars::json_schema!({
        "description": "If specified, a node selector for the pod",
        "additionalProperties": {
            "type": "string"
        },
        "type": "object",
        "x-kubernetes-map-type": "atomic"
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> InProcessTunnelParams {
        InProcessTunnelParams {
            environment_id: "env_123abc".into(),
            region: "us".into(),
            signing_public_key: "publickeyv1_abc".into(),
        }
    }

    #[test]
    fn in_process_tunnel_url() {
        assert_eq!(
            params()
                .tunnel_url("greeter-5b8c7d", None)
                .unwrap()
                .as_str(),
            "https://tunnel.us.restate.cloud:9080/123abc/greeter-5b8c7d/http/in-process/9080/"
        );
    }

    #[test]
    fn in_process_tunnel_url_tolerates_unprefixed_environment_id() {
        let mut params = params();
        params.environment_id = "123abc".into();
        assert_eq!(
            params.tunnel_url("greeter-5b8c7d", None).unwrap().as_str(),
            "https://tunnel.us.restate.cloud:9080/123abc/greeter-5b8c7d/http/in-process/9080/"
        );
    }

    #[test]
    fn in_process_tunnel_url_appends_service_path() {
        for service_path in ["/api/restate", "api/restate"] {
            assert_eq!(
                params()
                    .tunnel_url("greeter-5b8c7d", Some(service_path))
                    .unwrap()
                    .as_str(),
                "https://tunnel.us.restate.cloud:9080/123abc/greeter-5b8c7d/http/in-process/9080/api/restate"
            );
        }
    }

    #[test]
    fn in_process_tunnel_url_rejects_escaping_service_paths() {
        // Url::join resolves these into a different base — the registered URL
        // would no longer contain the rendezvous key the pods hold
        for service_path in [
            "http://elsewhere.example",
            "../..",
            "../../../otherenv/othertunnel/http/in-process/9080/",
        ] {
            let err = params()
                .tunnel_url("greeter-5b8c7d", Some(service_path))
                .expect_err(service_path);
            assert!(matches!(err, crate::Error::InvalidRestateConfig(_)));
        }
    }

    fn rce(
        environment_id: &str,
        region: &str,
        signing_public_key: &str,
    ) -> RestateCloudEnvironment {
        RestateCloudEnvironment::new(
            "my-env",
            RestateCloudEnvironmentSpec {
                environment_id: environment_id.into(),
                region: region.into(),
                signing_public_key: signing_public_key.into(),
                authentication: RestateCloudEnvironmentAuthentication {
                    secret: SecretReference {
                        name: "secret".into(),
                        key: "token".into(),
                    },
                },
                tunnel: None,
            },
        )
    }

    #[test]
    fn from_rce_normalizes_the_environment_id() {
        // both spellings inject the env_-prefixed form the tunnel handshake uses
        for id in ["env_123abc", "123abc"] {
            let params =
                InProcessTunnelParams::from_rce(&rce(id, "us", "publickeyv1_abc")).unwrap();
            assert_eq!(params.environment_id, "env_123abc");
        }
    }

    #[test]
    fn from_rce_accepts_multi_label_byoc_region() {
        // BYOC environments use a multi-label region (dots); it still forms a
        // valid tunnel.{region}.restate.cloud host.
        let params = InProcessTunnelParams::from_rce(&rce(
            "env_123abc",
            "inl4edhpbxasp9yuz1n0yvvkme.byoc",
            "publickeyv1_abc",
        ))
        .unwrap();
        assert_eq!(params.region, "inl4edhpbxasp9yuz1n0yvvkme.byoc");
        assert_eq!(
            params.tunnel_url("greeter-5b8c7d", None).unwrap().as_str(),
            "https://tunnel.inl4edhpbxasp9yuz1n0yvvkme.byoc.restate.cloud:9080/123abc/greeter-5b8c7d/http/in-process/9080/"
        );
    }

    #[test]
    fn from_rce_rejects_values_in_process_clients_would_crash_on() {
        // in-process tunnel clients validate these on startup; a bad value must
        // fail the reconcile instead of crash-looping the pods
        for (id, region, key) in [
            ("", "us", "publickeyv1_abc"),
            ("env_", "us", "publickeyv1_abc"),
            ("env_a b", "us", "publickeyv1_abc"),
            ("env_123abc", "", "publickeyv1_abc"),
            ("env_123abc", "US", "publickeyv1_abc"),
            ("env_123abc", "us/extra", "publickeyv1_abc"),
            ("env_123abc", ".us", "publickeyv1_abc"),
            ("env_123abc", "us.", "publickeyv1_abc"),
            ("env_123abc", "us..eu", "publickeyv1_abc"),
            ("env_123abc", "us", "not-a-key"),
        ] {
            let err = InProcessTunnelParams::from_rce(&rce(id, region, key))
                .expect_err(&format!("({id:?}, {region:?}, {key:?})"));
            assert!(matches!(err, crate::Error::InvalidRestateConfig(_)));
        }
    }
}
