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
    Api, Client,
};
use tracing::debug;

use crate::reconcilers::{label_selector, object_meta};
use crate::{Error, RestateClusterNetworkPeers};

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
    peers: Option<&[NetworkPolicyPeer]>,
) -> NetworkPolicy {
    NetworkPolicy {
        metadata: object_meta(base_metadata, format!("allow-{port_name}-access")),
        spec: Some(NetworkPolicySpec {
            pod_selector: label_selector(base_metadata),
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

fn allow_egress(
    base_metadata: &ObjectMeta,
    egress: Option<&[crate::NetworkPolicyEgressRule]>,
) -> NetworkPolicy {
    NetworkPolicy {
        metadata: object_meta(base_metadata, "allow-restate-egress"),
        spec: Some(NetworkPolicySpec {
            pod_selector: label_selector(base_metadata),
            policy_types: Some(vec!["Egress".into()]),
            egress: egress.map(|e| e.iter().map(|r| r.clone().into()).collect()), // if none, this policy will do nothing
            ..Default::default()
        }),
    }
}

pub async fn reconcile_network_policies(
    client: Client,
    namespace: &str,
    base_metadata: &ObjectMeta,
    network_peers: Option<&RestateClusterNetworkPeers>,
    network_egress_rules: Option<&[crate::NetworkPolicyEgressRule]>,
    aws_pod_identity_enabled: bool,
) -> Result<(), Error> {
    let np_api: Api<NetworkPolicy> = Api::namespaced(client, namespace);

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
        allow_egress(base_metadata, network_egress_rules),
    )
    .await?;

    apply_network_policy(
        namespace,
        &np_api,
        allow_access(
            "ingress",
            8080,
            base_metadata,
            network_peers.and_then(|peers| peers.ingress.as_deref()),
        ),
    )
    .await?;

    apply_network_policy(
        namespace,
        &np_api,
        allow_access(
            "admin",
            9070,
            base_metadata,
            network_peers.and_then(|peers| peers.admin.as_deref()),
        ),
    )
    .await?;

    apply_network_policy(
        namespace,
        &np_api,
        allow_access(
            "metrics",
            5122,
            base_metadata,
            network_peers.and_then(|peers| peers.metrics.as_deref()),
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
