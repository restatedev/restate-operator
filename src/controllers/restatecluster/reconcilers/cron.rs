//! The cron: one Deployment for all tenants, registered in Restate's own registry at the
//! relay's plain route `platform/cron` (not in the workflow registry). What cluster-setup's
//! `k8s/30-system-services.yaml` and `lib.sh` `register_cron` do by hand.

use k8s_openapi::api::apps::v1::{Deployment, DeploymentSpec};
use k8s_openapi::api::core::v1::{
    Container, ContainerPort, PodSpec, PodTemplateSpec, Probe, Service, ServicePort, ServiceSpec,
    TCPSocketAction,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use reqwest::{Method, StatusCode};
use tracing::info;

use crate::Error;
use crate::controllers::restatecluster::controller::Context;
use crate::controllers::restatedeployment::controller::check_admin_response;
use crate::controllers::restatedeployment::registration::post_deployment;
use crate::resources::restateclusters::{CronSpec, CronStatus};

use super::multitenancy::{
    Apis, ComponentState, Dns, apply, component_label_selector, component_labels,
    component_selector, delete_if_exists, deployment_rollout,
};
use super::object_meta;
use super::relay::{RELAY, SENDER_PORT};

pub(super) const CRON: &str = "cron";
pub(super) const CRON_PORT: u16 = 9080;
const DEFAULT_REPLICAS: i32 = 3;

/// The `x-restate-cluster-name` the relay's static route for the cron expects.
const CRON_CLUSTER_NAME: &str = "env_platform";

fn cron_deployment(base_metadata: &ObjectMeta, cron: &CronSpec) -> Deployment {
    Deployment {
        metadata: object_meta(base_metadata, CRON),
        spec: Some(DeploymentSpec {
            replicas: Some(cron.replicas.unwrap_or(DEFAULT_REPLICAS)),
            selector: component_label_selector(base_metadata, CRON),
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(component_labels(base_metadata, CRON)),
                    ..Default::default()
                }),
                spec: Some(PodSpec {
                    containers: vec![Container {
                        name: CRON.into(),
                        image: Some(cron.image.clone()),
                        image_pull_policy: cron.image_pull_policy.clone(),
                        env: cron.env.clone(),
                        ports: Some(vec![ContainerPort {
                            name: Some("h2c".into()),
                            container_port: CRON_PORT.into(),
                            ..Default::default()
                        }]),
                        readiness_probe: Some(Probe {
                            tcp_socket: Some(TCPSocketAction {
                                port: IntOrString::Int(CRON_PORT.into()),
                                ..Default::default()
                            }),
                            initial_delay_seconds: Some(3),
                            period_seconds: Some(5),
                            failure_threshold: Some(30),
                            ..Default::default()
                        }),
                        resources: cron.resources.clone(),
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
            },
            ..Default::default()
        }),
        status: None,
    }
}

/// The relay dials this directly (h2c, the static route of its config).
fn cron_service(base_metadata: &ObjectMeta) -> Service {
    Service {
        metadata: object_meta(base_metadata, CRON),
        spec: Some(ServiceSpec {
            selector: Some(component_selector(base_metadata, CRON)),
            ports: Some(vec![ServicePort {
                name: Some("h2c".into()),
                port: CRON_PORT.into(),
                target_port: Some(IntOrString::Int(CRON_PORT.into())),
                app_protocol: Some("kubernetes.io/h2c".into()),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        status: None,
    }
}

/// Where Restate reaches the cron: through the relay's plain route `platform/cron`, whose
/// `http/<host>/<port>/` tail names the cron's Service.
pub(super) fn cron_uri(dns: Dns<'_>) -> String {
    format!(
        "{}/platform/cron/http/{}/{CRON_PORT}/",
        dns.url(RELAY, SENDER_PORT),
        dns.host(CRON)
    )
}

/// The `POST /deployments` body, as `register_cron` sends it.
fn registration_payload(dns: Dns<'_>) -> serde_json::Value {
    serde_json::json!({
        "uri": cron_uri(dns),
        "additional_headers": { "x-restate-cluster-name": CRON_CLUSTER_NAME },
        // Same URI ⇒ the same deployment, updated in place (a new image re-discovered).
        "force": true,
    })
}

/// What the cron's reconcile found, before its registration.
pub(super) struct CronOutcome {
    pub status: CronStatus,
    pub state: ComponentState,
    pub rolled_out: bool,
}

pub(super) async fn reconcile_cron(
    ctx: &Context,
    name: &str,
    base_metadata: &ObjectMeta,
    cron: &CronSpec,
    previous: Option<&CronStatus>,
) -> Result<CronOutcome, Error> {
    let apis = Apis::new(ctx, name);
    apply(&apis.services, &cron_service(base_metadata)).await?;
    let deployment = apply(&apis.deployments, &cron_deployment(base_metadata, cron)).await?;
    let (ready_replicas, rolled_out) = deployment_rollout(&deployment);

    // The registration carries over until it's checked again, once Restate is Ready.
    let status = CronStatus {
        ready_replicas,
        deployment_id: previous.and_then(|p| p.deployment_id.clone()),
        registered_image: previous.and_then(|p| p.registered_image.clone()),
    };
    let replicas = cron.replicas.unwrap_or(DEFAULT_REPLICAS);
    let state = if !rolled_out {
        ComponentState::waiting(
            "CronRollingOut",
            format!("{ready_replicas}/{replicas} cron pods ready on the current spec"),
        )
    } else if is_registered(&status, cron) {
        ComponentState::ready(
            "CronRegistered",
            format!(
                "registered as {}",
                status.deployment_id.as_deref().unwrap_or_default()
            ),
        )
    } else {
        ComponentState::waiting(
            "CronNotRegistered",
            "waiting for Restate and the relay to be Ready to register the cron",
        )
    };

    Ok(CronOutcome {
        status,
        state,
        rolled_out,
    })
}

fn is_registered(status: &CronStatus, cron: &CronSpec) -> bool {
    status.deployment_id.is_some() && status.registered_image.as_deref() == Some(&cron.image)
}

/// Make sure Restate has the cron, at its current image: re-register it when the image
/// changed or Restate doesn't know the recorded deployment (e.g. a wiped cluster). Updates
/// `status` with what's registered. Admin API failures are reported in the state (and retried
/// by the requeue), not as reconcile errors.
pub(super) async fn register_cron(
    ctx: &Context,
    dns: Dns<'_>,
    cron: &CronSpec,
    status: &mut CronStatus,
) -> Result<ComponentState, Error> {
    match ensure_registered(ctx, dns, cron, status).await {
        Ok(()) => Ok(ComponentState::ready(
            "CronRegistered",
            format!(
                "registered as {}",
                status.deployment_id.as_deref().unwrap_or_default()
            ),
        )),
        Err(err @ (Error::AdminCallFailed(_) | Error::AdminCallRejected { .. })) => {
            Ok(ComponentState::waiting(
                "CronRegistrationFailed",
                format!("registering the cron: {err}"),
            ))
        }
        Err(err) => Err(err),
    }
}

async fn ensure_registered(
    ctx: &Context,
    dns: Dns<'_>,
    cron: &CronSpec,
    status: &mut CronStatus,
) -> Result<(), Error> {
    let admin = url::Url::parse(&dns.url("restate", 9070))?;

    if is_registered(status, cron)
        && let Some(id) = status.deployment_id.as_deref()
    {
        let resp = ctx
            .http_client
            .request(Method::GET, admin.join(&format!("/deployments/{id}"))?)
            .send()
            .await
            .map_err(Error::AdminCallFailed)?;
        if resp.status() != StatusCode::NOT_FOUND {
            check_admin_response(resp).await?;
            return Ok(());
        }
        info!(
            deployment_id = id,
            "Restate no longer knows the cron, registering it again"
        );
    }

    let registered = post_deployment(
        ctx.http_client
            .request(Method::POST, admin.join("/deployments")?),
        &registration_payload(dns),
    )
    .await?;
    info!(
        deployment_id = %registered.id,
        image = %cron.image,
        services = ?registered.services,
        "Registered the cron with Restate"
    );
    status.deployment_id = Some(registered.id);
    status.registered_image = Some(cron.image.clone());
    Ok(())
}

pub(super) async fn cleanup_cron(ctx: &Context, name: &str) -> Result<(), Error> {
    let apis = Apis::new(ctx, name);
    delete_if_exists(&apis.deployments, CRON).await?;
    delete_if_exists(&apis.services, CRON).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controllers::restatecluster::reconcilers::multitenancy::tests::base_metadata;

    const DNS: Dns<'static> = Dns {
        namespace: "restate",
        cluster_dns: "cluster.local",
    };

    fn cron_spec() -> CronSpec {
        CronSpec {
            image: "cron:demo".into(),
            replicas: None,
            image_pull_policy: Some("Always".into()),
            resources: None,
            env: None,
        }
    }

    #[test]
    fn registration_matches_register_cron() {
        // cluster-setup/lib.sh: CRON_URI and the jq body of register_cron
        assert_eq!(
            registration_payload(DNS),
            serde_json::json!({
                "uri": "http://relay.restate.svc.cluster.local:9080/platform/cron/http/cron.restate.svc.cluster.local/9080/",
                "additional_headers": { "x-restate-cluster-name": "env_platform" },
                "force": true,
            })
        );
    }

    #[test]
    fn deployment_and_service_match_cluster_setups_cron() {
        let deployment = cron_deployment(&base_metadata(), &cron_spec());
        let spec = deployment.spec.unwrap();
        assert_eq!(spec.replicas, Some(3));
        let pod = spec.template.spec.unwrap();
        let probe = pod.containers[0].readiness_probe.as_ref().unwrap();
        assert_eq!(probe.failure_threshold, Some(30));
        assert_eq!(
            spec.template.metadata.unwrap().labels.unwrap()["app.kubernetes.io/name"],
            "cron"
        );

        let svc = cron_service(&base_metadata()).spec.unwrap();
        let port = &svc.ports.unwrap()[0];
        assert_eq!(port.port, 9080);
        assert_eq!(port.app_protocol.as_deref(), Some("kubernetes.io/h2c"));
    }

    #[test]
    fn a_new_image_is_not_registered() {
        let status = CronStatus {
            ready_replicas: 3,
            deployment_id: Some("dp_1".into()),
            registered_image: Some("cron:demo".into()),
        };
        assert!(is_registered(&status, &cron_spec()));
        assert!(!is_registered(
            &status,
            &CronSpec {
                image: "cron:v2".into(),
                ..cron_spec()
            }
        ));
        assert!(!is_registered(&CronStatus::default(), &cron_spec()));
    }
}
