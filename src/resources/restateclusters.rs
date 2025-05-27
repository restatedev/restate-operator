use std::borrow::Cow;
use std::collections::BTreeMap;
use std::path::PathBuf;

use k8s_openapi::api::core::v1::{
    Affinity, EnvVar, PodDNSConfig, ResourceRequirements, Toleration,
};
use k8s_openapi::api::networking::v1;
use k8s_openapi::api::networking::v1::{NetworkPolicyPeer, NetworkPolicyPort};

use kube::{CELSchema, CustomResource};
use schemars::schema::{Schema, SchemaObject};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

pub static RESTATE_CLUSTER_FINALIZER: &str = "clusters.restate.dev";

/// Represents the configuration of a Restate Cluster
#[derive(CustomResource, CELSchema, Deserialize, Serialize, Clone, Debug)]
#[cfg_attr(test, derive(Default))]
#[kube(
    kind = "RestateCluster",
    group = "restate.dev",
    version = "v1",
    schema = "manual",
    printcolumn = r#"{"name":"Ready", "type":"string", "jsonPath":".status.conditions[?(@.type==\"Ready\")].status"}"#,
    printcolumn = r#"{"name":"Status", "priority": 1, "type":"string", "jsonPath":".status.conditions[?(@.type==\"Ready\")].message"}"#,
    printcolumn = r#"{"name":"Age", "description": "CreationTimestamp is a timestamp representing the server time when this object was created. It is not guaranteed to be set in happens-before order across separate operations. Clients may not set this value. It is represented in RFC3339 form and is in UTC", "type":"date", "jsonPath":".metadata.creationTimestamp"}"#
)]
#[kube(status = "RestateClusterStatus", shortname = "rsc")]
#[serde(rename_all = "camelCase")]
pub struct RestateClusterSpec {
    /// clusterName sets the RESTATE_CLUSTER_NAME environment variable. Defaults to the object name.
    pub cluster_name: Option<String>,
    pub storage: RestateClusterStorage,
    pub compute: RestateClusterCompute,
    pub security: Option<RestateClusterSecurity>,
    /// TOML-encoded Restate config file
    pub config: Option<String>,
}

// Hoisted from the derived implementation so that we can restrict names to be valid namespace names
impl schemars::JsonSchema for RestateCluster {
    fn schema_name() -> String {
        "RestateCluster".to_owned()
    }
    fn schema_id() -> Cow<'static, str> {
        "restate_operator::controller::RestateCluster".into()
    }
    fn json_schema(gen: &mut schemars::gen::SchemaGenerator) -> Schema {
        {
            let mut schema_object = SchemaObject {
                instance_type: Some(
                    schemars::schema::InstanceType::Object.into(),
                ),
                metadata: Some(Box::new(schemars::schema::Metadata {
                    description: Some(
                        "RestateCluster describes the configuration and status of a Restate cluster."
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
                                        "maxLength": 63,
                                        "pattern": "^[a-z0-9]([-a-z0-9]*[a-z0-9])?(\\.[a-z0-9]([-a-z0-9]*[a-z0-9])?)*$",
                                    }
                                }
                            })).unwrap(),
                );
            object_validation.required.insert("metadata".to_owned());

            object_validation
                .properties
                .insert("spec".to_owned(), gen.subschema_for::<RestateClusterSpec>());
            object_validation.required.insert("spec".to_owned());

            object_validation.properties.insert(
                "status".to_owned(),
                gen.subschema_for::<Option<RestateClusterStatus>>(),
            );
            Schema::Object(schema_object)
        }
    }
}

/// Storage configuration
#[derive(Deserialize, Serialize, Clone, Default, Debug, CELSchema)]
#[serde(rename_all = "camelCase")]
pub struct RestateClusterStorage {
    /// storageClassName is the name of the StorageClass required by the claim. More info: https://kubernetes.io/docs/concepts/storage/persistent-volumes#class-1.
    /// This field is immutable
    #[schemars(default)]
    #[cel_validate(rule = Rule::new("self == oldSelf").message("storageClassName is immutable"))]
    pub storage_class_name: Option<String>,
    /// storageRequestBytes is the amount of storage to request in volume claims. It is allowed to increase but not decrease.
    #[schemars(range(min = 1))]
    #[cel_validate(rule = Rule::new("self >= oldSelf").message("storageRequestBytes cannot be decreased"))]
    pub storage_request_bytes: i64,
}

/// Compute configuration
#[derive(Deserialize, Serialize, Clone, Default, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RestateClusterCompute {
    /// replicas is the desired number of Restate nodes. If unspecified, defaults to 1.
    pub replicas: Option<i32>,
    /// Container image name. More info: https://kubernetes.io/docs/concepts/containers/images.
    pub image: String,
    /// Image pull policy. One of Always, Never, IfNotPresent. Defaults to Always if :latest tag is specified, or IfNotPresent otherwise. More info: https://kubernetes.io/docs/concepts/containers/images#updating-images
    pub image_pull_policy: Option<String>,
    /// List of environment variables to set in the container; these may override defaults
    #[schemars(default, schema_with = "env_schema")]
    pub env: Option<Vec<EnvVar>>,
    /// Compute Resources for the Restate container. More info: https://kubernetes.io/docs/concepts/configuration/manage-resources-containers/
    pub resources: Option<ResourceRequirements>,
    /// Specifies the DNS parameters of the Restate pod. Parameters specified here will be merged to the generated DNS configuration based on DNSPolicy.
    pub dns_config: Option<PodDNSConfig>,
    /// Set DNS policy for the pod. Defaults to "ClusterFirst". Valid values are 'ClusterFirstWithHostNet', 'ClusterFirst', 'Default' or 'None'. DNS parameters given in DNSConfig will be merged with the policy selected with DNSPolicy.
    pub dns_policy: Option<String>,
    /// If specified, the pod's tolerations.
    pub tolerations: Option<Vec<Toleration>>,
    // If specified, a node selector for the pod
    #[schemars(default, schema_with = "node_selector_schema")]
    pub node_selector: Option<BTreeMap<String, String>>,
    // If specified, pod affinity
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

/// Security configuration
#[derive(Deserialize, Serialize, Clone, Default, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RestateClusterSecurity {
    /// Annotations to set on the Service created for Restate
    pub service_annotations: Option<BTreeMap<String, String>>,
    /// Annotations to set on the ServiceAccount created for Restate
    pub service_account_annotations: Option<BTreeMap<String, String>>,
    /// If set, create an AWS PodIdentityAssociation using the ACK CRD in order to give the Restate pod access to this role and
    /// allow the cluster to reach the Pod Identity agent.
    pub aws_pod_identity_association_role_arn: Option<String>,
    /// If set, create an AWS SecurityGroupPolicy CRD object to place the Restate pod into these security groups
    pub aws_pod_security_groups: Option<Vec<String>>,
    /// Network peers to allow inbound access to restate ports
    /// If unset, will not allow any new traffic. Set any of these to [] to allow all traffic - not recommended.
    pub network_peers: Option<RestateClusterNetworkPeers>,
    /// If set to true, add a rule to the allow-admin-access NetworkPolicy allowing traffic from this operator.
    /// This is needed when using RestateDeployments which rely on the operator calling the admin API to
    /// register your service.
    /// Defaults to true.
    pub allow_operator_access_to_admin: Option<bool>,
    /// Egress rules to allow the cluster to make outbound requests; this is in addition to the default
    /// of allowing public internet access, cluster DNS access and pods labelled with `allow.restate.dev/<cluster-name>: "true"`.
    /// Providing a single empty rule will allow all outbound traffic - not recommended
    pub network_egress_rules: Option<Vec<NetworkPolicyEgressRule>>,
    /// If set, configure the use of a private key to sign outbound requests from this cluster
    pub request_signing_private_key: Option<RequestSigningPrivateKey>,
}

#[derive(Deserialize, Serialize, Clone, Default, Debug, JsonSchema)]
pub struct RestateClusterNetworkPeers {
    #[schemars(default, schema_with = "network_peers_schema")]
    pub ingress: Option<Vec<NetworkPolicyPeer>>,
    #[schemars(default, schema_with = "network_peers_schema")]
    pub admin: Option<Vec<NetworkPolicyPeer>>,
    #[schemars(default, schema_with = "network_peers_schema")]
    pub metrics: Option<Vec<NetworkPolicyPeer>>,
}

fn network_peers_schema(g: &mut schemars::gen::SchemaGenerator) -> Schema {
    serde_json::from_value(json!({
        "items": NetworkPolicyPeer::json_schema(g),
        "nullable": true,
        "type": "array",
        "x-kubernetes-list-type": "atomic"
    }))
    .unwrap()
}

/// NetworkPolicyEgressRule describes a particular set of traffic that is allowed out of pods matched by a NetworkPolicySpec's podSelector. The traffic must match both ports and to. This type is beta-level in 1.8
#[derive(Deserialize, Serialize, Clone, Default, Debug, JsonSchema)]
pub struct NetworkPolicyEgressRule {
    /// ports is a list of destination ports for outgoing traffic. Each item in this list is combined using a logical OR. If this field is empty or missing, this rule matches all ports (traffic not restricted by port). If this field is present and contains at least one item, then this rule allows traffic only if the traffic matches at least one port in the list.
    #[schemars(default, schema_with = "network_ports_schema")]
    pub ports: Option<Vec<NetworkPolicyPort>>,

    /// to is a list of destinations for outgoing traffic of pods selected for this rule. Items in this list are combined using a logical OR operation. If this field is empty or missing, this rule matches all destinations (traffic not restricted by destination). If this field is present and contains at least one item, this rule allows traffic only if the traffic matches at least one item in the to list.
    #[schemars(default, schema_with = "network_peers_schema")]
    pub to: Option<Vec<NetworkPolicyPeer>>,
}

impl From<NetworkPolicyEgressRule> for v1::NetworkPolicyEgressRule {
    fn from(value: NetworkPolicyEgressRule) -> Self {
        Self {
            ports: value.ports,
            to: value.to,
        }
    }
}

fn network_ports_schema(_: &mut schemars::gen::SchemaGenerator) -> Schema {
    serde_json::from_value(json!({
          "items": {
            "description": "NetworkPolicyPort describes a port to allow traffic on",
            "properties": {
              "endPort": {
                "description": "endPort indicates that the range of ports from port to endPort if set, inclusive, should be allowed by the policy. This field cannot be defined if the port field is not defined or if the port field is defined as a named (string) port. The endPort must be equal or greater than port.",
                "format": "int32",
                "type": "integer"
              },
              "port": {
                "x-kubernetes-int-or-string": true,
                "anyOf": [{"type": "integer"}, {"type": "string"}],
                "description": "port represents the port on the given protocol. This can either be a numerical or named port on a pod. If this field is not provided, this matches all port names and numbers. If present, only traffic on the specified protocol AND port will be matched."
              },
              "protocol": {
                "description": "protocol represents the protocol (TCP, UDP, or SCTP) which traffic must match. If not specified, this field defaults to TCP.",
                "type": "string"
              }
            },
            "type": "object",
          },
          "nullable": true,
          "type": "array",
          "x-kubernetes-list-type": "atomic"
        }))
        .unwrap()
}

/// Configuration for request signing private keys. Exactly one source of 'secret', 'secretProvider'
/// must be provided.
#[derive(Deserialize, Serialize, Clone, Default, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RequestSigningPrivateKey {
    /// The version of Restate request signing that the key is for; currently only "v1" accepted.
    pub version: String,
    /// A Kubernetes Secret source for the private key
    pub secret: Option<SecretSigningKeySource>,
    /// A CSI secret provider source for the private key; will create a SecretProviderClass.
    pub secret_provider: Option<SecretProviderSigningKeySource>,
}

#[derive(Deserialize, Serialize, Clone, Default, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SecretSigningKeySource {
    /// The key of the secret to select from.  Must be a valid secret key.
    pub key: String,
    /// Name of the secret.
    pub secret_name: String,
}

#[derive(Deserialize, Serialize, Clone, Default, Debug, JsonSchema)]
pub struct SecretProviderSigningKeySource {
    /// Configuration for specific provider
    pub parameters: Option<BTreeMap<String, String>>,
    /// Configuration for provider name
    pub provider: Option<String>,
    /// The path of the private key relative to the root of the mounted volume
    pub path: PathBuf,
}

/// Status of the RestateCluster.
/// This is set and managed automatically.
/// Read-only.
/// More info: https://git.k8s.io/community/contributors/devel/sig-architecture/api-conventions.md#spec-and-status
#[derive(Deserialize, Serialize, Clone, Default, Debug, JsonSchema)]
pub struct RestateClusterStatus {
    pub conditions: Option<Vec<RestateClusterCondition>>,
}

#[derive(Deserialize, Serialize, Clone, Default, Debug, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RestateClusterCondition {
    /// Last time the condition transitioned from one status to another.
    pub last_transition_time: Option<k8s_openapi::apimachinery::pkg::apis::meta::v1::Time>,

    /// Human-readable message indicating details about last transition.
    pub message: Option<String>,

    /// Unique, one-word, CamelCase reason for the condition's last transition.
    pub reason: Option<String>,

    /// Status is the status of the condition. Can be True, False, Unknown.
    pub status: String,

    /// Type of the condition, known values are (`Ready`).
    pub r#type: String,
}
