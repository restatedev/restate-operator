use std::convert::Into;
use std::string::ToString;

use k8s_openapi::api::networking::v1::{IPBlock, NetworkPolicy, NetworkPolicyEgressRule, NetworkPolicyPeer, NetworkPolicyPort};
use k8s_openapi::api::networking::v1::NetworkPolicySpec;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::{
    api::{Patch, PatchParams},
    Api,
    Client,
};
use tracing::debug;

use crate::Error;

const DENY_ALL: NetworkPolicy = NetworkPolicy {
    metadata: ObjectMeta {
        name: Some("deny-all".into()),
        ..Default::default()
    },
    spec: Some(NetworkPolicySpec {
        policy_types: Some(vec!["Egress".into(), "Ingress".into()]),
        ..Default::default()
    }),
};

const ALLOW_DNS: NetworkPolicy = NetworkPolicy {
    metadata: ObjectMeta {
        name: Some("allow-egress-to-kube-dns".into()),
        ..Default::default()
    },
    spec: Some(NetworkPolicySpec {
        policy_types: Some(vec!["Egress".into()]),
        egress: Some(vec![NetworkPolicyEgressRule {
            to: Some(vec![NetworkPolicyPeer {
                pod_selector: Some(LabelSelector {
                    match_labels: Some([("k8s-app".to_string(), "kube-dns".to_string())].into()),
                    ..Default::default()
                }),
                namespace_selector: Some(LabelSelector {
                    match_labels: Some([("kubernetes.io/metadata.name".to_string(), "kube-system".to_string())].into()),
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
};

const ALLOW_PUBLIC: NetworkPolicy = NetworkPolicy {
    metadata: ObjectMeta {
        name: Some("allow-restate-egress-to-public-internet".into()),
        ..Default::default()
    },
    spec: Some(NetworkPolicySpec {
        pod_selector: LabelSelector {
            match_labels: Some([("app".to_string(), "restate".to_string())].into()),
            ..Default::default()
        },
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
};

pub async fn reconcile_network_policies(client: Client, namespace: &str) -> Result<(), Error> {
    let np_api: Api<NetworkPolicy> = Api::namespaced(client, namespace);

    apply_network_policy(namespace, &np_api, DENY_ALL).await?;
    apply_network_policy(namespace, &np_api, ALLOW_DNS).await?;
    apply_network_policy(namespace, &np_api, ALLOW_PUBLIC).await?;

    // Namespaces that should be allowed to access an instance namespace
    let allow_system_ingress = serde_json::json!({
        "apiVersion": "networking.k8s.io/v1",
        "kind": "NetworkPolicy",
        "metadata": {
          "name": "allow-system",
          "namespace": format!("{namespace}"),
        },
        "spec": {
          "podSelector": {},
          "policyTypes": ["Ingress"],
          "ingress": [
            {
              "from": [
                {
                  "namespaceSelector": {
                    "matchLabels": {
                      "kubernetes.io/metadata.name": "monitoring"
                    }
                  }
                },
                {
                  "namespaceSelector": {
                    "matchLabels": {
                      "kubernetes.io/metadata.name": "cnpg-system"
                    }
                  }
                },
                {
                  "namespaceSelector": {
                    "matchLabels": {
                      "kubernetes.io/metadata.name": "coredb-operator"
                    }
                  }
                },
                {
                  "namespaceSelector": {
                    "matchLabels": {
                      "kubernetes.io/metadata.name": "traefik"
                    }
                  }
                },
                {
                  "namespaceSelector": {
                    "matchLabels": {
                      "kubernetes.io/metadata.name": "tembo-system"
                    }
                  }
                }
              ]
            }
          ]
        }
    });
    apply_network_policy(namespace, &np_api, allow_system_ingress).await?;

    // Namespaces that should be accessible from instance namespaces
    let allow_system_egress = serde_json::json!({
        "apiVersion": "networking.k8s.io/v1",
        "kind": "NetworkPolicy",
        "metadata": {
          "name": "allow-system-egress",
          "namespace": format!("{namespace}"),
        },
        "spec": {
          "podSelector": {},
          "policyTypes": ["Egress"],
          "egress": [
            {
              "to": [
                {
                  "namespaceSelector": {
                    "matchLabels": {
                      "kubernetes.io/metadata.name": "minio"
                    }
                  }
                }
              ]
            }
          ]
        }
    });
    apply_network_policy(namespace, &np_api, allow_system_egress).await?;

    let allow_public_internet = serde_json::json!({
        "apiVersion": "networking.k8s.io/v1",
        "kind": "NetworkPolicy",
        "metadata": {
          "name": "allow-egress-to-internet",
          "namespace": format!("{namespace}"),
        },
        "spec": {
          "podSelector": {},
          "policyTypes": ["Egress"],
          "egress": [
            {
              "to": [
                {
                  "ipBlock": {
                    "cidr": "0.0.0.0/0",
                    "except": [
                      "10.0.0.0/8",
                      "172.16.0.0/12",
                      "192.168.0.0/16"
                    ]
                  }
                }
              ]
            }
          ]
        }
    });
    apply_network_policy(namespace, &np_api, allow_public_internet).await?;

    let allow_within_namespace = serde_json::json!({
        "apiVersion": "networking.k8s.io/v1",
        "kind": "NetworkPolicy",
        "metadata": {
          "name": "allow-within-namespace",
          "namespace": format!("{namespace}"),
        },
        "spec": {
          "podSelector": {},
          "policyTypes": ["Ingress", "Egress"],
          "ingress": [
            {
              "from": [
                {
                  "podSelector": {}
                }
              ]
            }
          ],
          "egress": [
            {
              "to": [
                {
                  "podSelector": {}
                }
              ]
            }
          ]
        }
    });
    apply_network_policy(namespace, &np_api, allow_within_namespace).await?;

    Ok(())
}

async fn apply_network_policy(
    namespace: &str,
    np_api: &Api<NetworkPolicy>,
    mut np: NetworkPolicy,
) -> Result<(), Error> {
    np.metadata.namespace = Some(namespace.to_string())
    let name = np.metadata.name.as_ref().unwrap();
    let params: PatchParams = PatchParams::apply("restate-cloud").force();
    debug!(
        "Applying Network Policy {} in namespace {}",
        name, namespace
    );
    np_api.patch(&name, &params, &Patch::Apply(&np)).await?;
    Ok(())
}
