use std::fmt::Display;

use kube::{
    CustomResource, KubeSchema,
    runtime::reflector::{ObjectRef, Store},
};
use schemars::{JsonSchema, Schema};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::{
    controllers::service_url,
    resources::{
        restatecloudenvironments::RestateCloudEnvironment,
        restatedeployments::{PodTemplateMetadata, ServiceReference},
    },
};

/// The directory the operator mounts every configuration file under. The container's
/// `CONFIG_FILE` is a comma-separated list of paths inside it.
pub const CONFIG_DIR: &str = "/etc/restate-kafka";

/// The ConfigMap key (and mounted file name) the operator writes the resolved Restate ingress
/// location under. It is listed *first* in `CONFIG_FILE`, so any `spec.config` entry a user
/// adds can override it.
pub const RESTATE_CONFIG_KEY: &str = "restate.properties";

/// The default key read from a `secretRef` / `configMapRef` `spec.config` entry when the
/// entry does not name one.
pub const CONFIG_FILE_KEY: &str = "config.properties";

/// RestateKafkaIntegration runs the Restate Kafka ingress integration
/// (`ghcr.io/restatedev/ingress-integration-kafka`), which consumes Kafka topics and turns
/// each record into a Restate invocation.
///
/// The operator only models where Restate is and how to authenticate to it; everything else
/// (Kafka connection, consumer settings, record mapper, metrics, retry policy) is the
/// container's own configuration surface and is passed through verbatim as one or more
/// `.properties` files via `spec.config`.
#[derive(CustomResource, Deserialize, Serialize, Clone, Debug, KubeSchema)]
#[kube(
    kind = "RestateKafkaIntegration",
    group = "restate.dev",
    version = "v1alpha1",
    namespaced,
    scale = r#"{"specReplicasPath": ".spec.replicas", "statusReplicasPath": ".status.replicas", "labelSelectorPath": ".status.labelSelector"}"#,
    printcolumn = r#"{"name":"Desired", "type":"integer", "jsonPath":".spec.replicas"}"#,
    printcolumn = r#"{"name":"Ready", "type":"integer", "jsonPath":".status.readyReplicas"}"#,
    printcolumn = r#"{"name":"Available", "type":"integer", "jsonPath":".status.availableReplicas"}"#,
    printcolumn = r#"{"name":"Age", "type":"date", "jsonPath":".metadata.creationTimestamp"}"#,
    printcolumn = r#"{"name":"Ingress", "type":"string", "jsonPath":".status.ingressUrl", "priority": 1}"#,
    printcolumn = r#"{"name":"Status", "type":"string", "jsonPath":".status.conditions[?(@.type==\"Ready\")].message", "priority": 1}"#
)]
#[kube(status = "RestateKafkaIntegrationStatus", shortname = "rki")]
#[serde(rename_all = "camelCase")]
pub struct RestateKafkaIntegrationSpec {
    /// Number of desired pods. Defaults to 1.
    ///
    /// Kafka distributes the subscribed partitions across the whole consumer group, so
    /// throughput scales with replicas up to the partition count; replicas beyond that sit
    /// idle. Note that each pod itself runs `restate.kafka.consumer.instances` consumers
    /// (defaulting to twice the container's CPU count).
    #[schemars(default = "default_replicas", range(min = 0))]
    pub replicas: i32,

    /// Container image name. Defaults to a suggested version of
    /// ghcr.io/restatedev/ingress-integration-kafka.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,

    /// Image pull policy. One of Always, Never, IfNotPresent. Defaults to Always if :latest
    /// tag is specified, or IfNotPresent otherwise.
    /// More info: https://kubernetes.io/docs/concepts/containers/images#updating-images
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_pull_policy: Option<String>,

    /// Where to send the invocations produced from Kafka records, and how to authenticate.
    pub restate: RestateIngressSpec,

    /// Inline configuration for the integration, in Java `.properties` format.
    ///
    /// Every option documented at
    /// https://github.com/restatedev/ingress-integration-kafka/blob/main/CONFIGURATION.md
    /// is accepted, using the property-key spelling (not the environment-variable one).
    /// At a minimum the integration needs `bootstrap.servers`, `group.id` and `topics`.
    ///
    /// The operator stores this in a ConfigMap it owns and mounts it as a config file, merged
    /// after its own resolved `restate.ingress.url` (so this can override the ingress URL) and
    /// before `spec.configRefs` (so a ref can override this). Editing it rolls the pods.
    ///
    /// This is part of the custom resource, so it is stored and displayed in plain text - keep
    /// credentials (`sasl.jaas.config`, `sasl.password`, ...) in a `spec.configRefs` Secret,
    /// and the Restate ingress bearer token in `spec.restate.authToken`, never here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<String>,

    /// Additional configuration sources, read from Secrets or ConfigMaps in this namespace and
    /// merged on top of `spec.config` in order - a later ref overrides an earlier one, and any
    /// ref overrides `spec.config`.
    ///
    /// Each entry is a `secretRef` or a `configMapRef`; use a `secretRef` for anything
    /// sensitive. The operator does not read these objects, so changing their contents does not
    /// restart the pods; run `kubectl rollout restart deployment/<name>` afterwards.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub config_refs: Vec<ConfigRef>,

    /// Overrides applied on top of the pod template the operator generates.
    ///
    /// This is a partial pod template, strategic-merged over the generated one: objects are
    /// merged key by key, and lists whose Kubernetes merge key the operator knows
    /// (`containers`, `initContainers`, `volumes`, `env`, `volumeMounts`, `ports`,
    /// `imagePullSecrets`, `hostAliases`) are merged entry by entry, so you can override a
    /// single field without restating the rest. Any other list replaces the generated one,
    /// and an explicit `null` removes a generated field.
    ///
    /// The operator's own container is named `kafka-integration`; target it by name to set
    /// resources, probes, extra environment variables or volume mounts. The generated pod
    /// deliberately has no readiness or liveness probe (the image exposes no health endpoint
    /// and exits non-zero on unrecoverable errors), so add one here if you want one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub template: Option<PodTemplateOverlay>,
}

/// A partial pod template, merged over the one the operator generates.
///
/// Both halves are optional: unlike a RestateDeployment's `spec.template`, this is an overlay
/// rather than the whole template, so setting only `metadata.labels` is valid.
#[derive(Deserialize, Serialize, Clone, Debug, JsonSchema)]
pub struct PodTemplateOverlay {
    /// Labels and annotations to merge into the generated pod template's metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<PodTemplateMetadata>,

    /// Pod spec fields to merge over the generated pod spec.
    ///
    /// Passed through verbatim, so it accepts any field the target Kubernetes version
    /// understands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(default, schema_with = "pod_spec_overlay_schema")]
    pub spec: Option<serde_json::Value>,
}

fn pod_spec_overlay_schema(_g: &mut schemars::SchemaGenerator) -> Schema {
    schemars::json_schema!({
        "description": "Pod spec fields to merge over the generated pod spec. Passed through verbatim, so it accepts any field the target Kubernetes version understands.",
        "nullable": true,
        "x-kubernetes-preserve-unknown-fields": true
    })
}

fn default_replicas() -> i32 {
    1
}

/// Where to send the invocations produced from Kafka records, and how to authenticate.
#[derive(Deserialize, Serialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RestateIngressSpec {
    /// The location of the Restate ingress to send invocations to.
    pub ingress: RestateIngressEndpoint,

    /// A Secret key in this namespace holding a bearer token for the Restate ingress,
    /// supplied to the container as the `RESTATE_AUTH_TOKEN` environment variable.
    ///
    /// This is where the token belongs: it stays in a Secret and is never written into the
    /// plain-text `.properties` files. The container prefers the environment variable over any
    /// `restate.auth.token` in a config file, so nothing in `spec.config` can shadow it.
    ///
    /// Required when `ingress.cloud` is set; optional (and usually unnecessary) otherwise.
    /// The operator never reads this Secret - it is referenced from the pod spec, so the
    /// kubelet resolves it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_token: Option<SecretKeyRef>,
}

/// A reference to a single key of a Secret in the same namespace as the RestateKafkaIntegration
#[derive(Deserialize, Serialize, Clone, Debug, JsonSchema)]
pub struct SecretKeyRef {
    /// The name of the referenced Secret. It must be in the same namespace as this resource.
    pub name: String,
    /// The key to read from the referenced Secret
    pub key: String,
}

/// One additional source of `.properties` configuration: a Secret key or a ConfigMap key.
#[derive(Deserialize, Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ConfigRef {
    /// A Secret in this namespace holding the `.properties` configuration.
    /// Exactly one of `secretRef` or `configMapRef` must be specified.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret_ref: Option<ConfigKeyRef>,
    /// A ConfigMap in this namespace holding the `.properties` configuration.
    /// Exactly one of `secretRef` or `configMapRef` must be specified.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_map_ref: Option<ConfigKeyRef>,
}

// Custom JsonSchema implementation so that exactly one of secretRef, configMapRef is required.
impl JsonSchema for ConfigRef {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ConfigRef".into()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> Schema {
        let mut secret_schema = generator.subschema_for::<ConfigKeyRef>();
        secret_schema.insert(
            "description".into(),
            serde_json::Value::String(
                "A Secret in this namespace holding the `.properties` configuration. Exactly one of `secretRef` or `configMapRef` must be specified".into(),
            ),
        );
        let mut config_map_schema = generator.subschema_for::<ConfigKeyRef>();
        config_map_schema.insert(
            "description".into(),
            serde_json::Value::String(
                "A ConfigMap in this namespace holding the `.properties` configuration. Exactly one of `secretRef` or `configMapRef` must be specified".into(),
            ),
        );

        schemars::json_schema!({
            "description": "One additional source of `.properties` configuration: a Secret key or a ConfigMap key",
            "properties": {
                "secretRef": secret_schema,
                "configMapRef": config_map_schema
            },
            "oneOf": [
                {"required": ["secretRef"]},
                {"required": ["configMapRef"]}
            ],
            "type": "object"
        })
    }
}

impl ConfigRef {
    /// The referenced object, or an error if not exactly one of the two was given.
    pub fn resolve(&self) -> crate::Result<ConfigRefKind<'_>> {
        match (self.secret_ref.as_ref(), self.config_map_ref.as_ref()) {
            (Some(secret), None) => Ok(ConfigRefKind::Secret(secret)),
            (None, Some(config_map)) => Ok(ConfigRefKind::ConfigMap(config_map)),
            _ => Err(crate::Error::InvalidRestateConfig(
                "Exactly one of `secretRef` or `configMapRef` must be specified in each spec.configRefs entry"
                    .into(),
            )),
        }
    }
}

/// The resolved form of a [`ConfigRef`]
#[derive(Debug, Clone, Copy)]
pub enum ConfigRefKind<'a> {
    Secret(&'a ConfigKeyRef),
    ConfigMap(&'a ConfigKeyRef),
}

/// A reference to a single key of a Secret or ConfigMap in the same namespace
#[derive(Deserialize, Serialize, Clone, Debug, JsonSchema)]
pub struct ConfigKeyRef {
    /// The name of the referenced object. It must be in the same namespace as this resource.
    pub name: String,
    /// The key to read the `.properties` configuration from. Defaults to `config.properties`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

impl ConfigKeyRef {
    pub fn key(&self) -> &str {
        self.key.as_deref().unwrap_or(CONFIG_FILE_KEY)
    }
}

/// The location of the Restate ingress to send invocations to
#[derive(Deserialize, Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct RestateIngressEndpoint {
    /// The name of a RestateCluster whose ingress to send invocations to.
    /// Exactly one of `cluster`, `cloud`, `service` or `url` must be specified
    pub cluster: Option<String>,
    /// The name of a RestateCloudEnvironment whose ingress to send invocations to.
    /// Requires `spec.restate.authToken`.
    /// Exactly one of `cluster`, `cloud`, `service` or `url` must be specified
    pub cloud: Option<String>,
    /// A reference to a Service hosting the Restate ingress.
    /// Exactly one of `cluster`, `cloud`, `service` or `url` must be specified
    pub service: Option<ServiceReference>,
    /// A url of the Restate ingress to send invocations to.
    /// Exactly one of `cluster`, `cloud`, `service` or `url` must be specified
    pub url: Option<Url>,
}

impl Display for RestateIngressEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (&self.cluster, &self.cloud, &self.service, &self.url) {
            (Some(cluster), _, _, _) => write!(f, "RestateCluster/{cluster}"),
            (_, Some(cloud), _, _) => write!(f, "RestateCloudEnvironment/{cloud}"),
            (_, _, Some(service), _) => write!(f, "Service/{}/{}", service.namespace, service.name),
            (_, _, _, Some(url)) => write!(f, "{url}"),
            _ => write!(f, "N/A"),
        }
    }
}

// Custom JsonSchema implementation so that we can make one of cluster, cloud, service, url
// required. Mirrors RestateAdminEndpoint, which resolves the admin API rather than the ingress.
impl JsonSchema for RestateIngressEndpoint {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "RestateIngressEndpoint".into()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> Schema {
        let mut service_schema = generator.subschema_for::<ServiceReference>();
        service_schema.insert(
            "description".into(),
            serde_json::Value::String(
                "A reference to a Service hosting the Restate ingress. If `port` is unset it defaults to 8080. Exactly one of `cluster`, `cloud`, `service` or `url` must be specified".into(),
            ),
        );

        schemars::json_schema!({
            "description": "The location of the Restate ingress to send invocations to",
            "properties": {
                "cluster": {
                    "description": "The name of a RestateCluster whose ingress to send invocations to. Exactly one of `cluster`, `cloud`, `service` or `url` must be specified",
                    "type": "string"
                },
                "cloud": {
                    "description": "The name of a RestateCloudEnvironment whose ingress to send invocations to. Requires `spec.restate.authToken`. Exactly one of `cluster`, `cloud`, `service` or `url` must be specified",
                    "type": "string"
                },
                "service": service_schema,
                "url": {
                    "description": "A url of the Restate ingress to send invocations to. Exactly one of `cluster`, `cloud`, `service` or `url` must be specified",
                    "type": "string"
                }
            },
            "oneOf": [
                {"required": ["cluster"]},
                {"required": ["cloud"]},
                {"required": ["service"]},
                {"required": ["url"]}
            ],
            "type": "object"
        })
    }
}

/// The default port the Restate ingress is served on
pub const RESTATE_INGRESS_PORT: i32 = 8080;

impl RestateIngressEndpoint {
    /// The resolved ingress URL, which the operator writes as `restate.ingress.url` into the
    /// first (lowest-precedence) config file it mounts.
    pub fn ingress_url(
        &self,
        rce_store: &Store<RestateCloudEnvironment>,
        cluster_dns: &str,
    ) -> crate::Result<Url> {
        match (
            self.cluster.as_deref(),
            self.cloud.as_deref(),
            self.service.as_ref(),
            self.url.as_ref(),
        ) {
            // A RestateCluster named X runs a Service named `restate` in namespace X, serving
            // the ingress on 8080.
            (Some(cluster), None, None, None) => Ok(service_url(
                "restate",
                cluster,
                RESTATE_INGRESS_PORT,
                None,
                cluster_dns,
            )?),
            (None, Some(cloud), None, None) => {
                let Some(rce) = rce_store.get(&ObjectRef::new(cloud)) else {
                    return Err(crate::Error::RestateCloudEnvironmentNotFound(cloud.into()));
                };

                Ok(rce.ingress_url()?)
            }
            (None, None, Some(service), None) => Ok(service_url(
                &service.name,
                &service.namespace,
                service.port.unwrap_or(RESTATE_INGRESS_PORT),
                service.path.as_deref(),
                cluster_dns,
            )?),
            (None, None, None, Some(url)) => Ok(url.clone()),
            _ => Err(crate::Error::InvalidRestateConfig(
                "Exactly one of `cluster`, `cloud`, `service` or `url` must be specified in spec.restate.ingress"
                    .into(),
            )),
        }
    }

    /// The RestateCluster this integration talks to, if any. Its pods are labelled
    /// `allow.restate.dev/<cluster>` so that the cluster's NetworkPolicies can select them.
    pub fn cluster(&self) -> Option<&str> {
        self.cluster.as_deref()
    }
}

/// Status of the RestateKafkaIntegration
/// This is set and managed automatically by the controller
#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RestateKafkaIntegrationStatus {
    /// Total number of non-terminated pods targeted by this RestateKafkaIntegration
    pub replicas: i32,

    /// Total number of ready pods
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ready_replicas: Option<i32>,

    /// Total number of available pods (ready for at least minReadySeconds)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub available_replicas: Option<i32>,

    /// Total number of unavailable pods
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unavailable_replicas: Option<i32>,

    /// The generation observed by the controller
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,

    /// The resolved Restate ingress URL the integration was configured with
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ingress_url: Option<String>,

    /// The label selector of this RestateKafkaIntegration as a string, for the scale subresource
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label_selector: Option<String>,

    /// Represents the latest available observations of current state
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conditions: Option<Vec<RestateKafkaIntegrationCondition>>,
}

/// A condition of a RestateKafkaIntegration
#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RestateKafkaIntegrationCondition {
    /// Last time the condition transitioned from one status to another.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_transition_time: Option<k8s_openapi::apimachinery::pkg::apis::meta::v1::Time>,

    /// Human-readable message indicating details about last transition.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,

    /// Unique, one-word, CamelCase reason for the condition's last transition.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,

    /// Status is the status of the condition. Can be True, False, Unknown.
    pub status: String,

    /// Type of the condition, known values are (`Ready`).
    pub r#type: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use kube::runtime::reflector;

    fn empty_rce_store() -> Store<RestateCloudEnvironment> {
        reflector::store::<RestateCloudEnvironment>().0
    }

    fn endpoint(json: serde_json::Value) -> RestateIngressEndpoint {
        serde_json::from_value(json).expect("endpoint deserializes")
    }

    #[test]
    fn cluster_resolves_to_the_ingress_port() {
        let url = endpoint(serde_json::json!({"cluster": "my-cluster"}))
            .ingress_url(&empty_rce_store(), "cluster.local")
            .unwrap();
        assert_eq!(
            url.as_str(),
            "http://restate.my-cluster.svc.cluster.local:8080/"
        );
    }

    #[test]
    fn service_defaults_to_the_ingress_port() {
        let url =
            endpoint(serde_json::json!({"service": {"name": "restate", "namespace": "other"}}))
                .ingress_url(&empty_rce_store(), "cluster.local")
                .unwrap();
        assert_eq!(url.as_str(), "http://restate.other.svc.cluster.local:8080/");
    }

    #[test]
    fn service_honours_an_explicit_port_and_path() {
        let url = endpoint(serde_json::json!({
            "service": {"name": "restate", "namespace": "other", "port": 9090, "path": "/ingress"}
        }))
        .ingress_url(&empty_rce_store(), "cluster.local")
        .unwrap();
        assert_eq!(
            url.as_str(),
            "http://restate.other.svc.cluster.local:9090/ingress"
        );
    }

    #[test]
    fn url_is_passed_through() {
        let url = endpoint(serde_json::json!({"url": "https://restate.example:8080/"}))
            .ingress_url(&empty_rce_store(), "cluster.local")
            .unwrap();
        assert_eq!(url.as_str(), "https://restate.example:8080/");
    }

    #[test]
    fn cloud_requires_the_environment_to_exist() {
        let err = endpoint(serde_json::json!({"cloud": "my-env"}))
            .ingress_url(&empty_rce_store(), "cluster.local")
            .expect_err("no such RestateCloudEnvironment");
        assert!(matches!(
            err,
            crate::Error::RestateCloudEnvironmentNotFound(name) if name == "my-env"
        ));
    }

    #[test]
    fn exactly_one_reference_is_required() {
        for json in [
            serde_json::json!({}),
            serde_json::json!({"cluster": "a", "url": "http://b:8080/"}),
            serde_json::json!({"cluster": "a", "cloud": "b"}),
        ] {
            let err = endpoint(json.clone())
                .ingress_url(&empty_rce_store(), "cluster.local")
                .expect_err(&format!("{json}"));
            assert!(matches!(err, crate::Error::InvalidRestateConfig(_)));
        }
    }

    #[test]
    fn config_ref_resolves_exactly_one_reference() {
        let secret: ConfigRef =
            serde_json::from_value(serde_json::json!({"secretRef": {"name": "s"}})).unwrap();
        assert!(matches!(
            secret.resolve().unwrap(),
            ConfigRefKind::Secret(r) if r.name == "s" && r.key() == "config.properties"
        ));

        let config_map: ConfigRef = serde_json::from_value(
            serde_json::json!({"configMapRef": {"name": "c", "key": "other.properties"}}),
        )
        .unwrap();
        assert!(matches!(
            config_map.resolve().unwrap(),
            ConfigRefKind::ConfigMap(r) if r.name == "c" && r.key() == "other.properties"
        ));

        for json in [
            serde_json::json!({}),
            serde_json::json!({"secretRef": {"name": "s"}, "configMapRef": {"name": "c"}}),
        ] {
            let entry: ConfigRef = serde_json::from_value(json).unwrap();
            assert!(matches!(
                entry.resolve(),
                Err(crate::Error::InvalidRestateConfig(_))
            ));
        }
    }
}
