use std::collections::{BTreeMap, HashSet};

use k8s_openapi::{
    api::{
        apps::v1::{Deployment, DeploymentSpec},
        core::v1::{
            CSIVolumeSource, Container, ContainerPort, EnvVar, HTTPGetAction, KeyToPath,
            PodSecurityContext, PodSpec, PodTemplateSpec, Probe, ResourceRequirements,
            SeccompProfile, SecretVolumeSource, SecurityContext, Service, ServicePort, ServiceSpec,
            Volume, VolumeMount,
        },
    },
    apimachinery::pkg::{api::resource::Quantity, util::intstr::IntOrString},
};
use kube::{
    api::{ObjectMeta, Patch, PatchParams},
    Api,
};
use tracing::debug;

use crate::{
    controllers::restatecloudenvironment::{
        controller::Context,
        reconcilers::{label_selector, object_meta},
    },
    resources::restatecloudenvironments::RestateCloudEnvironmentSpec,
    Error,
};

fn default_resources() -> ResourceRequirements {
    ResourceRequirements {
        claims: None,
        limits: Some(BTreeMap::from([
            ("cpu".into(), Quantity("2".into())),
            ("memory".into(), Quantity("2Gi".into())),
        ])),
        requests: Some(BTreeMap::from([
            ("cpu".into(), Quantity("1".into())),
            ("memory".into(), Quantity("1Gi".into())),
        ])),
    }
}

const BEARER_TOKEN_MOUNT_PATH: &str = "/restate-bearer-token";

fn env(tunnel_name: &str, spec: &RestateCloudEnvironmentSpec) -> Vec<EnvVar> {
    let remote_proxy = spec
        .tunnel
        .as_ref()
        .and_then(|t| t.remote_proxy)
        .unwrap_or_default();

    let defaults = [
        ("RESTATE_TUNNEL_NAME", tunnel_name),
        ("RESTATE_SIGNING_PUBLIC_KEY", &spec.signing_public_key),
        ("RESTATE_ENVIRONMENT_ID", &spec.environment_id),
        ("RESTATE_CLOUD_REGION", &spec.region),
        ("RESTATE_BEARER_TOKEN_FILE", BEARER_TOKEN_MOUNT_PATH),
        (
            "RESTATE_REMOTE_PROXY",
            if remote_proxy { "true" } else { "false" },
        ),
        ("RUST_BACKTRACE", "1"),
        ("RUST_LIB_BACKTRACE", "0"),
        ("RUST_LOG", "info"),
    ];

    let custom = spec.tunnel.as_ref().and_then(|t| t.env.as_ref());

    // allow crd to override our defaults
    let custom_names: HashSet<&str> = custom
        .map(|custom| custom.iter().map(|e| e.name.as_ref()).collect())
        .unwrap_or_default();

    let defaults = defaults
        .into_iter()
        .filter(|(k, _)| !custom_names.contains(k))
        .map(|(k, v)| EnvVar {
            name: k.into(),
            value: Some(v.into()),
            value_from: None,
        });

    if let Some(custom) = custom {
        defaults.chain(custom.iter().cloned()).collect()
    } else {
        defaults.collect()
    }
}

fn tunnel_deployment(
    base_metadata: &ObjectMeta,
    tunnel_name: &str,
    spec: &RestateCloudEnvironmentSpec,
    tunnel_client_default_image: &str,
) -> Deployment {
    let metadata = object_meta(base_metadata);
    let labels = metadata.labels.clone();
    let annotations = metadata.annotations.clone();
    let tunnel = spec.tunnel.as_ref();

    let env = env(tunnel_name, spec);

    let ports = if tunnel.and_then(|t| t.remote_proxy).unwrap_or_default() {
        vec![
            ContainerPort {
                name: Some("ingress".into()),
                container_port: 8080,
                ..Default::default()
            },
            ContainerPort {
                name: Some("admin".into()),
                container_port: 9070,
                ..Default::default()
            },
            ContainerPort {
                name: Some("health".into()),
                container_port: 9090,
                ..Default::default()
            },
        ]
    } else {
        vec![ContainerPort {
            name: Some("health".into()),
            container_port: 9090,
            ..Default::default()
        }]
    };

    let (bearer_token_path, secret_volume) =
        if let Some(secret_provider) = &spec.authentication.secret_provider {
            (
                secret_provider.path.clone(),
                Volume {
                    name: "bearer-token".into(),
                    csi: Some(CSIVolumeSource {
                        driver: "secrets-store.csi.k8s.io".into(),
                        read_only: Some(true),
                        volume_attributes: Some(BTreeMap::from([(
                            "secretProviderClass".into(),
                            secret_provider.secret_provider_class.clone(),
                        )])),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            )
        } else {
            (
                "bearer-token".into(),
                Volume {
                    name: "bearer-token".into(),
                    secret: Some(SecretVolumeSource {
                        secret_name: Some(spec.authentication.secret.name.clone()),
                        items: Some(vec![KeyToPath {
                            key: spec.authentication.secret.key.clone(),
                            mode: None,
                            path: "bearer-token".into(),
                        }]),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            )
        };

    Deployment {
        metadata,
        spec: Some(DeploymentSpec {
            replicas: tunnel.and_then(|t| t.replicas),
            selector: label_selector(base_metadata),
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels,
                    annotations,
                    ..Default::default()
                }),
                spec: Some(PodSpec {
                    affinity: tunnel.and_then(|t| t.affinity.clone()),
                    automount_service_account_token: Some(false),
                    dns_policy: tunnel.and_then(|t| t.dns_policy.clone()),
                    dns_config: tunnel.and_then(|t| t.dns_config.clone()),
                    containers: vec![Container {
                        name: "restate-cloud-tunnel-client".into(),
                        image: Some(
                            tunnel
                                .and_then(|t| t.image.as_deref())
                                .unwrap_or(tunnel_client_default_image)
                                .into(),
                        ),
                        image_pull_policy: tunnel.and_then(|t| t.image_pull_policy.clone()),
                        env: Some(env),
                        readiness_probe: Some(Probe {
                            http_get: Some(HTTPGetAction {
                                port: IntOrString::Int(9090),
                                path: Some("/health".into()),
                                ..Default::default()
                            }),
                            ..Default::default()
                        }),
                        ports: Some(ports),
                        resources: Some(
                            tunnel
                                .and_then(|t| t.resources.clone())
                                .unwrap_or_else(default_resources),
                        ),
                        security_context: Some(SecurityContext {
                            read_only_root_filesystem: Some(true),
                            allow_privilege_escalation: Some(false),
                            ..Default::default()
                        }),
                        volume_mounts: Some(vec![VolumeMount {
                            mount_path: BEARER_TOKEN_MOUNT_PATH.into(),
                            name: "bearer-token".into(),
                            sub_path: Some(bearer_token_path),
                            read_only: Some(true),
                            ..Default::default()
                        }]),
                        ..Default::default()
                    }],
                    security_context: Some(PodSecurityContext {
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
                    termination_grace_period_seconds: Some(310),
                    tolerations: tunnel.and_then(|t| t.tolerations.clone()),
                    node_selector: tunnel.and_then(|t| t.node_selector.clone()),
                    volumes: Some(vec![secret_volume]),
                    ..Default::default()
                }),
            },
            ..Default::default()
        }),
        status: None,
    }
}

fn tunnel_service(base_metadata: &ObjectMeta, remote_proxy: bool) -> Service {
    let metadata = object_meta(base_metadata);

    let ports = if remote_proxy {
        vec![
            ServicePort {
                app_protocol: Some("kubernetes.io/h2c".into()),
                port: 8080,
                name: Some("ingress".into()),
                ..Default::default()
            },
            ServicePort {
                app_protocol: Some("kubernetes.io/h2c".into()),
                port: 9070,
                name: Some("admin".into()),
                ..Default::default()
            },
            ServicePort {
                port: 9090,
                name: Some("health".into()),
                ..Default::default()
            },
        ]
    } else {
        vec![ServicePort {
            port: 9090,
            name: Some("health".into()),
            ..Default::default()
        }]
    };

    Service {
        metadata,
        spec: Some(ServiceSpec {
            selector: label_selector(base_metadata).match_labels,
            ports: Some(ports),
            ..Default::default()
        }),
        status: None,
    }
}

async fn apply_deployment(
    namespace: &str,
    dp_api: &Api<Deployment>,
    dp: Deployment,
) -> Result<(), Error> {
    let name = dp.metadata.name.as_ref().unwrap();
    let params: PatchParams = PatchParams::apply("restate-operator").force();
    debug!("Applying Deployment {} in namespace {}", name, namespace);
    dp_api.patch(name, &params, &Patch::Apply(&dp)).await?;
    Ok(())
}

async fn apply_service(namespace: &str, svc_api: &Api<Service>, svc: Service) -> Result<(), Error> {
    let name = svc.metadata.name.as_ref().unwrap();
    let params: PatchParams = PatchParams::apply("restate-operator").force();
    debug!("Applying Service {} in namespace {}", name, namespace);
    svc_api.patch(name, &params, &Patch::Apply(&svc)).await?;
    Ok(())
}

pub async fn reconcile_tunnel(
    ctx: &Context,
    namespace: &str,
    base_metadata: &ObjectMeta,
    tunnel_name: &str,
    spec: &RestateCloudEnvironmentSpec,
) -> Result<(), Error> {
    let dp_api: Api<Deployment> = Api::namespaced(ctx.client.clone(), namespace);
    let svc_api: Api<Service> = Api::namespaced(ctx.client.clone(), namespace);

    apply_deployment(
        namespace,
        &dp_api,
        tunnel_deployment(
            base_metadata,
            tunnel_name,
            spec,
            &ctx.tunnel_client_default_image,
        ),
    )
    .await?;

    apply_service(
        namespace,
        &svc_api,
        tunnel_service(
            base_metadata,
            spec.tunnel
                .as_ref()
                .and_then(|t| t.remote_proxy)
                .unwrap_or_default(),
        ),
    )
    .await?;

    Ok(())
}
