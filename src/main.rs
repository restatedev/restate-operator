use actix_web::{
    App, HttpRequest, HttpResponse, HttpServer, Responder, get, middleware, web::Data,
};
use clap::Parser;
use kube::Client;
use prometheus::{Encoder, TextEncoder};

use restate_operator::controllers::State;
pub use restate_operator::{self, telemetry};

#[derive(Debug, clap::Parser)]
struct Arguments {
    #[arg(
        long = "aws-pod-identity-association-cluster",
        env = "AWS_POD_IDENTITY_ASSOCIATION_CLUSTER",
        value_name = "CLUSTERNAME"
    )]
    aws_pod_identity_association_cluster: Option<String>,

    #[arg(
        long = "gcp-workload-identity",
        env = "GCP_WORKLOAD_IDENTITY",
        value_name = "ENABLED",
        default_value = "false"
    )]
    gcp_workload_identity: bool,

    #[arg(
        long = "operator-namespace",
        env = "OPERATOR_NAMESPACE",
        value_name = "NAMESPACE"
    )]
    operator_namespace: String,

    #[arg(
        long = "operator-label-name",
        env = "OPERATOR_LABEL_NAME",
        value_name = "LABEL_NAME"
    )]
    operator_label_name: Option<String>,

    #[arg(
        long = "operator-label-value",
        env = "OPERATOR_LABEL_VALUE",
        value_name = "LABEL_VALUE"
    )]
    operator_label_value: Option<String>,

    #[arg(
        long = "tunnel-client-default-image",
        env = "OPERATOR_TUNNEL_CLIENT_DEFAULT_IMAGE",
        value_name = "IMAGE",
        default_value = "ghcr.io/restatedev/restate-cloud-tunnel-client:0.6.0"
    )]
    tunnel_client_default_image: String,

    #[arg(
        long = "cluster-dns",
        env = "CLUSTER_DNS",
        value_name = "CLUSTER_DNS",
        default_value = "cluster.local"
    )]
    cluster_dns: String,

    #[arg(
        long = "canary-image",
        env = "CANARY_IMAGE",
        value_name = "IMAGE",
        default_value = "alpine:3.21"
    )]
    canary_image: String,

    /// The name of the pod the operator is running in, used to attach operator-level events
    /// to it. Unset when running outside a cluster, in which case no such events are emitted.
    #[arg(
        long = "operator-pod-name",
        env = "OPERATOR_POD_NAME",
        value_name = "POD_NAME"
    )]
    operator_pod_name: Option<String>,

    /// The uid of the pod the operator is running in; without it, events attached to the pod
    /// are still recorded but do not show up in `kubectl describe pod`.
    #[arg(
        long = "operator-pod-uid",
        env = "OPERATOR_POD_UID",
        value_name = "POD_UID"
    )]
    operator_pod_uid: Option<String>,
}

#[get("/metrics")]
async fn metrics(c: Data<State>, _req: HttpRequest) -> impl Responder {
    let metrics = c.metrics();
    let encoder = TextEncoder::new();
    let mut buffer = vec![];
    encoder.encode(&metrics, &mut buffer).unwrap();
    HttpResponse::Ok().body(buffer)
}

/// Liveness: the process is up and the web server is serving.
///
/// Deliberately says nothing about whether the controllers are reconciling — that is what
/// `/ready` is for. Restarting the operator does not make a missing CRD appear, so a
/// controller waiting for its CRD must not fail a liveness probe.
#[get("/health")]
async fn health(_: HttpRequest) -> impl Responder {
    HttpResponse::Ok().json("healthy")
}

/// Readiness: every controller has its CRD and has started reconciling.
///
/// Returns `503` with the list of controllers still waiting, so that an operator whose CRDs
/// were never installed shows up as `NotReady` instead of sitting there looking healthy.
#[get("/ready")]
async fn ready(c: Data<State>, _req: HttpRequest) -> impl Responder {
    let report = c.readiness.report().await;
    if report.ready {
        HttpResponse::Ok().json(report)
    } else {
        HttpResponse::ServiceUnavailable().json(report)
    }
}

#[get("/")]
async fn index(c: Data<State>, _req: HttpRequest) -> impl Responder {
    let d = c.diagnostics().await;
    HttpResponse::Ok().json(&d)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    telemetry::init();

    let args: Arguments = Arguments::parse();

    // Validate cluster DNS suffix
    anyhow::ensure!(
        !args.cluster_dns.is_empty()
            && args
                .cluster_dns
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-'),
        "--cluster-dns must be a valid DNS suffix (e.g. 'cluster.local'), got: '{}'",
        args.cluster_dns
    );

    // Initialize Kubernetes controller state
    let state = State::new(
        args.aws_pod_identity_association_cluster,
        args.gcp_workload_identity,
        args.operator_namespace,
        args.operator_label_name,
        args.operator_label_value,
        args.tunnel_client_default_image,
        args.cluster_dns,
        args.canary_image,
        args.operator_pod_name,
        args.operator_pod_uid,
    );

    let client = Client::try_default()
        .await
        .expect("failed to create kube Client");

    let metric = restate_operator::Metrics::default()
        .register(&state.registry)
        .unwrap();

    // Start the controllers
    let cluster_controller = restate_operator::controllers::restatecluster::run(
        client.clone(),
        metric.clone(),
        state.clone(),
    );
    let cloud_environment_controller = restate_operator::controllers::restatecloudenvironment::run(
        client.clone(),
        metric.clone(),
        state.clone(),
    );
    let deployment_controller = restate_operator::controllers::restatedeployment::run(
        client.clone(),
        metric,
        state.clone(),
    );
    // The controllers wait for their CRDs independently; this reports all of them in one event
    // rather than one per controller, and finishes once they are all reconciling.
    let crd_wait_reporter =
        restate_operator::controllers::report_pending_crds(client, state.clone());

    tokio::pin!(cluster_controller);
    tokio::pin!(cloud_environment_controller);
    tokio::pin!(deployment_controller);
    tokio::pin!(crd_wait_reporter);

    // Start web server
    let server = HttpServer::new(move || {
        App::new()
            .app_data(Data::new(state.clone()))
            .wrap(
                middleware::Logger::default()
                    .exclude("/health")
                    .exclude("/ready"),
            )
            .service(index)
            .service(health)
            .service(ready)
            .service(metrics)
    })
    .bind("[::]:8080")?
    .shutdown_timeout(5)
    .run();

    tokio::pin!(server);

    // Both runtimes implements graceful shutdown, so poll until both are done
    let (_, _, _, _, server_result) = tokio::join!(
        cluster_controller,
        cloud_environment_controller,
        deployment_controller,
        crd_wait_reporter,
        server
    );
    server_result?;
    Ok(())
}
