//! The cron: served by the relay itself (`[embedded] services = ["cron"]`), registered in
//! Restate's own registry at the relay's plain route `platform/cron` (not in the workflow
//! registry). What cluster-setup's `lib.sh` `register_cron` did by hand.

use reqwest::{Method, StatusCode};
use tracing::info;

use crate::Error;
use crate::controllers::restatecluster::controller::Context;
use crate::controllers::restatedeployment::controller::check_admin_response;
use crate::controllers::restatedeployment::registration::post_deployment;
use crate::resources::restateclusters::CronStatus;

use super::multitenancy::{ComponentState, Dns};
use super::relay::{RELAY, SENDER_PORT};

/// The name of the relay's in-process cron, and of its static route's tunnel.
pub(super) const CRON: &str = "cron";

/// The `x-restate-cluster-name` the relay's static route for the cron expects.
const CRON_CLUSTER_NAME: &str = "env_platform";

/// Where Restate reaches the cron: the relay's plain route `platform/cron`, served in process
/// (the `http/<host>/<port>/` tail of a plain route is required but names nothing here).
pub(super) fn cron_uri(dns: Dns<'_>) -> String {
    format!(
        "{}/platform/cron/http/{CRON}/{SENDER_PORT}/",
        dns.url(RELAY, SENDER_PORT)
    )
}

/// The `POST /deployments` body, as `register_cron` sends it.
fn registration_payload(dns: Dns<'_>) -> serde_json::Value {
    serde_json::json!({
        "uri": cron_uri(dns),
        "additional_headers": { "x-restate-cluster-name": CRON_CLUSTER_NAME },
        // Same URI ⇒ the same deployment, updated in place (a new relay image re-discovered).
        "force": true,
    })
}

/// The cron's state before its registration is checked: registered at the relay image that
/// serves it, or waiting for Restate and the relay to be Ready.
pub(super) fn cron_state(status: &CronStatus, relay_image: &str) -> ComponentState {
    if is_registered(status, relay_image) {
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
    }
}

fn is_registered(status: &CronStatus, relay_image: &str) -> bool {
    status.deployment_id.is_some() && status.registered_image.as_deref() == Some(relay_image)
}

/// Make sure Restate has the cron, at the relay's current image: re-register it when the image
/// changed or Restate doesn't know the recorded deployment (e.g. a wiped cluster). Updates
/// `status` with what's registered. Admin API failures are reported in the state (and retried
/// by the requeue), not as reconcile errors.
pub(super) async fn register_cron(
    ctx: &Context,
    dns: Dns<'_>,
    relay_image: &str,
    status: &mut CronStatus,
) -> Result<ComponentState, Error> {
    match ensure_registered(ctx, dns, relay_image, status).await {
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
    relay_image: &str,
    status: &mut CronStatus,
) -> Result<(), Error> {
    let admin = url::Url::parse(&dns.url("restate", 9070))?;

    if is_registered(status, relay_image)
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
        relay_image,
        services = ?registered.services,
        "Registered the cron with Restate"
    );
    status.deployment_id = Some(registered.id);
    status.registered_image = Some(relay_image.to_owned());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DNS: Dns<'static> = Dns {
        namespace: "restate",
        cluster_dns: "cluster.local",
    };

    #[test]
    fn registration_at_the_relays_plain_route() {
        assert_eq!(
            registration_payload(DNS),
            serde_json::json!({
                "uri": "http://relay.restate.svc.cluster.local:9080/platform/cron/http/cron/9080/",
                "additional_headers": { "x-restate-cluster-name": "env_platform" },
                "force": true,
            })
        );
    }

    #[test]
    fn a_new_relay_image_is_not_registered() {
        let status = CronStatus {
            deployment_id: Some("dp_1".into()),
            registered_image: Some("relay:demo".into()),
        };
        assert!(is_registered(&status, "relay:demo"));
        assert!(cron_state(&status, "relay:demo").ready);
        assert!(!is_registered(&status, "relay:v2"));
        assert!(!cron_state(&status, "relay:v2").ready);
        assert!(!is_registered(&CronStatus::default(), "relay:demo"));
    }
}
