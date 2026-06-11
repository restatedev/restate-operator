use k8s_openapi::api::autoscaling::v2::HorizontalPodAutoscaler;
use kube::api::{Api, Patch, PatchParams};
use kube::{Resource, ResourceExt};
use serde_json::{Value, json};
use tracing::*;

use crate::controllers::restatedeployment::controller::{APP_MANAGED_BY_LABEL, OWNED_BY_LABEL};
use crate::resources::restatedeployments::RestateDeployment;
use crate::{Error, Result};

/// Field manager used for server-side applies of operator-managed HPAs.
const FIELD_MANAGER: &str = "restate-operator/autoscaling";

/// What to do with the operator-managed HPA for one owned, non-latest, **active**
/// ReplicaSet. Encodes the active-version half of the invariant: a non-latest
/// version has an operator HPA iff it is active *and* autoscaling is configured.
/// (Inactive versions always have their HPA removed before scale-down; that path
/// is unconditional and not modelled here.)
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum HpaPlan {
    /// Create/update the HPA for this version.
    Ensure,
    /// Autoscaling was disabled/removed while the version is still active: drop
    /// any HPA we created and restore the version to its full replica count.
    RemoveAndRestore,
    /// Do nothing — e.g. the RestateDeployment is being deleted, in which case
    /// owned HPAs are garbage-collected with it.
    Skip,
}

/// Decide the HPA action for an active, non-latest version. Pure so it can be
/// table-tested without a cluster.
pub(crate) fn plan_active_version_hpa(autoscaling_configured: bool, rd_deleting: bool) -> HpaPlan {
    if rd_deleting {
        HpaPlan::Skip
    } else if autoscaling_configured {
        HpaPlan::Ensure
    } else {
        HpaPlan::RemoveAndRestore
    }
}

/// Build the HorizontalPodAutoscaler object for a single non-latest version.
///
/// `template` is the user-supplied pass-through HPA `.spec` (without
/// `scaleTargetRef`). The operator injects `scaleTargetRef` to point at this
/// version's ReplicaSet, floors `minReplicas` at 1 (there is no scale-to-zero in
/// ReplicaSet mode), and sets ownership/labels so the HPA is discovered by the
/// controller's watch and garbage-collected with the RestateDeployment.
///
/// The HPA shares the versioned ReplicaSet's name (1:1). Metrics are passed
/// through verbatim; Resource (CPU/memory) metrics are automatically scoped to
/// this version's pods via the target ReplicaSet's own pod selector (which
/// already carries the pod-template-hash), so no metric-selector injection is
/// needed for the CPU-based first cut.
pub(crate) fn build_version_hpa(
    rsd: &RestateDeployment,
    namespace: &str,
    versioned_name: &str,
    template: &Value,
) -> Value {
    let owner_ref = rsd
        .controller_owner_ref(&())
        .expect("RestateDeployment to have a uid");

    // Start from the user's template and inject/normalise operator-owned fields.
    let mut spec = template.clone();
    if !spec.is_object() {
        spec = json!({});
    }
    let spec_obj = spec.as_object_mut().expect("spec is a JSON object");

    if spec_obj.contains_key("scaleTargetRef") {
        warn!(
            "autoscaling template for {versioned_name} sets scaleTargetRef; the operator manages it per version and is overriding the provided value (it should be omitted)"
        );
    }
    spec_obj.insert(
        "scaleTargetRef".to_string(),
        json!({
            "apiVersion": "apps/v1",
            "kind": "ReplicaSet",
            "name": versioned_name,
        }),
    );

    // No scale-to-zero in ReplicaSet mode: floor minReplicas at 1.
    if let Some(min) = spec_obj.get("minReplicas").and_then(Value::as_i64)
        && min < 1
    {
        warn!(
            "minReplicas {min} for {versioned_name} is below 1; flooring to 1 (no scale-to-zero in ReplicaSet mode)"
        );
        spec_obj.insert("minReplicas".to_string(), json!(1));
    }

    json!({
        "apiVersion": "autoscaling/v2",
        "kind": "HorizontalPodAutoscaler",
        "metadata": {
            "name": versioned_name,
            "namespace": namespace,
            "labels": {
                APP_MANAGED_BY_LABEL: "restate-operator",
                OWNED_BY_LABEL: rsd.name_any(),
            },
            "ownerReferences": [owner_ref],
        },
        "spec": spec,
    })
}

/// Create or update the HPA for a non-latest version (server-side apply).
pub(crate) async fn reconcile_version_hpa(
    client: &kube::Client,
    rsd: &RestateDeployment,
    namespace: &str,
    versioned_name: &str,
    template: &Value,
) -> Result<()> {
    let hpa = build_version_hpa(rsd, namespace, versioned_name, template);
    let api: Api<HorizontalPodAutoscaler> = Api::namespaced(client.clone(), namespace);

    debug!("Applying HPA {versioned_name} for non-latest version in namespace {namespace}");
    api.patch(
        versioned_name,
        &PatchParams::apply(FIELD_MANAGER).force(),
        &Patch::Apply(&hpa),
    )
    .await?;

    Ok(())
}

/// Delete the HPA for a version if it exists. Returns whether an HPA was present.
/// Idempotent: a missing HPA is not an error.
pub(crate) async fn delete_version_hpa(
    client: &kube::Client,
    namespace: &str,
    versioned_name: &str,
) -> Result<bool> {
    let api: Api<HorizontalPodAutoscaler> = Api::namespaced(client.clone(), namespace);
    match api.delete(versioned_name, &Default::default()).await {
        Ok(_) => {
            debug!("Deleted HPA {versioned_name} in namespace {namespace}");
            Ok(true)
        }
        Err(kube::Error::Api(err)) if err.code == 404 => Ok(false),
        Err(err) => Err(Error::KubeError(err)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{Request, Response};
    use kube::client::Body;
    use std::convert::Infallible;

    fn make_rsd() -> RestateDeployment {
        let spec = serde_json::from_value(json!({
            "replicas": 3,
            "revisionHistoryLimit": 10,
            "template": {
                "metadata": null,
                "spec": { "containers": [{ "name": "main", "image": "greeter:v1" }] }
            },
            "restate": {
                "register": { "cluster": null, "cloud": null, "service": null, "url": "http://localhost:9070/" },
                "servicePath": null,
                "useHttp11": null,
                "drainDelaySeconds": null
            }
        }))
        .expect("test RestateDeploymentSpec deserializes");

        let mut rsd = RestateDeployment::new("greeter", spec);
        rsd.metadata.uid = Some("uid-123".to_string());
        rsd
    }

    fn template() -> Value {
        json!({
            "minReplicas": 1,
            "maxReplicas": 10,
            "metrics": [{
                "type": "Resource",
                "resource": {
                    "name": "cpu",
                    "target": { "type": "Utilization", "averageUtilization": 70 }
                }
            }]
        })
    }

    #[test]
    fn injects_scale_target_ref_to_versioned_replicaset() {
        let hpa = build_version_hpa(&make_rsd(), "ns", "greeter-abc", &template());
        assert_eq!(hpa["spec"]["scaleTargetRef"]["apiVersion"], "apps/v1");
        assert_eq!(hpa["spec"]["scaleTargetRef"]["kind"], "ReplicaSet");
        assert_eq!(hpa["spec"]["scaleTargetRef"]["name"], "greeter-abc");
    }

    #[test]
    fn passes_through_user_template_verbatim() {
        let template = template();
        let hpa = build_version_hpa(&make_rsd(), "ns", "greeter-abc", &template);
        assert_eq!(hpa["spec"]["maxReplicas"], 10);
        assert_eq!(hpa["spec"]["metrics"], template["metrics"]);
    }

    #[test]
    fn floors_min_replicas_at_one() {
        let mut t = template();
        t["minReplicas"] = json!(0);
        let hpa = build_version_hpa(&make_rsd(), "ns", "greeter-abc", &t);
        assert_eq!(hpa["spec"]["minReplicas"], 1);
    }

    #[test]
    fn leaves_valid_min_replicas_untouched() {
        let mut t = template();
        t["minReplicas"] = json!(3);
        let hpa = build_version_hpa(&make_rsd(), "ns", "greeter-abc", &t);
        assert_eq!(hpa["spec"]["minReplicas"], 3);
    }

    #[test]
    fn owned_by_restate_deployment_and_labelled() {
        let hpa = build_version_hpa(&make_rsd(), "ns", "greeter-abc", &template());
        assert_eq!(
            hpa["metadata"]["labels"][APP_MANAGED_BY_LABEL],
            "restate-operator"
        );
        assert_eq!(hpa["metadata"]["labels"][OWNED_BY_LABEL], "greeter");
        assert_eq!(hpa["metadata"]["ownerReferences"][0]["uid"], "uid-123");
        assert_eq!(
            hpa["metadata"]["ownerReferences"][0]["kind"],
            "RestateDeployment"
        );
    }

    #[test]
    fn tolerates_non_object_template() {
        // A malformed/empty template must not panic; scaleTargetRef is still set.
        let hpa = build_version_hpa(&make_rsd(), "ns", "greeter-abc", &json!(null));
        assert_eq!(hpa["spec"]["scaleTargetRef"]["name"], "greeter-abc");
    }

    // The active-version decision matrix (the invariant). The inactive case is a
    // separate, unconditional delete and is exercised by the e2e teardown test.
    #[test]
    fn plan_ensures_hpa_when_active_and_configured() {
        assert_eq!(plan_active_version_hpa(true, false), HpaPlan::Ensure);
    }

    #[test]
    fn plan_removes_and_restores_when_active_but_unconfigured() {
        // autoscaling removed while the version is still active
        assert_eq!(
            plan_active_version_hpa(false, false),
            HpaPlan::RemoveAndRestore
        );
    }

    #[test]
    fn plan_skips_while_restate_deployment_is_deleting() {
        // owned HPAs are GC'd with the RestateDeployment, so don't stamp during deletion
        assert_eq!(plan_active_version_hpa(true, true), HpaPlan::Skip);
        assert_eq!(plan_active_version_hpa(false, true), HpaPlan::Skip);
    }

    // --- mocked-Client tests: a tower service stands in for the apiserver (the
    // kube-rs idiom), exercising the real HPA API calls deterministically. ---

    fn client_with<F>(handler: F) -> kube::Client
    where
        F: Fn(Request<Body>) -> Response<Body> + Clone + Send + 'static,
    {
        let svc = tower::service_fn(move |req: Request<Body>| {
            let handler = handler.clone();
            async move { Ok::<_, Infallible>(handler(req)) }
        });
        kube::Client::new(svc, "default")
    }

    fn json_body(status: u16, v: serde_json::Value) -> Response<Body> {
        Response::builder()
            .status(status)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&v).unwrap()))
            .unwrap()
    }

    #[tokio::test]
    async fn delete_version_hpa_tolerates_missing() {
        let client = client_with(|_req| {
            json_body(
                404,
                json!({
                    "kind": "Status", "apiVersion": "v1", "status": "Failure",
                    "reason": "NotFound", "code": 404, "message": "not found"
                }),
            )
        });
        assert!(
            !delete_version_hpa(&client, "ns", "greeter-abc")
                .await
                .unwrap()
        );
    }
}
