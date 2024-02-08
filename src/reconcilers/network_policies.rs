use std::convert::Into;
use std::string::ToString;

use k8s_openapi::api::networking::v1::NetworkPolicySpec;
use k8s_openapi::api::networking::v1::{
    IPBlock, NetworkPolicy, NetworkPolicyEgressRule, NetworkPolicyIngressRule, NetworkPolicyPeer,
    NetworkPolicyPort,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, OwnerReference};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::{
    api::{Patch, PatchParams},
    Api, Client,
};
use tracing::debug;

use crate::reconcilers::{label_selector, object_meta};
use crate::{Error, RestateClusterNetworkPeers};

fn deny_all(oref: &OwnerReference) -> NetworkPolicy {
    NetworkPolicy {
        metadata: object_meta(oref, "deny-all"),
        spec: Some(NetworkPolicySpec {
            policy_types: Some(vec!["Egress".into(), "Ingress".into()]),
            ..Default::default()
        }),
    }
}

fn allow_dns(oref: &OwnerReference) -> NetworkPolicy {
    NetworkPolicy {
        metadata: object_meta(oref, "allow-egress-to-kube-dns"),
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

fn allow_public(oref: &OwnerReference) -> NetworkPolicy {
    NetworkPolicy {
        metadata: object_meta(oref, "allow-restate-egress-to-public-internet"),
        spec: Some(NetworkPolicySpec {
            pod_selector: label_selector(&oref.name),
            policy_types: Some(vec!["Egress".into()]),
            egress: Some(vec![NetworkPolicyEgressRule {
                to: Some(vec![NetworkPolicyPeer {
                    ip_block: Some(IPBlock {
                        // all ipv4
                        cidr: "0.0.0.0/0".into(),
                        except: Some(vec![
                            // except the private IP ranges: https://en.wikipedia.org/wiki/Private_network
                            "10.0.0.0/8".into(),
                            "192.168.0.0/16".into(),
                            "172.16.0.0/20".into(),
                            // and the link-local IP ranges, as this is used by AWS instance metadata
                            "169.254.0.0/16".into(),
                        ]),
                    }),
                    ..Default::default()
                }]),
                ports: None, // all ports
            }]),
            ..Default::default()
        }),
    }
}

fn allow_access(
    port_name: &str,
    port: i32,
    oref: &OwnerReference,
    peers: Option<&[NetworkPolicyPeer]>,
) -> NetworkPolicy {
    NetworkPolicy {
        metadata: object_meta(oref, format!("allow-{port_name}-access")),
        spec: Some(NetworkPolicySpec {
            pod_selector: label_selector(&oref.name),
            policy_types: Some(vec!["Ingress".into()]),
            ingress: peers.map(|peers| {
                vec![NetworkPolicyIngressRule {
                    from: Some(peers.into()),
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

pub async fn reconcile_network_policies(
    client: Client,
    namespace: &str,
    oref: &OwnerReference,
    network_peers: Option<&RestateClusterNetworkPeers>,
) -> Result<(), Error> {
    let np_api: Api<NetworkPolicy> = Api::namespaced(client, namespace);

    apply_network_policy(namespace, &np_api, deny_all(oref)).await?;
    apply_network_policy(namespace, &np_api, allow_dns(oref)).await?;
    apply_network_policy(namespace, &np_api, allow_public(oref)).await?;

    apply_network_policy(
        namespace,
        &np_api,
        allow_access(
            "ingress",
            8080,
            oref,
            network_peers.and_then(|peers| peers.ingress.as_ref().map(Vec::as_slice)),
        ),
    )
    .await?;

    apply_network_policy(
        namespace,
        &np_api,
        allow_access(
            "admin",
            9070,
            oref,
            network_peers.and_then(|peers| peers.admin.as_ref().map(Vec::as_slice)),
        ),
    )
    .await?;

    apply_network_policy(
        namespace,
        &np_api,
        allow_access(
            "metrics",
            5122,
            oref,
            network_peers.and_then(|peers| peers.metrics.as_ref().map(Vec::as_slice)),
        ),
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
