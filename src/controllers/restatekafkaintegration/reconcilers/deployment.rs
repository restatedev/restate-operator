use k8s_openapi::api::apps::v1::{
    Deployment, DeploymentSpec, DeploymentStatus, DeploymentStrategy, RollingUpdateDeployment,
};
use k8s_openapi::api::core::v1::{
    ConfigMapVolumeSource, Container, ContainerPort, EnvVar, EnvVarSource, KeyToPath,
    PodSecurityContext, PodSpec, PodTemplateSpec, SeccompProfile, SecretKeySelector,
    SecretVolumeSource, SecurityContext, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{Api, ApiResource, DynamicObject, Patch, PatchParams};
use serde_json::{Map, Value, json};
use tracing::debug;

use super::config::{CONFIG_HASH_ANNOTATION, config_hash};
use super::{
    CONFIG_VOLUME_NAME, CONTAINER_NAME, METRICS_PORT, label_selector, object_meta, selector_labels,
};
use crate::Error;
use crate::controllers::restatekafkaintegration::controller::Context;
use crate::controllers::restatekafkaintegration::merge::strategic_merge;
use crate::resources::restatekafkaintegrations::{
    CONFIG_FILE_KEY, CONFIG_FILE_PATH, ConfigSourceKind, RestateKafkaIntegrationSpec,
};

/// The subPath the config key is projected to inside the volume, so that the mount lands as a
/// single file at [`CONFIG_FILE_PATH`] rather than turning its parent into a directory.
const CONFIG_SUB_PATH: &str = "config.properties";

/// The environment variables the operator owns.
///
/// The container prefers environment variables over the `.properties` file, which is exactly
/// what we want here: the operator decides where Restate is, and a stale `restate.ingress.url`
/// left in a config file must not silently win. Users who really need to override one of these
/// can do so from `spec.template`, which is merged last.
fn env(spec: &RestateKafkaIntegrationSpec, ingress_url: &str) -> Vec<EnvVar> {
    let mut env = vec![EnvVar {
        name: "RESTATE_INGRESS_URL".into(),
        value: Some(ingress_url.to_owned()),
        value_from: None,
    }];

    if let Some(auth_token) = spec.restate.auth_token.as_ref() {
        env.push(EnvVar {
            name: "RESTATE_AUTH_TOKEN".into(),
            value: None,
            value_from: Some(EnvVarSource {
                secret_key_ref: Some(SecretKeySelector {
                    name: auth_token.name.clone(),
                    key: auth_token.key.clone(),
                    optional: None,
                }),
                ..Default::default()
            }),
        });
    }

    if spec.config.is_some() || spec.config_from.is_some() {
        env.push(EnvVar {
            name: "CONFIG_FILE".into(),
            value: Some(CONFIG_FILE_PATH.to_owned()),
            value_from: None,
        });
    }

    env
}

/// The volume projecting the `.properties` configuration, if there is any.
///
/// Inline `spec.config` comes from the ConfigMap the operator owns (named after the
/// RestateKafkaIntegration); `spec.configFrom` comes from the user's own Secret or ConfigMap.
fn config_volume(name: &str, spec: &RestateKafkaIntegrationSpec) -> Result<Option<Volume>, Error> {
    let items = |key: &str| {
        Some(vec![KeyToPath {
            key: key.to_owned(),
            mode: None,
            path: CONFIG_SUB_PATH.to_owned(),
        }])
    };

    let volume = match (spec.config.as_deref(), spec.config_from.as_ref()) {
        (Some(_), None) => Volume {
            name: CONFIG_VOLUME_NAME.into(),
            config_map: Some(ConfigMapVolumeSource {
                name: name.to_owned(),
                items: items(CONFIG_FILE_KEY),
                ..Default::default()
            }),
            ..Default::default()
        },
        (None, Some(config_from)) => match config_from.resolve()? {
            ConfigSourceKind::Secret(secret) => Volume {
                name: CONFIG_VOLUME_NAME.into(),
                secret: Some(SecretVolumeSource {
                    secret_name: Some(secret.name.clone()),
                    items: items(secret.key()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ConfigSourceKind::ConfigMap(config_map) => Volume {
                name: CONFIG_VOLUME_NAME.into(),
                config_map: Some(ConfigMapVolumeSource {
                    name: config_map.name.clone(),
                    items: items(config_map.key()),
                    ..Default::default()
                }),
                ..Default::default()
            },
        },
        (None, None) => return Ok(None),
        // The CRD's CEL rule rejects this, but an older apiserver or a hand-edited object
        // could still get here, and silently picking one would be worse than saying so.
        (Some(_), Some(_)) => {
            return Err(Error::InvalidRestateConfig(
                "only one of spec.config and spec.configFrom may be set".into(),
            ));
        }
    };

    Ok(Some(volume))
}

/// Build the Deployment the operator wants, before the user's `spec.template` overlay.
///
/// Deliberately absent: any readiness or liveness probe. The image exposes no health
/// endpoint, and it exits non-zero on unrecoverable errors (bad configuration, exhausted
/// reconnect retries), so the restart policy is the real liveness mechanism. A probe on the
/// metrics port would also be wrong the moment someone sets `restate.metrics.enabled=false`.
fn base_deployment(
    base_metadata: &ObjectMeta,
    spec: &RestateKafkaIntegrationSpec,
    ingress_url: &str,
    default_image: &str,
) -> Result<Deployment, Error> {
    let metadata = object_meta(base_metadata);
    let name = base_metadata.name.as_ref().unwrap();

    let mut pod_labels = metadata.labels.clone().unwrap_or_default();
    // The cluster's NetworkPolicies select peers by this label; see the RestateDeployment
    // pods, which are labelled the same way.
    if let Some(cluster) = spec.restate.ingress.cluster() {
        pod_labels.insert(format!("allow.restate.dev/{cluster}"), "true".to_owned());
    }

    let mut pod_annotations = metadata.annotations.clone().unwrap_or_default();
    if let Some(config) = spec.config.as_deref() {
        pod_annotations.insert(CONFIG_HASH_ANNOTATION.to_owned(), config_hash(config));
    }

    let config_volume = config_volume(name, spec)?;
    let volume_mounts = config_volume.as_ref().map(|_| {
        vec![VolumeMount {
            name: CONFIG_VOLUME_NAME.into(),
            mount_path: CONFIG_FILE_PATH.into(),
            sub_path: Some(CONFIG_SUB_PATH.into()),
            read_only: Some(true),
            ..Default::default()
        }]
    });

    Ok(Deployment {
        metadata,
        spec: Some(DeploymentSpec {
            replicas: Some(spec.replicas),
            selector: label_selector(base_metadata),
            // Surging would add consumers to the group before the old ones leave, costing an
            // extra Kafka rebalance on every rollout.
            strategy: Some(DeploymentStrategy {
                type_: Some("RollingUpdate".into()),
                rolling_update: Some(RollingUpdateDeployment {
                    max_surge: Some(
                        k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::Int(0),
                    ),
                    max_unavailable: Some(
                        k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::Int(1),
                    ),
                }),
            }),
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(pod_labels),
                    annotations: Some(pod_annotations),
                    ..Default::default()
                }),
                spec: Some(PodSpec {
                    automount_service_account_token: Some(false),
                    containers: vec![Container {
                        name: CONTAINER_NAME.into(),
                        image: Some(spec.image.as_deref().unwrap_or(default_image).to_owned()),
                        image_pull_policy: spec.image_pull_policy.clone(),
                        env: Some(env(spec, ingress_url)),
                        ports: Some(vec![ContainerPort {
                            name: Some("metrics".into()),
                            container_port: METRICS_PORT,
                            ..Default::default()
                        }]),
                        security_context: Some(SecurityContext {
                            read_only_root_filesystem: Some(true),
                            allow_privilege_escalation: Some(false),
                            ..Default::default()
                        }),
                        volume_mounts,
                        ..Default::default()
                    }],
                    security_context: Some(PodSecurityContext {
                        run_as_non_root: Some(true),
                        run_as_user: Some(1000),
                        run_as_group: Some(3000),
                        fs_group: Some(2000),
                        fs_group_change_policy: Some("OnRootMismatch".into()),
                        seccomp_profile: Some(SeccompProfile {
                            type_: "RuntimeDefault".into(),
                            localhost_profile: None,
                        }),
                        ..Default::default()
                    }),
                    // Long enough for in-flight records to finish and their offsets to be
                    // committed before the consumer is killed.
                    termination_grace_period_seconds: Some(60),
                    volumes: config_volume.map(|volume| vec![volume]),
                    ..Default::default()
                }),
            },
            ..Default::default()
        }),
        status: None,
    })
}

/// Apply the user's `spec.template` on top of the generated pod template.
///
/// The generated template goes through JSON because `spec.template.spec` is an opaque
/// pass-through: it may legitimately carry fields this build of the operator does not know
/// about, so it can never be deserialized into a typed `PodSpec`.
fn apply_template_overlay(
    deployment: &Deployment,
    base_metadata: &ObjectMeta,
    spec: &RestateKafkaIntegrationSpec,
) -> Result<Value, Error> {
    let mut deployment = serde_json::to_value(deployment)?;

    if let Some(template) = spec.template.as_ref() {
        // Built key by key rather than with `json!`, because an absent field must stay absent:
        // a `null` in the overlay *deletes* the generated value, so serializing `None` here
        // would silently strip the labels and annotations the operator just computed.
        let mut metadata = Map::new();
        if let Some(labels) = template.metadata.as_ref().and_then(|m| m.labels.as_ref()) {
            metadata.insert("labels".into(), serde_json::to_value(labels)?);
        }
        if let Some(annotations) = template
            .metadata
            .as_ref()
            .and_then(|m| m.annotations.as_ref())
        {
            metadata.insert("annotations".into(), serde_json::to_value(annotations)?);
        }

        let mut overlay = Map::new();
        if !metadata.is_empty() {
            overlay.insert("metadata".into(), Value::Object(metadata));
        }
        if let Some(pod_spec) = template.spec.as_ref() {
            overlay.insert("spec".into(), pod_spec.clone());
        }

        let merged = strategic_merge(
            deployment["spec"]["template"].take(),
            &Value::Object(overlay),
        );
        deployment["spec"]["template"] = merged;
    }

    // A Deployment whose template labels no longer match its (immutable) selector is
    // rejected outright by the apiserver, so put ours back rather than letting an overlay
    // produce an object that can never be applied.
    let labels = deployment["spec"]["template"]["metadata"]["labels"]
        .as_object_mut()
        .ok_or_else(|| {
            Error::InvalidRestateConfig(
                "spec.template.metadata.labels must be an object".to_owned(),
            )
        })?;
    for (key, value) in selector_labels(base_metadata) {
        labels.insert(key, Value::String(value));
    }

    Ok(deployment)
}

/// Reconcile the Deployment, returning its observed status.
pub async fn reconcile_deployment(
    ctx: &Context,
    namespace: &str,
    base_metadata: &ObjectMeta,
    spec: &RestateKafkaIntegrationSpec,
    ingress_url: &str,
) -> Result<Option<DeploymentStatus>, Error> {
    let name = base_metadata.name.as_ref().unwrap();

    let deployment = base_deployment(
        base_metadata,
        spec,
        ingress_url,
        &ctx.kafka_integration_default_image,
    )?;
    let mut deployment = apply_template_overlay(&deployment, base_metadata, spec)?;

    // The pod template is passed through verbatim, so it cannot round-trip through the typed
    // Deployment; write it as a DynamicObject instead, as the RestateDeployment controller
    // does for its ReplicaSets.
    let resource = ApiResource::erase::<Deployment>(&());
    let types = json!({
        "apiVersion": resource.api_version,
        "kind": resource.kind,
    });
    let deployment = strategic_merge(deployment.take(), &types);

    let dp_api: Api<DynamicObject> = Api::namespaced_with(ctx.client.clone(), namespace, &resource);
    let params: PatchParams = PatchParams::apply("restate-operator").force();

    debug!("Applying Deployment {name} in namespace {namespace}");
    let applied = dp_api
        .patch(name, &params, &Patch::Apply(&deployment))
        .await?;

    // Only the status is read back, and that part is always a well-known shape.
    let applied: Deployment = serde_json::from_value(serde_json::to_value(applied)?)?;

    Ok(applied.status)
}

/// The Deployment's label selector as a string, for `status.labelSelector` / the scale
/// subresource.
pub fn label_selector_string(name: &str) -> String {
    selector_labels(&ObjectMeta {
        name: Some(name.to_owned()),
        ..Default::default()
    })
    .into_iter()
    .map(|(key, value)| format!("{key}={value}"))
    .collect::<Vec<_>>()
    .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta() -> ObjectMeta {
        ObjectMeta {
            name: Some("orders".into()),
            namespace: Some("apps".into()),
            ..Default::default()
        }
    }

    fn spec(json: Value) -> RestateKafkaIntegrationSpec {
        serde_json::from_value(json).expect("spec deserializes")
    }

    fn minimal(extra: Value) -> RestateKafkaIntegrationSpec {
        let mut base = json!({
            "replicas": 1,
            "restate": {"ingress": {"cluster": "my-cluster"}}
        });
        let Value::Object(extra) = extra else {
            panic!("extra must be an object")
        };
        for (key, value) in extra {
            base[key] = value;
        }
        spec(base)
    }

    fn built(spec: &RestateKafkaIntegrationSpec) -> Value {
        let deployment = base_deployment(
            &meta(),
            spec,
            "http://restate.my-cluster.svc.cluster.local:8080/",
            "default-image:1",
        )
        .expect("builds");
        apply_template_overlay(&deployment, &meta(), spec).expect("overlay applies")
    }

    fn container(deployment: &Value) -> &Value {
        &deployment["spec"]["template"]["spec"]["containers"][0]
    }

    #[test]
    fn the_ingress_url_and_defaults_are_set() {
        let deployment = built(&minimal(json!({})));
        let container = container(&deployment);

        assert_eq!(container["name"], "kafka-integration");
        assert_eq!(container["image"], "default-image:1");
        assert_eq!(
            container["env"],
            json!([{
                "name": "RESTATE_INGRESS_URL",
                "value": "http://restate.my-cluster.svc.cluster.local:8080/"
            }])
        );
        // no config source, so no CONFIG_FILE, no volume and no mount
        assert!(container.get("volumeMounts").is_none());
        assert!(
            deployment["spec"]["template"]["spec"]
                .get("volumes")
                .is_none()
        );
        assert_eq!(deployment["spec"]["replicas"], 1);
        assert_eq!(
            deployment["spec"]["strategy"]["rollingUpdate"]["maxSurge"],
            0
        );
    }

    #[test]
    fn the_cluster_reference_labels_the_pods_for_network_policies() {
        let deployment = built(&minimal(json!({})));
        assert_eq!(
            deployment["spec"]["template"]["metadata"]["labels"]["allow.restate.dev/my-cluster"],
            "true"
        );
    }

    #[test]
    fn a_url_reference_does_not_label_the_pods() {
        let spec = spec(json!({
            "replicas": 1,
            "restate": {"ingress": {"url": "https://restate.example:8080/"}}
        }));
        let deployment = built(&spec);
        let labels = &deployment["spec"]["template"]["metadata"]["labels"];
        assert!(labels.get("allow.restate.dev/my-cluster").is_none());
    }

    #[test]
    fn an_auth_token_is_referenced_not_read() {
        let deployment = built(&minimal(
            json!({"restate": {"ingress": {"cloud": "my-env"}, "authToken": {"name": "tok", "key": "k"}}}),
        ));
        let env = &container(&deployment)["env"];
        assert_eq!(
            env[1],
            json!({
                "name": "RESTATE_AUTH_TOKEN",
                "valueFrom": {"secretKeyRef": {"name": "tok", "key": "k"}}
            })
        );
    }

    #[test]
    fn inline_config_mounts_the_operators_configmap_and_hashes_it() {
        let deployment = built(&minimal(json!({"config": "group.id=orders\n"})));
        let container = container(&deployment);

        assert_eq!(
            container["env"][1],
            json!({"name": "CONFIG_FILE", "value": "/etc/restate-kafka/config.properties"})
        );
        assert_eq!(
            container["volumeMounts"],
            json!([{
                "name": "config",
                "mountPath": "/etc/restate-kafka/config.properties",
                "subPath": "config.properties",
                "readOnly": true
            }])
        );
        assert_eq!(
            deployment["spec"]["template"]["spec"]["volumes"],
            json!([{
                "name": "config",
                "configMap": {
                    "name": "orders",
                    "items": [{"key": "config.properties", "path": "config.properties"}]
                }
            }])
        );
        assert_eq!(
            deployment["spec"]["template"]["metadata"]["annotations"]["restate.dev/config-hash"],
            config_hash("group.id=orders\n")
        );
    }

    #[test]
    fn config_from_a_secret_mounts_the_users_secret_and_is_not_hashed() {
        let deployment = built(&minimal(
            json!({"configFrom": {"secretRef": {"name": "kafka-config", "key": "kafka.properties"}}}),
        ));

        assert_eq!(
            deployment["spec"]["template"]["spec"]["volumes"],
            json!([{
                "name": "config",
                "secret": {
                    "secretName": "kafka-config",
                    "items": [{"key": "kafka.properties", "path": "config.properties"}]
                }
            }])
        );
        // we never read the Secret, so we cannot hash its contents
        assert!(
            deployment["spec"]["template"]["metadata"]["annotations"]
                .get("restate.dev/config-hash")
                .is_none()
        );
    }

    #[test]
    fn config_from_a_configmap_defaults_the_key() {
        let deployment = built(&minimal(
            json!({"configFrom": {"configMapRef": {"name": "kafka-config"}}}),
        ));
        assert_eq!(
            deployment["spec"]["template"]["spec"]["volumes"][0]["configMap"],
            json!({
                "name": "kafka-config",
                "items": [{"key": "config.properties", "path": "config.properties"}]
            })
        );
    }

    #[test]
    fn both_config_sources_is_rejected() {
        let spec = minimal(json!({
            "config": "a=b\n",
            "configFrom": {"secretRef": {"name": "s"}}
        }));
        let err = base_deployment(&meta(), &spec, "http://restate:8080/", "img").expect_err("both");
        assert!(matches!(err, Error::InvalidRestateConfig(_)));
    }

    #[test]
    fn the_template_overlay_reaches_the_operators_container() {
        let deployment = built(&minimal(json!({"template": {"spec": {
            "containers": [{
                "name": "kafka-integration",
                "image": "mine:2",
                "resources": {"requests": {"cpu": "500m"}},
                "env": [{"name": "JDK_JAVA_OPTIONS", "value": "-Xmx512m"}]
            }],
            "serviceAccountName": "kafka",
            "nodeSelector": {"kubernetes.io/arch": "arm64"}
        }}})));
        let container = container(&deployment);

        assert_eq!(container["image"], "mine:2");
        assert_eq!(container["resources"]["requests"]["cpu"], "500m");
        // the operator's own env survives, and the user's is appended
        assert_eq!(container["env"][0]["name"], "RESTATE_INGRESS_URL");
        assert_eq!(container["env"][1]["name"], "JDK_JAVA_OPTIONS");
        // the generated port is untouched
        assert_eq!(container["ports"][0]["containerPort"], METRICS_PORT);
        assert_eq!(
            deployment["spec"]["template"]["spec"]["serviceAccountName"],
            "kafka"
        );
    }

    #[test]
    fn the_template_overlay_can_add_a_sidecar_and_a_probe() {
        let deployment = built(&minimal(json!({"template": {"spec": {
            "containers": [
                {"name": "kafka-integration", "readinessProbe": {"tcpSocket": {"port": 9464}}},
                {"name": "sidecar", "image": "sidecar:1"}
            ]
        }}})));

        let containers = &deployment["spec"]["template"]["spec"]["containers"];
        assert_eq!(containers.as_array().unwrap().len(), 2);
        assert_eq!(containers[0]["readinessProbe"]["tcpSocket"]["port"], 9464);
        assert_eq!(containers[1]["name"], "sidecar");
    }

    #[test]
    fn the_template_overlay_cannot_break_the_selector() {
        let deployment = built(&minimal(json!({"template": {"metadata": {
            "labels": {"app.kubernetes.io/name": "something-else", "team": "payments"}
        }}})));
        let labels = &deployment["spec"]["template"]["metadata"]["labels"];

        assert_eq!(
            labels["app.kubernetes.io/name"],
            "restate-kafka-integration"
        );
        assert_eq!(labels["app.kubernetes.io/instance"], "orders");
        // unrelated labels the user added are kept
        assert_eq!(labels["team"], "payments");
    }

    #[test]
    fn the_template_overlay_can_remove_a_generated_field() {
        let deployment = built(&minimal(
            json!({"template": {"spec": {"securityContext": null}}}),
        ));
        assert!(
            deployment["spec"]["template"]["spec"]
                .get("securityContext")
                .is_none()
        );
    }

    #[test]
    fn the_label_selector_string_matches_the_selector() {
        assert_eq!(
            label_selector_string("orders"),
            "app.kubernetes.io/instance=orders,app.kubernetes.io/name=restate-kafka-integration"
        );
    }
}
