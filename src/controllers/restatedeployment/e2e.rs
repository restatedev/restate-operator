//! Docker-backed compatibility tests for the deployment-usage SQL.
//!
//! These tests deliberately use the same query builder and response types as the controller.
//! They validate the Restate schema and invocation lifecycle that this repository does not own.

use std::net::{Ipv4Addr, SocketAddr};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use restate_sdk::prelude::*;
use serde_json::json;
use tokio::net::TcpListener;

use super::cleanup::{CleanupMode, DeploymentUsageRows, deployment_usage_query};

// Restate 1.6.2. Digest-pinning keeps a registry tag rewrite from silently changing the
// compatibility contract of this test.
const LEGACY_IMAGE: &str = "docker.io/restatedev/restate@sha256:e8e072c174bb0f997331c055b7bd84cae6ddc8c7c31ac7c8a197bdc935eec2f5";
const HOST_GATEWAY: &str = "host.docker.internal";
const CONVERGE_TIMEOUT: Duration = Duration::from_secs(90);

static SLOW_STARTED: AtomicBool = AtomicBool::new(false);
const SLOW_HANDLER_SLEEP: Duration = Duration::from_secs(12);

struct Greeter;

#[service]
impl Greeter {
    #[handler]
    async fn greet(&self, _ctx: Context<'_>, name: String) -> HandlerResult<String> {
        Ok(format!("Greetings {name}"))
    }

    /// `Context::sleep` creates a durable suspended invocation. Tests poll the resulting state
    /// rather than assuming a particular scheduling delay.
    #[handler]
    async fn slow(&self, ctx: Context<'_>) -> HandlerResult<()> {
        SLOW_STARTED.store(true, Ordering::Release);
        ctx.sleep(SLOW_HANDLER_SLEEP).await?;
        Ok(())
    }
}

struct RestateServer {
    container_id: String,
    ingress_url: String,
    admin_url: String,
}

impl RestateServer {
    fn start(image: &str) -> Self {
        let mut args = vec![
            "run".into(),
            "--detach".into(),
            "--add-host".into(),
            format!("{HOST_GATEWAY}:host-gateway"),
            "--publish".into(),
            "127.0.0.1::8080".into(),
            "--publish".into(),
            "127.0.0.1::9070".into(),
        ];
        args.push(image.into());
        // One partition keeps this compatibility suite focused on schema/lifecycle semantics
        // rather than spending most of its budget waiting for a fresh 24-partition server.
        args.extend(["--default-num-partitions".into(), "1".into()]);
        let arg_refs = args.iter().map(String::as_str).collect::<Vec<_>>();
        let container_id = docker(&arg_refs);
        Self {
            ingress_url: format!("http://{}", host_port(&container_id, 8080)),
            admin_url: format!("http://{}", host_port(&container_id, 9070)),
            container_id,
        }
    }
}

impl Drop for RestateServer {
    fn drop(&mut self) {
        // Preserve the original failure; cleanup failures must not double-panic and hide it.
        let _ = Command::new("docker")
            .args(["rm", "--force", "--volumes", &self.container_id])
            .output();
    }
}

fn docker(args: &[&str]) -> String {
    let output = Command::new("docker")
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("failed to run `docker {}`: {err}", args.join(" ")));
    assert!(
        output.status.success(),
        "`docker {}` failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn host_port(container_id: &str, container_port: u16) -> String {
    docker(&["port", container_id, &container_port.to_string()])
        .lines()
        .next()
        .expect("container port is published")
        .trim()
        .to_owned()
}

async fn serve_greeter() -> String {
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)))
        .await
        .expect("bind test service");
    let port = listener.local_addr().expect("listener address").port();
    tokio::spawn(HttpServer::new(Endpoint::builder().bind(Greeter).build()).serve(listener));
    format!("http://{HOST_GATEWAY}:{port}")
}

struct Admin {
    client: reqwest::Client,
    admin_url: String,
}

impl Admin {
    async fn register(&self, endpoint_url: &str) -> String {
        let response = self
            .client
            .post(format!("{}/deployments", self.admin_url))
            .json(&json!({ "uri": endpoint_url }))
            .send()
            .await
            .expect("registration request");
        let status = response.status();
        let body: serde_json::Value = response.json().await.expect("registration JSON");
        assert!(status.is_success(), "registration failed: {body}");
        body["id"].as_str().expect("deployment id").to_owned()
    }

    async fn usage(&self, mode: CleanupMode) -> super::cleanup::DeploymentUsageMap {
        self.query::<DeploymentUsageRows>(&deployment_usage_query(mode))
            .await
            .into_map()
    }

    async fn query<T: serde::de::DeserializeOwned>(&self, sql: &str) -> T {
        let deadline = Instant::now() + CONVERGE_TIMEOUT;
        loop {
            match self.try_query(sql).await {
                Ok(body) => {
                    return serde_json::from_str(&body).unwrap_or_else(|err| {
                        panic!("query response did not match controller type: {err}\n{body}")
                    });
                }
                Err(error) if is_partition_placement_error(&error) && Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                Err(error) => panic!("{error}\nquery: {sql}"),
            }
        }
    }

    async fn try_query(&self, sql: &str) -> Result<String, String> {
        let response = self
            .client
            .post(format!("{}/query", self.admin_url))
            .header(reqwest::header::ACCEPT, "application/json")
            .json(&json!({ "query": sql }))
            .send()
            .await
            .map_err(|err| err.to_string())?;
        let status = response.status();
        let body = response.text().await.map_err(|err| err.to_string())?;
        if status.is_success() {
            Ok(body)
        } else {
            Err(format!("query failed ({status}): {body}"))
        }
    }
}

fn is_partition_placement_error(error: &str) -> bool {
    error.contains("doesn't exist on this node")
        || error.contains("node lookup for partition")
        || error.contains("error sending request")
}

async fn await_ok<F, Fut>(what: &str, mut condition: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = Instant::now() + CONVERGE_TIMEOUT;
    while Instant::now() < deadline {
        if condition().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test]
#[ignore = "needs Docker; run `just test-e2e`"]
async fn legacy_query_tracks_safe_rollout_decisions() {
    let server = RestateServer::start(LEGACY_IMAGE);
    let admin = Admin {
        client: reqwest::Client::new(),
        admin_url: server.admin_url.clone(),
    };

    // The query engine is the readiness condition this test needs. It may serve the Admin port
    // before its partition is placed, so wait through only that documented transient here.
    let _: DeploymentUsageRows = admin
        .query(&deployment_usage_query(CleanupMode::Rollout))
        .await;

    let v1 = admin.register(&serve_greeter().await).await;
    let v2 = admin.register(&serve_greeter().await).await;
    let usage = admin.usage(CleanupMode::Rollout).await;
    assert!(!usage[&v1].is_active(CleanupMode::Rollout));
    assert!(usage[&v2].is_active(CleanupMode::Rollout));

    SLOW_STARTED.store(false, Ordering::Release);
    admin
        .client
        .post(format!("{}/Greeter/slow/send", server.ingress_url))
        .send()
        .await
        .expect("start durable slow invocation")
        .error_for_status()
        .expect("slow invocation accepted");
    await_ok("slow handler to start", || async {
        SLOW_STARTED.load(Ordering::Acquire)
    })
    .await;
    await_ok("v2 invocation to become pinned", || async {
        admin
            .usage(CleanupMode::Rollout)
            .await
            .get(&v2)
            .is_some_and(|usage| usage.pinned_invocations > 0)
    })
    .await;

    let v3 = admin.register(&serve_greeter().await).await;
    await_ok(
        "superseded pinned deployment to appear in usage query",
        || async {
            let usage = admin.usage(CleanupMode::Rollout).await;
            usage
                .get(&v2)
                .is_some_and(|usage| usage.is_active(CleanupMode::Rollout))
                && usage.get(&v3).is_some_and(|usage| usage.latest_for_service)
        },
    )
    .await;

    await_ok("v2 to drain", || async {
        admin
            .usage(CleanupMode::Rollout)
            .await
            .get(&v2)
            .is_some_and(|usage| !usage.is_active(CleanupMode::Rollout))
    })
    .await;
}
