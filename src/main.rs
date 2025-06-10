use actix_web::{
    get, middleware, web::Data, App, HttpRequest, HttpResponse, HttpServer, Responder,
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
        long = "operator-namespace",
        env = "OPERATOR_NAMESPACE",
        value_name = "NAMESPACE"
    )]
    operator_namespace: Option<String>,

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
}

#[get("/metrics")]
async fn metrics(c: Data<State>, _req: HttpRequest) -> impl Responder {
    let metrics = c.metrics();
    let encoder = TextEncoder::new();
    let mut buffer = vec![];
    encoder.encode(&metrics, &mut buffer).unwrap();
    HttpResponse::Ok().body(buffer)
}

#[get("/health")]
async fn health(_: HttpRequest) -> impl Responder {
    HttpResponse::Ok().json("healthy")
}

#[get("/")]
async fn index(c: Data<State>, _req: HttpRequest) -> impl Responder {
    let d = c.diagnostics().await;
    HttpResponse::Ok().json(&d)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    telemetry::init().await;

    let args: Arguments = Arguments::parse();

    // Initialize Kubernetes controller state
    let state = State::default()
        .with_aws_pod_identity_association_cluster(args.aws_pod_identity_association_cluster)
        .with_operator_namespace(args.operator_namespace)
        .with_operator_label_name(args.operator_label_name)
        .with_operator_label_value(args.operator_label_value);

    let client = Client::try_default()
        .await
        .expect("failed to create kube Client");

    let metric = restate_operator::Metrics::default()
        .register(&state.registry)
        .unwrap();

    // Start both controllers
    let cluster_controller = restate_operator::controllers::restatecluster::run(
        client.clone(),
        metric.clone(),
        state.clone(),
    );
    let deployment_controller =
        restate_operator::controllers::restatedeployment::run(client, metric, state.clone());

    tokio::pin!(cluster_controller);
    tokio::pin!(deployment_controller);

    // Start web server
    let server = HttpServer::new(move || {
        App::new()
            .app_data(Data::new(state.clone()))
            .wrap(middleware::Logger::default().exclude("/health"))
            .service(index)
            .service(health)
            .service(metrics)
    })
    .bind("0.0.0.0:8080")?
    .shutdown_timeout(5)
    .run();

    tokio::pin!(server);

    // Both runtimes implements graceful shutdown, so poll until both are done
    tokio::join!(cluster_controller, deployment_controller, server).2?;
    Ok(())
}
