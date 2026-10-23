//! The relay: Restate's single endpoint in preflight mode (`:9080/_/preflight/`), the cron
//! (served in process at the plain route `platform/cron`), the tenants' ingress mux (`:8080`),
//! in `included` mode the workflow registry (the registry API `:8090`, the UI's read API
//! `:8091`), and optionally the tunnel receiver (`:19080`), clustered over gossip.

use std::collections::BTreeMap;

use k8s_openapi::api::apps::v1::{StatefulSet, StatefulSetSpec};
use k8s_openapi::api::core::v1::{
    ConfigMap, ConfigMapVolumeSource, Container, ContainerPort, EnvVar, EnvVarSource,
    HTTPGetAction, ObjectFieldSelector, PersistentVolumeClaim, PersistentVolumeClaimSpec,
    PodSecurityContext, PodSpec, PodTemplateSpec, Probe, SecretVolumeSource, Service,
    ServiceAccount, ServicePort, ServiceSpec, TopologySpreadConstraint, Volume, VolumeMount,
    VolumeResourceRequirements,
};
use k8s_openapi::api::policy::v1::{PodDisruptionBudget, PodDisruptionBudgetSpec};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::api::ListParams;
use kube::runtime::reflector::ObjectRef;
use kube::{Api, ResourceExt};
use sha2::Digest;

use crate::Error;
use crate::controllers::restatecluster::controller::Context;
use crate::resources::restateclusters::{RelaySpec, RelayStatus};

use super::cron;
use super::multitenancy::{
    Advertised, Apis, ComponentState, Dns, apply, component_label_selector, component_labels,
    component_selector, delete_if_exists, exposed_service, is_load_balancer, load_balancer_address,
    statefulset_rollout,
};
use super::object_meta;

pub(super) const RELAY: &str = "relay";
const RELAY_HEADLESS: &str = "relay-headless";
const INGRESS_MUX: &str = "ingress-mux";
const RELAY_TUNNEL: &str = "relay-tunnel";

pub(super) const SENDER_PORT: u16 = 9080;
const TUNNEL_PORT: u16 = 19080;
const METRICS_PORT: u16 = 9000;
const MUX_PORT: u16 = 8080;
const GOSSIP_PORT: u16 = 6000;
const CLUSTER_RPC_PORT: u16 = 6001;
const CLUSTER_FORWARD_PORT: u16 = 6002;
/// The registry API (`included` mode): register, list, delete; the lookups.
pub(super) const REGISTRY_API_PORT: u16 = 8090;
/// The web UI's per-scope read API (`included` mode).
pub(super) const REGISTRY_READ_PORT: u16 = 8091;

/// The pods' grace period unless `extraConfig` sets `[limits.drain]
/// termination_grace_period_secs`: the two must be equal.
const DEFAULT_GRACE_PERIOD_SECS: i64 = 70;
const DEFAULT_REPLICAS: i32 = 3;
/// The `state` PVC: the gossip generation and the registry's replica (1M tenants are a few GB).
const DEFAULT_STORAGE: &str = "10Gi";

/// Labels the relay's config ConfigMaps carry, so the old ones can be found and deleted.
const RELAY_CONFIG_LABEL: &str = "restate.dev/relay-config";

const CONFIG_MOUNT: &str = "/etc/relay";
const AUTH_MOUNT: &str = "/etc/relay-auth";
const STATE_MOUNT: &str = "/var/lib/relay";
/// Without a tunnel the relay still needs a receiver listener and an auth backend: it binds
/// loopback, behind no Service, and authorises nobody (this empty keys file).
const NO_TUNNEL_KEYS: &str = "no-tunnel-auth-keys.toml";

/// Where the relay and the mux look the workflows up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RelayRegistry {
    /// A service speaking `02-contract.md` §4 (`external` and `operator` modes).
    Url(String),
    /// The registry in the relay (`included` mode).
    InRelay {
        bucket_url: String,
        shards: Option<i32>,
        /// Serve the web UI's read API (`public_listen`).
        read_api: bool,
    },
}

/// What the relay config is rendered from.
pub(super) struct RelayConfigParams<'a> {
    pub dns: Dns<'a>,
    pub replicas: i32,
    pub registry: &'a RelayRegistry,
    /// The tunnel receiver's public URL, with a tunnel; `None` without one.
    pub tunnel_url: Option<&'a str>,
    pub extra_config: Option<&'a str>,
}

fn table<const N: usize>(entries: [(&str, toml::Value); N]) -> toml::Value {
    toml::Value::Table(
        entries
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect(),
    )
}

fn listen(port: u16) -> toml::Value {
    format!("0.0.0.0:{port}").into()
}

/// The relay's config, from the spec (`testdata/relay-*.toml` are the reference renders). `__NODE_ID__` and
/// `__POD_IP__` are per pod: the container's wrapper substitutes them at start (the relay
/// config has no env interpolation). `extraConfig` is merged last, the user's keys winning.
pub(super) fn render_relay_config(params: &RelayConfigParams<'_>) -> Result<toml::Table, Error> {
    let dns = params.dns;
    let mut config = toml::Table::new();
    let mut set = |key: &str, value: toml::Value| {
        config.insert(key.into(), value);
    };

    // Restate's invokers and admins, via the ClusterIP `relay` Service; no sender auth
    // (in-cluster only, no network policies in v1).
    set("sender_listen", listen(SENDER_PORT));
    set("allow_unauthenticated_sender", true.into());
    // Also how preflight tells a tunnel address from a direct one.
    set("external_proxy_url", dns.url(RELAY, SENDER_PORT).into());

    match params.tunnel_url {
        Some(tunnel_url) => {
            // Tunnel clients dial in here through the relay-tunnel Service, with the Restate
            // Cloud tunnel handshake (`start_tunnel`), so the unmodified SDK tunnel client works.
            set("receiver_listen", listen(TUNNEL_PORT));
            set("receiver_protocol", "start_tunnel".into());
            set("external_tunnel_url", tunnel_url.into());
            set(
                "auth",
                table([(
                    "static",
                    table([("keys_path", format!("{AUTH_MOUNT}/auth-keys.toml").into())]),
                )]),
            );
        }
        None => {
            set("receiver_listen", format!("127.0.0.1:{TUNNEL_PORT}").into());
            set(
                "auth",
                table([(
                    "static",
                    table([(
                        "keys_path",
                        format!("{CONFIG_MOUNT}/{NO_TUNNEL_KEYS}").into(),
                    )]),
                )]),
            );
        }
    }

    // /metrics, /healthz, /readyz
    set("metrics", table([("listen", listen(METRICS_PORT))]));

    // The staged SIGTERM drain, sized for GKE (a terminating pod leaves the Service endpoints
    // within seconds). termination_grace_period_secs is the pods' grace period.
    set(
        "limits",
        table([(
            "drain",
            table([
                ("deregistration_delay_secs", 15.into()),
                ("straggler_window_secs", 5.into()),
                ("drain_timeout_secs", 60.into()),
                (
                    "termination_grace_period_secs",
                    DEFAULT_GRACE_PERIOD_SECS.into(),
                ),
            ]),
        )]),
    );

    let retry = table([
        ("initial_interval_ms", 100.into()),
        ("exponentiation_factor", 2.0.into()),
        ("max_interval_ms", 10000.into()),
        ("max_attempts", 70.into()),
        ("on_max_attempts", "pause".into()),
    ]);
    match params.registry {
        RelayRegistry::Url(url) => set(
            "preflight",
            table([
                ("registry_url", url.as_str().into()),
                ("registry_timeout_ms", 1000.into()),
                (
                    "cache",
                    table([
                        ("deployment_ttl_secs", 3600.into()),
                        ("target_ttl_secs", 30.into()),
                    ]),
                ),
                ("retry", retry),
            ]),
        ),
        RelayRegistry::InRelay {
            bucket_url,
            shards,
            read_api,
        } => {
            // The lookups are answered in process from the replica: no registry client, no
            // caches in front of it.
            set("preflight", table([("retry", retry)]));
            let mut registry = toml::Table::new();
            registry.insert("bucket_url".into(), bucket_url.as_str().into());
            registry.insert(
                "db_path".into(),
                format!("{STATE_MOUNT}/registry.sqlite").into(),
            );
            registry.insert("api_listen".into(), listen(REGISTRY_API_PORT));
            if *read_api {
                registry.insert("public_listen".into(), listen(REGISTRY_READ_PORT));
            }
            if let Some(shards) = shards {
                registry.insert("shards".into(), i64::from(*shards).into());
            }
            set("registry", toml::Value::Table(registry));
        }
    }

    // Gossip membership (which pod holds which tunnel) + cross-pod forwarding. Every pod is a
    // seed, through the headless Service's per-pod DNS.
    let seeds: Vec<toml::Value> = (0..params.replicas)
        .map(|i| format!("{RELAY}-{i}.{}:{GOSSIP_PORT}", dns.host(RELAY_HEADLESS)).into())
        .collect();
    set(
        "cluster",
        table([
            ("node_id", "__NODE_ID__".into()),
            ("cluster_id", format!("{}-relay", dns.namespace).into()),
            ("insecure_gossip_acknowledged", true.into()),
            ("gossip_listen", listen(GOSSIP_PORT)),
            ("cluster_rpc_listen", listen(CLUSTER_RPC_PORT)),
            ("cluster_forward_listen", listen(CLUSTER_FORWARD_PORT)),
            ("advertise_addr", "__POD_IP__".into()),
            (
                "generation_state_path",
                format!("{STATE_MOUNT}/generation.state").into(),
            ),
            ("seeds", seeds.into()),
        ]),
    );

    // Plain routes run the same rewriting as preflight streams: that's the cron's path, served
    // by every relay in process.
    set("service_protocol", table([("policy", "preflight".into())]));
    set(
        "embedded",
        table([("services", toml::Value::Array(vec![cron::CRON.into()]))]),
    );
    set(
        "routing",
        table([(
            "static",
            table([(
                "routes",
                toml::Value::Array(vec![table([
                    ("env", "platform".into()),
                    ("tunnel", cron::CRON.into()),
                    ("embedded", cron::CRON.into()),
                ])]),
            )]),
        )]),
    );

    // The tenants' front door to Restate's ingress; looks the workflows up like preflight does.
    set(
        "ingress_mux",
        table([
            ("listen", listen(MUX_PORT)),
            ("upstream", dns.url("restate", 8080).into()),
            ("admin", dns.url("restate", 9070).into()),
        ]),
    );

    if let Some(extra_config) = params.extra_config {
        let extra: toml::Table = toml::from_str(extra_config).map_err(|e| {
            Error::InvalidRestateConfig(format!(
                "Failed to parse spec.multitenancy.relay.extraConfig: {e}"
            ))
        })?;
        merge_toml(&mut config, extra);
    }

    Ok(config)
}

/// Deep-merge `overlay` into `base`, table by table: the overlay's keys win, its tables are
/// merged into the base's, anything else (arrays included) replaces.
pub(super) fn merge_toml(base: &mut toml::Table, overlay: toml::Table) {
    for (key, value) in overlay {
        match (base.get_mut(&key), value) {
            (Some(toml::Value::Table(base)), toml::Value::Table(overlay)) => {
                merge_toml(base, overlay)
            }
            (_, value) => {
                base.insert(key, value);
            }
        }
    }
}

/// The pods' grace period: `[limits.drain] termination_grace_period_secs`.
fn grace_period_secs(config: &toml::Table) -> i64 {
    config
        .get("limits")
        .and_then(|l| l.get("drain"))
        .and_then(|d| d.get("termination_grace_period_secs"))
        .and_then(|g| g.as_integer())
        .unwrap_or(DEFAULT_GRACE_PERIOD_SECS)
}

/// `relay-config-<hash of the content>`: a new config is a new name in the pod template, which
/// rolls the pods (the relay reads its config once, at start).
fn relay_configmap(
    base_metadata: &ObjectMeta,
    relay_toml: String,
    tunnel: bool,
) -> (ConfigMap, String) {
    let mut data = BTreeMap::from([("relay.toml".to_owned(), relay_toml)]);
    if !tunnel {
        data.insert(
            NO_TUNNEL_KEYS.into(),
            "# No tunnel: no receiver is authorised.\n".into(),
        );
    }

    let mut hasher = sha2::Sha256::new();
    for (key, value) in &data {
        hasher.update(key.as_bytes());
        hasher.update(b"\0");
        hasher.update(value.as_bytes());
        hasher.update(b"\0");
    }
    let hash = format!(
        "{:x}",
        u32::from_le_bytes(hasher.finalize()[..4].try_into().unwrap())
    );

    let mut metadata = object_meta(base_metadata, format!("relay-config-{hash}"));
    metadata
        .labels
        .get_or_insert_with(Default::default)
        .insert(RELAY_CONFIG_LABEL.into(), "true".into());

    (
        ConfigMap {
            metadata,
            data: Some(data),
            ..Default::default()
        },
        hash,
    )
}

fn container_port(name: &str, port: u16, protocol: Option<&str>) -> ContainerPort {
    ContainerPort {
        name: Some(name.into()),
        container_port: port.into(),
        protocol: protocol.map(Into::into),
        ..Default::default()
    }
}

fn service_port(name: &str, port: u16, protocol: Option<&str>) -> ServicePort {
    ServicePort {
        name: Some(name.into()),
        port: port.into(),
        target_port: Some(IntOrString::Int(port.into())),
        protocol: protocol.map(Into::into),
        ..Default::default()
    }
}

fn field_ref_env(name: &str, field_path: &str) -> EnvVar {
    EnvVar {
        name: name.into(),
        value: None,
        value_from: Some(EnvVarSource {
            field_ref: Some(ObjectFieldSelector {
                api_version: None,
                field_path: field_path.into(),
            }),
            ..Default::default()
        }),
    }
}

/// The relay pods' identity: what may reach the registry's bucket (workload identity).
fn relay_service_account(base_metadata: &ObjectMeta, relay: &RelaySpec) -> ServiceAccount {
    let mut metadata = object_meta(base_metadata, RELAY);
    if let Some(annotations) = &relay.service_account_annotations {
        metadata
            .annotations
            .get_or_insert_with(Default::default)
            .extend(annotations.clone());
    }
    ServiceAccount {
        metadata,
        ..Default::default()
    }
}

/// The relay pods: a StatefulSet for a stable, unique `node_id` per pod (relay-N), stable
/// per-pod DNS for the gossip seeds, and the `state` PVC for the gossip generation and the
/// registry's replica.
fn relay_statefulset(
    base_metadata: &ObjectMeta,
    relay: &RelaySpec,
    cm_name: &str,
    grace_period_secs: i64,
    in_relay_registry: bool,
) -> StatefulSet {
    let labels = component_labels(base_metadata, RELAY);
    let tunnel = relay.tunnel.as_ref();

    let custom_env = relay.env.as_deref().unwrap_or_default();
    let mut env = vec![
        field_ref_env("POD_NAME", "metadata.name"),
        field_ref_env("POD_IP", "status.podIP"),
    ];
    if !custom_env.iter().any(|e| e.name == "RUST_LOG") {
        env.push(EnvVar {
            name: "RUST_LOG".into(),
            value: Some("info,h2=warn,chitchat::server=error".into()),
            value_from: None,
        });
    }
    env.extend(custom_env.iter().cloned());

    let mut ports = vec![
        container_port("sender", SENDER_PORT, None),
        container_port("metrics", METRICS_PORT, None),
        container_port("mux", MUX_PORT, None),
        container_port("gossip", GOSSIP_PORT, Some("UDP")),
        container_port("cluster-rpc", CLUSTER_RPC_PORT, None),
        container_port("cluster-fwd", CLUSTER_FORWARD_PORT, None),
    ];
    if in_relay_registry {
        ports.push(container_port("registry", REGISTRY_API_PORT, None));
        ports.push(container_port("registry-read", REGISTRY_READ_PORT, None));
    }

    let storage = relay.storage.as_ref();
    let mut volume_mounts = vec![
        VolumeMount {
            name: "config".into(),
            mount_path: CONFIG_MOUNT.into(),
            read_only: Some(true),
            ..Default::default()
        },
        VolumeMount {
            name: "state".into(),
            mount_path: STATE_MOUNT.into(),
            ..Default::default()
        },
    ];
    let mut volumes = vec![Volume {
        name: "config".into(),
        config_map: Some(ConfigMapVolumeSource {
            name: cm_name.into(),
            ..Default::default()
        }),
        ..Default::default()
    }];

    if let Some(tunnel) = tunnel {
        ports.push(container_port("tunnel", TUNNEL_PORT, None));
        volume_mounts.push(VolumeMount {
            name: "auth".into(),
            mount_path: AUTH_MOUNT.into(),
            read_only: Some(true),
            ..Default::default()
        });
        volumes.push(Volume {
            name: "auth".into(),
            secret: Some(SecretVolumeSource {
                secret_name: Some(tunnel.auth_secret_name.clone()),
                ..Default::default()
            }),
            ..Default::default()
        });
    }

    StatefulSet {
        metadata: object_meta(base_metadata, RELAY),
        spec: Some(StatefulSetSpec {
            service_name: Some(RELAY_HEADLESS.into()),
            replicas: Some(relay.replicas.unwrap_or(DEFAULT_REPLICAS)),
            // All pods boot together (chitchat handles a concurrent join); updates still roll
            // one pod at a time, each waiting for its replacement to be Ready.
            pod_management_policy: Some("Parallel".into()),
            selector: component_label_selector(base_metadata, RELAY),
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(labels.clone()),
                    ..Default::default()
                }),
                spec: Some(PodSpec {
                    service_account_name: Some(RELAY.into()),
                    termination_grace_period_seconds: Some(grace_period_secs),
                    security_context: Some(PodSecurityContext {
                        // The image runs as uid 10001; make the state PVC writable for it.
                        fs_group: Some(10001),
                        ..Default::default()
                    }),
                    topology_spread_constraints: Some(vec![TopologySpreadConstraint {
                        max_skew: 1,
                        topology_key: "kubernetes.io/hostname".into(),
                        when_unsatisfiable: "ScheduleAnyway".into(),
                        label_selector: Some(component_label_selector(base_metadata, RELAY)),
                        ..Default::default()
                    }]),
                    containers: vec![Container {
                        name: RELAY.into(),
                        image: Some(relay.image.clone()),
                        image_pull_policy: relay.image_pull_policy.clone(),
                        command: Some(vec!["/bin/sh".into(), "-c".into()]),
                        args: Some(vec![format!(
                            "set -e\nsed -e \"s/__NODE_ID__/$POD_NAME/\" -e \"s/__POD_IP__/$POD_IP/\" \\\n  {CONFIG_MOUNT}/relay.toml > /tmp/relay.toml\nexec relay /tmp/relay.toml\n"
                        )]),
                        env: Some(env),
                        ports: Some(ports),
                        // /readyz flips to 503 the instant a drain starts, so the pod leaves
                        // the Services (and the LBs) while it keeps serving the hold.
                        readiness_probe: Some(Probe {
                            http_get: Some(HTTPGetAction {
                                path: Some("/readyz".into()),
                                port: IntOrString::Int(METRICS_PORT.into()),
                                ..Default::default()
                            }),
                            period_seconds: Some(3),
                            ..Default::default()
                        }),
                        // Pure "process up": /healthz stays 200 while draining.
                        liveness_probe: Some(Probe {
                            http_get: Some(HTTPGetAction {
                                path: Some("/healthz".into()),
                                port: IntOrString::Int(METRICS_PORT.into()),
                                ..Default::default()
                            }),
                            period_seconds: Some(10),
                            ..Default::default()
                        }),
                        resources: relay.resources.clone(),
                        volume_mounts: Some(volume_mounts),
                        ..Default::default()
                    }],
                    volumes: Some(volumes),
                    ..Default::default()
                }),
            },
            // One PVC per pod: [cluster].generation_state_path (a single integer that must
            // survive restarts under the same pod name) and the registry's replica (a cache of
            // the bucket; a lost volume only costs a reload).
            volume_claim_templates: Some(vec![PersistentVolumeClaim {
                metadata: ObjectMeta {
                    name: Some("state".into()),
                    labels: Some(labels),
                    ..Default::default()
                },
                spec: Some(PersistentVolumeClaimSpec {
                    storage_class_name: storage.and_then(|s| s.storage_class_name.clone()),
                    access_modes: Some(vec!["ReadWriteOnce".into()]),
                    resources: Some(VolumeResourceRequirements {
                        requests: Some(BTreeMap::from([(
                            "storage".to_owned(),
                            Quantity(
                                storage
                                    .and_then(|s| s.storage_request_bytes)
                                    .map_or_else(|| DEFAULT_STORAGE.into(), |b| b.to_string()),
                            ),
                        )])),
                        limits: None,
                    }),
                    ..Default::default()
                }),
                status: None,
            }]),
            ..Default::default()
        }),
        status: None,
    }
}

/// In-cluster: Restate's sender traffic, the metrics and (in `included` mode) the registry
/// API, spread across the pods.
fn relay_service(base_metadata: &ObjectMeta, in_relay_registry: bool) -> Service {
    let mut ports = vec![
        service_port("sender", SENDER_PORT, None),
        service_port("metrics", METRICS_PORT, None),
    ];
    if in_relay_registry {
        ports.push(service_port("registry", REGISTRY_API_PORT, None));
    }
    Service {
        metadata: object_meta(base_metadata, RELAY),
        spec: Some(ServiceSpec {
            selector: Some(component_selector(base_metadata, RELAY)),
            ports: Some(ports),
            ..Default::default()
        }),
        status: None,
    }
}

/// Stable per-pod DNS for the gossip seeds; peers resolve each other before they're ready.
fn relay_headless_service(base_metadata: &ObjectMeta) -> Service {
    Service {
        metadata: object_meta(base_metadata, RELAY_HEADLESS),
        spec: Some(ServiceSpec {
            cluster_ip: Some("None".into()),
            publish_not_ready_addresses: Some(true),
            selector: Some(component_selector(base_metadata, RELAY)),
            ports: Some(vec![
                service_port("gossip", GOSSIP_PORT, Some("UDP")),
                service_port("cluster-rpc", CLUSTER_RPC_PORT, None),
                service_port("cluster-fwd", CLUSTER_FORWARD_PORT, None),
            ]),
            ..Default::default()
        }),
        status: None,
    }
}

/// At most one relay down for voluntary disruptions too, so the Services never go empty.
fn relay_pod_disruption_budget(base_metadata: &ObjectMeta) -> PodDisruptionBudget {
    PodDisruptionBudget {
        metadata: object_meta(base_metadata, RELAY),
        spec: Some(PodDisruptionBudgetSpec {
            max_unavailable: Some(IntOrString::Int(1)),
            selector: Some(component_label_selector(base_metadata, RELAY)),
            ..Default::default()
        }),
        status: None,
    }
}

/// What the relay's reconcile found.
pub(super) struct RelayOutcome {
    pub status: RelayStatus,
    pub state: ComponentState,
    /// Every pod runs the current spec and is ready.
    pub rolled_out: bool,
    pub ingress_mux: Advertised,
    /// The tunnel receiver's URL, as advertised to tunnel clients.
    pub tunnel_url: Option<String>,
}

pub(super) async fn reconcile_relay(
    ctx: &Context,
    name: &str,
    base_metadata: &ObjectMeta,
    relay: &RelaySpec,
    registry: &RelayRegistry,
    dns: Dns<'_>,
) -> Result<RelayOutcome, Error> {
    let in_relay_registry = matches!(registry, RelayRegistry::InRelay { .. });
    let apis = Apis::new(ctx, name);
    let replicas = relay.replicas.unwrap_or(DEFAULT_REPLICAS);
    let selector = component_selector(base_metadata, RELAY);

    // The exposed Services first: the relay advertises the tunnel's address, Restate the mux's.
    let ingress_mux = match relay.ingress_mux.as_ref() {
        Some(mux) => {
            let exposed = mux.service.as_ref();
            let svc = apply(
                &apis.services,
                &exposed_service(
                    base_metadata,
                    INGRESS_MUX,
                    exposed,
                    selector.clone(),
                    "mux",
                    MUX_PORT.into(),
                ),
            )
            .await?;
            Advertised::of(mux.advertised_url.as_deref(), exposed, &svc, MUX_PORT)
        }
        None => {
            delete_if_exists(&apis.services, INGRESS_MUX).await?;
            Advertised::Unexposed
        }
    };

    // Some(None): a tunnel whose LoadBalancer has no address yet.
    let tunnel_url: Option<Option<String>> = match relay.tunnel.as_ref() {
        Some(tunnel) => {
            let exposed = tunnel.service.as_ref();
            let svc = apply(
                &apis.services,
                &exposed_service(
                    base_metadata,
                    RELAY_TUNNEL,
                    exposed,
                    selector.clone(),
                    "tunnel",
                    TUNNEL_PORT.into(),
                ),
            )
            .await?;
            Some(match (&tunnel.external_url, is_load_balancer(exposed)) {
                (Some(url), _) => Some(url.clone()),
                (None, true) => {
                    load_balancer_address(&svc).map(|a| format!("http://{a}:{TUNNEL_PORT}"))
                }
                // Not public: tunnel clients in the cluster dial the Service.
                (None, false) => Some(dns.url(RELAY_TUNNEL, TUNNEL_PORT)),
            })
        }
        None => {
            delete_if_exists(&apis.services, RELAY_TUNNEL).await?;
            None
        }
    };

    let service_accounts: Api<ServiceAccount> = Api::namespaced(ctx.client.clone(), name);
    apply(
        &service_accounts,
        &relay_service_account(base_metadata, relay),
    )
    .await?;
    apply(
        &apis.services,
        &relay_service(base_metadata, in_relay_registry),
    )
    .await?;
    apply(&apis.services, &relay_headless_service(base_metadata)).await?;
    apply(&apis.pdbs, &relay_pod_disruption_budget(base_metadata)).await?;

    let (config_hash, ss) = if let Some(None) = tunnel_url {
        // Can't render external_tunnel_url yet: leave the relay as it is until the address
        // comes (the Service's status change triggers a reconcile).
        let existing = ctx.ss_store.get(&ObjectRef::new(RELAY).within(name));
        (None, existing.map(|ss| (*ss).clone()))
    } else {
        let config = render_relay_config(&RelayConfigParams {
            dns,
            replicas,
            registry,
            tunnel_url: tunnel_url.as_ref().and_then(|t| t.as_deref()),
            extra_config: relay.extra_config.as_deref(),
        })?;
        let relay_toml = toml::to_string(&config).map_err(|e| {
            Error::InvalidRestateConfig(format!("Failed to render the relay config: {e}"))
        })?;
        let (cm, hash) = relay_configmap(base_metadata, relay_toml, relay.tunnel.is_some());
        let cm_name = cm.metadata.name.clone().unwrap();
        apply(&apis.configmaps, &cm).await?;

        let ss = apply(
            &apis.sts,
            &relay_statefulset(
                base_metadata,
                relay,
                &cm_name,
                grace_period_secs(&config),
                in_relay_registry,
            ),
        )
        .await?;

        if statefulset_rollout(&ss).1 {
            delete_old_configs(&apis, name, &cm_name).await?;
        }
        (Some(hash), Some(ss))
    };

    let (ready_replicas, rolled_out) = ss.as_ref().map(statefulset_rollout).unwrap_or((0, false));

    let state = if let Some(None) = tunnel_url {
        ComponentState::waiting(
            "TunnelAddressPending",
            "waiting for the relay-tunnel LoadBalancer address (external_tunnel_url)",
        )
    } else if ingress_mux == Advertised::Pending {
        ComponentState::waiting(
            "IngressMuxAddressPending",
            "waiting for the ingress-mux LoadBalancer address",
        )
    } else if !rolled_out {
        ComponentState::waiting(
            "RelayRollingOut",
            format!("{ready_replicas}/{replicas} relay pods ready on the current config"),
        )
    } else {
        ComponentState::ready(
            "RelayReady",
            format!("{ready_replicas}/{replicas} relay pods ready"),
        )
    };

    Ok(RelayOutcome {
        status: RelayStatus {
            replicas,
            ready_replicas,
            config_hash,
        },
        state,
        rolled_out,
        ingress_mux,
        tunnel_url: tunnel_url.flatten(),
    })
}

/// The configs of earlier renders, once every pod runs the current one (a pod of the old
/// revision recreated mid-rollout still needs its ConfigMap).
async fn delete_old_configs(apis: &Apis, name: &str, current: &str) -> Result<(), Error> {
    let old = apis
        .configmaps
        .list_metadata(&ListParams::default().labels(&format!(
            "{RELAY_CONFIG_LABEL}=true,app.kubernetes.io/instance={name}"
        )))
        .await?;
    for cm in old.items {
        let cm_name = cm.name_any();
        if cm_name != current {
            delete_if_exists(&apis.configmaps, &cm_name).await?;
        }
    }
    Ok(())
}

pub(super) async fn cleanup_relay(ctx: &Context, name: &str) -> Result<(), Error> {
    let apis = Apis::new(ctx, name);
    for svc in [INGRESS_MUX, RELAY_TUNNEL, RELAY, RELAY_HEADLESS] {
        delete_if_exists(&apis.services, svc).await?;
    }
    delete_if_exists(&apis.pdbs, RELAY).await?;
    delete_old_configs(&apis, name, "").await?;
    let service_accounts: Api<ServiceAccount> = Api::namespaced(ctx.client.clone(), name);
    delete_if_exists(&service_accounts, RELAY).await?;
    // last: its presence is what tells cleanup_multitenancy there's something to clean up
    delete_if_exists(&apis.sts, RELAY).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controllers::restatecluster::reconcilers::multitenancy::tests::base_metadata;
    use crate::resources::restateclusters::{RelayStorage, TunnelSpec};

    const DNS: Dns<'static> = Dns {
        namespace: "restate-demo",
        cluster_dns: "cluster.local",
    };

    /// An `external`-mode relay: one namespace for the relay and Restate, `restate-demo`; 3
    /// relays; the tunnel LB at 34.1.2.3.
    const EXTERNAL_RELAY_TOML: &str = include_str!("testdata/relay-external.toml");
    /// An `included`-mode relay in namespace `restate`, no tunnel.
    const INCLUDED_RELAY_TOML: &str = include_str!("testdata/relay-included.toml");

    /// What the perf setup tunes in the registry client's caches.
    const EXTRA_CONFIG: &str = r#"
        [preflight.cache]
        latest_ttl_secs = 60
        max_entries = 2000000
    "#;

    fn render(registry_url: &str, tunnel_url: Option<&str>, extra: Option<&str>) -> toml::Table {
        render_relay_config(&RelayConfigParams {
            dns: DNS,
            replicas: 3,
            registry: &RelayRegistry::Url(registry_url.into()),
            tunnel_url,
            extra_config: extra,
        })
        .unwrap()
    }

    fn assert_renders(rendered: toml::Table, expected: &str) {
        let expected: toml::Table = toml::from_str(expected).unwrap();
        assert_eq!(
            toml::Value::Table(rendered.clone()),
            toml::Value::Table(expected),
            "rendered:\n{}",
            toml::to_string(&rendered).unwrap()
        );

        // and it survives the trip through the ConfigMap
        let reparsed: toml::Table = toml::from_str(&toml::to_string(&rendered).unwrap()).unwrap();
        assert_eq!(reparsed, rendered);
    }

    #[test]
    fn renders_an_external_registry_relay() {
        assert_renders(
            render(
                "http://schema-registry.restate-demo.svc.cluster.local:8090",
                Some("http://34.1.2.3:19080"),
                Some(EXTRA_CONFIG),
            ),
            EXTERNAL_RELAY_TOML,
        );
    }

    fn render_included(shards: Option<i32>, read_api: bool) -> toml::Table {
        render_relay_config(&RelayConfigParams {
            dns: Dns {
                namespace: "restate",
                cluster_dns: "cluster.local",
            },
            replicas: 3,
            registry: &RelayRegistry::InRelay {
                bucket_url: "gs://restate-registry/registry".into(),
                shards,
                read_api,
            },
            tunnel_url: None,
            extra_config: None,
        })
        .unwrap()
    }

    #[test]
    fn renders_an_included_registry_relay() {
        assert_renders(render_included(None, true), INCLUDED_RELAY_TOML);

        let rendered = render_included(Some(100), false);
        assert_eq!(rendered["registry"]["shards"].as_integer(), Some(100));
        assert!(
            !rendered["registry"]
                .as_table()
                .unwrap()
                .contains_key("public_listen")
        );
    }

    #[test]
    fn without_tunnel_the_receiver_is_loopback_and_authorises_nobody() {
        let rendered = render("http://registry:8090", None, None);
        assert_eq!(
            rendered.get("receiver_listen").and_then(|v| v.as_str()),
            Some("127.0.0.1:19080")
        );
        assert!(!rendered.contains_key("receiver_protocol"));
        assert!(!rendered.contains_key("external_tunnel_url"));
        assert_eq!(
            rendered["auth"]["static"]["keys_path"].as_str(),
            Some("/etc/relay/no-tunnel-auth-keys.toml")
        );
        assert_eq!(
            rendered["preflight"]["registry_url"].as_str(),
            Some("http://registry:8090")
        );
    }

    #[test]
    fn extra_config_is_deep_merged_and_wins() {
        let rendered = render(
            "http://registry:8090",
            None,
            Some(
                r#"
                [preflight]
                registry_timeout_ms = 250
                [limits.drain]
                termination_grace_period_secs = 120
                [otel]
                endpoint = "http://otel:4317"
                "#,
            ),
        );
        // the user's key wins, its siblings stay
        assert_eq!(
            rendered["preflight"]["registry_timeout_ms"].as_integer(),
            Some(250)
        );
        assert_eq!(
            rendered["preflight"]["registry_url"].as_str(),
            Some("http://registry:8090")
        );
        assert_eq!(
            rendered["limits"]["drain"]["drain_timeout_secs"].as_integer(),
            Some(60)
        );
        assert!(rendered.contains_key("otel"));
        // the pods' grace period follows the drain's
        assert_eq!(grace_period_secs(&rendered), 120);
        assert_eq!(
            grace_period_secs(&render("http://registry:8090", None, None)),
            70
        );
    }

    #[test]
    fn invalid_extra_config_is_an_error() {
        let result = render_relay_config(&RelayConfigParams {
            dns: DNS,
            replicas: 3,
            registry: &RelayRegistry::Url("http://registry:8090".into()),
            tunnel_url: None,
            extra_config: Some("not [valid"),
        });
        assert!(matches!(result, Err(Error::InvalidRestateConfig(_))));
    }

    #[test]
    fn configmap_name_follows_the_content() {
        let (a, hash_a) = relay_configmap(&base_metadata(), "a = 1\n".into(), true);
        let (b, hash_b) = relay_configmap(&base_metadata(), "a = 2\n".into(), true);
        let (c, _) = relay_configmap(&base_metadata(), "a = 1\n".into(), false);
        assert_ne!(hash_a, hash_b);
        assert_eq!(
            a.metadata.name.as_deref(),
            Some(format!("relay-config-{hash_a}").as_str())
        );
        assert_ne!(a.metadata.name, b.metadata.name);
        assert_ne!(a.metadata.name, c.metadata.name);
        assert!(c.data.unwrap().contains_key(NO_TUNNEL_KEYS));
        assert_eq!(
            a.metadata.labels.unwrap().get(RELAY_CONFIG_LABEL).unwrap(),
            "true"
        );
    }

    fn relay_spec(tunnel: bool) -> RelaySpec {
        RelaySpec {
            image: "ghcr.io/restatedev/relay:test".into(),
            replicas: None,
            image_pull_policy: Some("Always".into()),
            resources: None,
            env: Some(vec![EnvVar {
                name: "RUST_LOG".into(),
                value: Some("debug".into()),
                value_from: None,
            }]),
            storage: None,
            service_account_annotations: None,
            ingress_mux: None,
            tunnel: tunnel.then(|| TunnelSpec {
                auth_secret_name: "relay-auth".into(),
                service: None,
                external_url: None,
            }),
            extra_config: None,
        }
    }

    #[test]
    fn statefulset_matches_cluster_setups_relay() {
        let ss = relay_statefulset(
            &base_metadata(),
            &relay_spec(true),
            "relay-config-abc",
            70,
            false,
        );
        let spec = ss.spec.unwrap();
        assert_eq!(spec.replicas, Some(3));
        assert_eq!(spec.pod_management_policy.as_deref(), Some("Parallel"));
        assert_eq!(spec.service_name.as_deref(), Some("relay-headless"));

        let pod = spec.template.spec.unwrap();
        assert_eq!(pod.termination_grace_period_seconds, Some(70));
        assert_eq!(pod.security_context.unwrap().fs_group, Some(10001));

        let container = &pod.containers[0];
        assert!(container.args.as_ref().unwrap()[0].contains("__NODE_ID__/$POD_NAME"));
        let env = container.env.as_ref().unwrap();
        // the user's RUST_LOG replaces the default
        let rust_log: Vec<_> = env.iter().filter(|e| e.name == "RUST_LOG").collect();
        assert_eq!(rust_log.len(), 1);
        assert_eq!(rust_log[0].value.as_deref(), Some("debug"));
        assert!(env.iter().any(|e| e.name == "POD_IP"));
        assert!(
            container
                .ports
                .as_ref()
                .unwrap()
                .iter()
                .any(|p| p.container_port == 19080)
        );

        let volumes = pod.volumes.unwrap();
        assert_eq!(
            volumes[0].config_map.as_ref().unwrap().name,
            "relay-config-abc"
        );
        assert_eq!(
            volumes[1].secret.as_ref().unwrap().secret_name.as_deref(),
            Some("relay-auth")
        );

        // the pods aren't Restate's: they must not be selected as Restate nodes, and the
        // state PVCs must not be resized as Restate's
        let pod_labels = spec.template.metadata.unwrap().labels.unwrap();
        assert_eq!(pod_labels.get("app.kubernetes.io/name").unwrap(), "relay");
        let pvc_labels = spec.volume_claim_templates.unwrap()[0]
            .metadata
            .labels
            .clone()
            .unwrap();
        assert_eq!(pvc_labels.get("app.kubernetes.io/name").unwrap(), "relay");
        // the object itself is the cluster's, so the controller's watch sees it
        assert_eq!(
            ss.metadata
                .labels
                .unwrap()
                .get("app.kubernetes.io/name")
                .unwrap(),
            "restate"
        );
    }

    #[test]
    fn statefulset_without_tunnel_mounts_no_secret() {
        let ss = relay_statefulset(
            &base_metadata(),
            &relay_spec(false),
            "relay-config-abc",
            70,
            false,
        );
        let pod = ss.spec.unwrap().template.spec.unwrap();
        assert_eq!(pod.volumes.unwrap().len(), 1);
        assert!(
            !pod.containers[0]
                .ports
                .as_ref()
                .unwrap()
                .iter()
                .any(|p| p.container_port == 19080)
        );
    }

    #[test]
    fn services_match_cluster_setups_relay() {
        let svc = relay_service(&base_metadata(), false).spec.unwrap();
        let ports: Vec<_> = svc.ports.unwrap().iter().map(|p| p.port).collect();
        assert_eq!(ports, vec![9080, 9000]);
        let svc = relay_service(&base_metadata(), true).spec.unwrap();
        let ports: Vec<_> = svc.ports.unwrap().iter().map(|p| p.port).collect();
        assert_eq!(ports, vec![9080, 9000, 8090]);

        let headless = relay_headless_service(&base_metadata()).spec.unwrap();
        assert_eq!(headless.cluster_ip.as_deref(), Some("None"));
        assert_eq!(headless.publish_not_ready_addresses, Some(true));
        let ports = headless.ports.unwrap();
        assert_eq!(ports[0].protocol.as_deref(), Some("UDP"));
        assert_eq!(
            ports.iter().map(|p| p.port).collect::<Vec<_>>(),
            vec![6000, 6001, 6002]
        );

        let pdb = relay_pod_disruption_budget(&base_metadata()).spec.unwrap();
        assert_eq!(pdb.max_unavailable, Some(IntOrString::Int(1)));
    }

    #[test]
    fn statefulset_storage_identity_and_registry_ports() {
        let ss = relay_statefulset(
            &base_metadata(),
            &relay_spec(false),
            "relay-config-abc",
            70,
            false,
        );
        let spec = ss.spec.unwrap();
        let pvc = spec.volume_claim_templates.unwrap()[0]
            .spec
            .clone()
            .unwrap();
        assert_eq!(pvc.storage_class_name, None);
        assert_eq!(
            pvc.resources.unwrap().requests.unwrap()["storage"],
            Quantity("10Gi".into())
        );
        let pod = spec.template.spec.unwrap();
        assert_eq!(pod.service_account_name.as_deref(), Some("relay"));
        assert!(
            !pod.containers[0]
                .ports
                .as_ref()
                .unwrap()
                .iter()
                .any(|p| p.container_port == 8090)
        );

        let relay = RelaySpec {
            storage: Some(RelayStorage {
                storage_class_name: Some("standard-rwo".into()),
                storage_request_bytes: Some(5368709120),
            }),
            service_account_annotations: Some(BTreeMap::from([(
                "iam.gke.io/gcp-service-account".into(),
                "relay@p.iam.gserviceaccount.com".into(),
            )])),
            ..relay_spec(false)
        };
        let spec = relay_statefulset(&base_metadata(), &relay, "relay-config-abc", 70, true)
            .spec
            .unwrap();
        let pvc = spec.volume_claim_templates.unwrap()[0]
            .spec
            .clone()
            .unwrap();
        assert_eq!(pvc.storage_class_name.as_deref(), Some("standard-rwo"));
        assert_eq!(
            pvc.resources.unwrap().requests.unwrap()["storage"],
            Quantity("5368709120".into())
        );
        let ports: Vec<_> = spec.template.spec.unwrap().containers[0]
            .ports
            .clone()
            .unwrap()
            .iter()
            .map(|p| p.container_port)
            .collect();
        assert!(ports.contains(&8090) && ports.contains(&8091));

        let sa = relay_service_account(&base_metadata(), &relay);
        assert_eq!(sa.metadata.name.as_deref(), Some("relay"));
        assert_eq!(
            sa.metadata.annotations.unwrap()["iam.gke.io/gcp-service-account"],
            "relay@p.iam.gserviceaccount.com"
        );
    }
}
