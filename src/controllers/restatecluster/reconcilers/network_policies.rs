use std::collections::BTreeMap;
use std::convert::Into;
use std::string::ToString;

use k8s_openapi::api::networking::v1::NetworkPolicySpec;
use k8s_openapi::api::networking::v1::{
    IPBlock, NetworkPolicy, NetworkPolicyEgressRule, NetworkPolicyIngressRule, NetworkPolicyPeer,
    NetworkPolicyPort,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::api::DeleteParams;
use kube::{
    api::{Patch, PatchParams},
    Api,
};
use tracing::debug;

use crate::controllers::restatecluster::controller::Context;
use crate::resources::restateclusters::RestateClusterNetworkPeers;
use crate::Error;

use super::{label_selector, object_meta};

fn deny_all(base_metadata: &ObjectMeta) -> NetworkPolicy {
    NetworkPolicy {
        metadata: object_meta(base_metadata, "deny-all"),
        spec: Some(NetworkPolicySpec {
            policy_types: Some(vec!["Egress".into(), "Ingress".into()]),
            ..Default::default()
        }),
    }
}

fn allow_dns(base_metadata: &ObjectMeta) -> NetworkPolicy {
    NetworkPolicy {
        metadata: object_meta(base_metadata, "allow-egress-to-kube-dns"),
        spec: Some(NetworkPolicySpec {
            policy_types: Some(vec!["Egress".into()]),
            egress: Some(vec![NetworkPolicyEgressRule {
                to: Some(vec![NetworkPolicyPeer {
                    pod_selector: Some(LabelSelector {
                        match_labels: Some(
                            [("k8s-app".to_string(), "kube-dns".to_string())].into(),
                        ),
                        ..Default::default()
                    }),
                    namespace_selector: Some(LabelSelector {
                        match_labels: Some(
                            [(
                                "kubernetes.io/metadata.name".to_string(),
                                "kube-system".to_string(),
                            )]
                            .into(),
                        ),
                        ..Default::default()
                    }),
                    ..Default::default()
                }]),
                ports: Some(vec![
                    NetworkPolicyPort {
                        protocol: Some("UDP".into()),
                        port: Some(IntOrString::Int(53)),
                        end_port: None,
                    },
                    NetworkPolicyPort {
                        protocol: Some("TCP".into()),
                        port: Some(IntOrString::Int(53)),
                        end_port: None,
                    },
                ]),
            }]),
            ..Default::default()
        }),
    }
}

fn allow_public(base_metadata: &ObjectMeta) -> NetworkPolicy {
    NetworkPolicy {
        metadata: object_meta(base_metadata, "allow-restate-egress-to-public-internet"),
        spec: Some(NetworkPolicySpec {
            pod_selector: label_selector(base_metadata),
            policy_types: Some(vec!["Egress".into()]),
            egress: Some(vec![NetworkPolicyEgressRule {
                to: Some(vec![
                    // we split the ipv4 space into two because there is a known issue with AWS VPC CNI
                    // that makes using 0.0.0.0/0 as `cidr` very dangerous.
                    // https://github.com/aws/aws-network-policy-agent/pull/58
                    NetworkPolicyPeer {
                        ip_block: Some(IPBlock {
                            // 0.0.0.0 to 127.255.255.255
                            cidr: "0.0.0.0/1".into(),
                            except: Some(vec![
                                // private IP ranges: https://en.wikipedia.org/wiki/Private_network
                                "10.0.0.0/8".into(),
                            ]),
                        }),
                        ..Default::default()
                    },
                    NetworkPolicyPeer {
                        ip_block: Some(IPBlock {
                            // 	128.0.0.0 to 255.255.255.255
                            cidr: "128.0.0.0/1".into(),
                            except: Some(vec![
                                // private IP ranges: https://en.wikipedia.org/wiki/Private_network
                                "192.168.0.0/16".into(),
                                "172.16.0.0/12".into(),
                                // and the link-local IP ranges, as this is used by AWS instance metadata
                                "169.254.0.0/16".into(),
                            ]),
                        }),
                        ..Default::default()
                    },
                ]),
                ports: None, // all ports
            }]),
            ..Default::default()
        }),
    }
}

const AWS_POD_IDENTITY_POLICY_NAME: &str = "allow-restate-egress-to-aws-pod-identity";

fn allow_aws_pod_identity(base_metadata: &ObjectMeta) -> NetworkPolicy {
    // https://docs.aws.amazon.com/eks/latest/userguide/pod-id-how-it-works.html
    NetworkPolicy {
        metadata: object_meta(base_metadata, AWS_POD_IDENTITY_POLICY_NAME),
        spec: Some(NetworkPolicySpec {
            pod_selector: label_selector(base_metadata),
            policy_types: Some(vec!["Egress".into()]),
            egress: Some(vec![NetworkPolicyEgressRule {
                to: Some(vec![NetworkPolicyPeer {
                    ip_block: Some(IPBlock {
                        cidr: "169.254.170.23/32".into(),
                        except: None,
                    }),
                    ..Default::default()
                }]),
                ports: Some(vec![NetworkPolicyPort {
                    port: Some(IntOrString::Int(80)),
                    protocol: Some("TCP".into()),
                    end_port: None,
                }]), // all ports
            }]),
            ..Default::default()
        }),
    }
}

fn allow_access(
    port_name: &str,
    port: i32,
    base_metadata: &ObjectMeta,
    peers: Option<Vec<NetworkPolicyPeer>>,
) -> NetworkPolicy {
    NetworkPolicy {
        metadata: object_meta(base_metadata, format!("allow-{port_name}-access")),
        spec: Some(NetworkPolicySpec {
            pod_selector: label_selector(base_metadata),
            policy_types: Some(vec!["Ingress".into()]),
            ingress: peers.map(|peers| {
                vec![NetworkPolicyIngressRule {
                    from: Some(peers),
                    ports: Some(vec![NetworkPolicyPort {
                        protocol: Some("TCP".into()),
                        port: Some(IntOrString::Int(port)),
                        end_port: None,
                    }]),
                }]
            }),
            ..Default::default()
        }),
    }
}

fn allow_egress(
    namespace: &str,
    base_metadata: &ObjectMeta,
    user_egress: Option<&[crate::resources::restateclusters::NetworkPolicyEgressRule]>,
) -> NetworkPolicy {
    let mut egress: Vec<NetworkPolicyEgressRule> =
        Vec::with_capacity(user_egress.map(|e| e.len()).unwrap_or(0) + 2);
    if let Some(user_egress) = user_egress {
        egress.extend(
            user_egress
                .iter()
                .cloned()
                .map(NetworkPolicyEgressRule::from),
        );
    }

    // allow egress to ourself on the node port
    egress.push(NetworkPolicyEgressRule {
        ports: Some(vec![NetworkPolicyPort {
            end_port: None,
            protocol: Some("TCP".into()),
            port: Some(IntOrString::Int(5122)),
        }]),
        to: Some(vec![NetworkPolicyPeer {
            ip_block: None,
            // allow all namespaces
            namespace_selector: Some(LabelSelector {
                match_expressions: None,
                match_labels: Some(BTreeMap::from([(
                    "kubernetes.io/metadata.name".into(),
                    namespace.into(),
                )])),
            }),
            // select the labels of the cluster
            pod_selector: Some(label_selector(base_metadata)),
        }]),
    });

    // allow egress to any pod labelled specifically to allow this cluster, in any namespace, on any port
    egress.push(NetworkPolicyEgressRule {
        // allow all ports
        ports: None,
        to: Some(vec![NetworkPolicyPeer {
            ip_block: None,
            // allow all namespaces
            namespace_selector: Some(LabelSelector::default()),
            pod_selector: Some(LabelSelector {
                match_expressions: None,
                match_labels: Some(BTreeMap::from([(
                    format!("allow.restate.dev/{namespace}",),
                    "true".into(),
                )])),
            }),
        }]),
    });

    NetworkPolicy {
        metadata: object_meta(base_metadata, "allow-restate-egress"),
        spec: Some(NetworkPolicySpec {
            pod_selector: label_selector(base_metadata),
            policy_types: Some(vec!["Egress".into()]),
            egress: Some(egress),
            ..Default::default()
        }),
    }
}

pub async fn reconcile_network_policies(
    ctx: &Context,
    namespace: &str,
    base_metadata: &ObjectMeta,
    network_peers: Option<&RestateClusterNetworkPeers>,
    allow_operator_access_to_admin: bool,
    network_egress_rules: Option<&[crate::resources::restateclusters::NetworkPolicyEgressRule]>,
    aws_pod_identity_enabled: bool,
) -> Result<(), Error> {
    let np_api: Api<NetworkPolicy> = Api::namespaced(ctx.client.clone(), namespace);

    apply_network_policy(namespace, &np_api, deny_all(base_metadata)).await?;
    apply_network_policy(namespace, &np_api, allow_dns(base_metadata)).await?;
    apply_network_policy(namespace, &np_api, allow_public(base_metadata)).await?;

    if aws_pod_identity_enabled {
        apply_network_policy(namespace, &np_api, allow_aws_pod_identity(base_metadata)).await?
    } else {
        delete_network_policy(namespace, &np_api, AWS_POD_IDENTITY_POLICY_NAME).await?
    }

    apply_network_policy(
        namespace,
        &np_api,
        allow_egress(namespace, base_metadata, network_egress_rules),
    )
    .await?;

    apply_network_policy(
        namespace,
        &np_api,
        allow_access(
            "ingress",
            8080,
            base_metadata,
            network_peers.and_then(|peers| peers.ingress.clone()),
        ),
    )
    .await?;

    let admin_peers: Option<Vec<NetworkPolicyPeer>> = match (
        allow_operator_access_to_admin,
        ctx.operator_namespace.as_ref(),
        ctx.operator_label_name.as_ref(),
        ctx.operator_label_value.as_ref(),
    ) {
        (true, Some(operator_namespace), Some(operator_label_name), Some(operator_label_value)) => {
            let mut peers = network_peers
                .and_then(|peers| peers.admin.clone())
                .unwrap_or_default();

            peers.push(NetworkPolicyPeer {
                ip_block: None,
                namespace_selector: Some(LabelSelector {
                    match_expressions: None,
                    match_labels: Some(BTreeMap::from([(
                        "kubernetes.io/metadata.name".into(),
                        operator_namespace.clone(),
                    )])),
                }),
                pod_selector: Some(LabelSelector {
                    match_expressions: None,
                    match_labels: Some(BTreeMap::from([(
                        operator_label_name.clone(),
                        operator_label_value.clone(),
                    )])),
                }),
            });

            Some(peers.into())
        }
        _ => network_peers.and_then(|peers| peers.admin.as_deref().map(|a| a.into())),
    };

    apply_network_policy(
        namespace,
        &np_api,
        allow_access("admin", 9070, base_metadata, admin_peers),
    )
    .await?;

    let mut node_peers = match network_peers.and_then(|peers| peers.metrics.as_deref()) {
        Some(node_peers) => {
            let mut node_peers_vec = Vec::with_capacity(node_peers.len() + 1);
            node_peers_vec.extend_from_slice(node_peers);
            node_peers_vec
        }
        None => Vec::with_capacity(1),
    };
    node_peers.push(NetworkPolicyPeer {
        ip_block: None,
        namespace_selector: Some(LabelSelector {
            match_expressions: None,
            match_labels: Some(BTreeMap::from([(
                "kubernetes.io/metadata.name".into(),
                namespace.into(),
            )])),
        }),
        // select the labels of the cluster
        pod_selector: Some(label_selector(base_metadata)),
    });

    apply_network_policy(
        namespace,
        &np_api,
        allow_access("metrics", 5122, base_metadata, Some(node_peers)),
    )
    .await?;

    Ok(())
}

async fn apply_network_policy(
    namespace: &str,
    np_api: &Api<NetworkPolicy>,
    np: NetworkPolicy,
) -> Result<(), Error> {
    let name = np.metadata.name.as_ref().unwrap();
    let params: PatchParams = PatchParams::apply("restate-operator").force();
    debug!(
        "Applying Network Policy {} in namespace {}",
        name, namespace
    );
    np_api.patch(name, &params, &Patch::Apply(&np)).await?;
    Ok(())
}

async fn delete_network_policy(
    namespace: &str,
    np_api: &Api<NetworkPolicy>,
    name: &str,
) -> Result<(), Error> {
    debug!(
        "Ensuring NetworkPolicy {} in namespace {} does not exist",
        name, namespace
    );
    match np_api.delete(name, &DeleteParams::default()).await {
        Err(kube::Error::Api(kube::error::ErrorResponse { code: 404, .. })) => Ok(()),
        Err(err) => Err(err.into()),
        Ok(_) => Ok(()),
    }
}
