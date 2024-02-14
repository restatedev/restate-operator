use std::ffi::OsString;

use actix_web::{
    get, middleware, web::Data, App, HttpRequest, HttpResponse, HttpServer, Responder,
};
use clap::Parser;
use prometheus::{Encoder, TextEncoder};

pub use restate_operator::{self, telemetry, State};

#[derive(Debug, clap::Parser)]
struct Arguments {
    #[arg(
        long = "aws-pod-identity-association-cluster",
        env = "AWS_POD_IDENTITY_ASSOCIATION_CLUSTER",
        value_name = "CLUSTERNAME"
    )]
    aws_pod_identity_association_cluster: Option<OsString>,
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

    // Initiatilize Kubernetes controller state
    let state = State::default().with_aws_pod_identity_association_cluster(
        args.aws_pod_identity_association_cluster
            .and_then(|s| s.to_str().map(|s| s.to_string())),
    );
    let controller = restate_operator::run(state.clone());
    tokio::pin!(controller);

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
    tokio::join!(controller, server).1?;
    Ok(())
}
