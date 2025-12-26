use std::time::Duration;

use k8s_openapi::api::core::v1::Pod;
use kube::Api;
use tonic::Code;
use tracing::{debug, info, warn};

use crate::Error;

// Include the generated protobuf code
pub mod node_ctl_svc {
    tonic::include_proto!("restate.node_ctl_svc");
}

/// Validates that the config TOML has `auto-provision = false` set.
/// This is required because when the operator is managing provisioning, the Restate
/// node should not try to auto-provision itself and the default is `true`.
pub fn validate_config_for_provisioning(config: Option<&str>) -> Result<(), Error> {
    let config =
        match config {
            Some(config) => config,
            None => return Err(Error::InvalidRestateConfig(
                "Cannot use clusterProvisioning.enabled without auto-provision = false in config. \
             Disable cluster auto-provisioning by setting auto-provision = false when using \
             operator-managed provisioning."
                    .into(),
            )),
        };

    // Parse the TOML config
    let parsed: toml::Value = match toml::from_str(config) {
        Ok(v) => v,
        Err(e) => {
            return Err(Error::InvalidRestateConfig(format!(
                "Failed to parse config TOML: {e}"
            )));
        }
    };

    // Check for top-level auto-provision = false
    if let Some(auto_provision) = parsed.get("auto-provision")
        && auto_provision.as_bool() == Some(false)
    {
        Ok(())
    } else {
        Err(Error::InvalidRestateConfig(
            "Cannot use clusterProvisioning.enabled without auto-provision = false in config. \
             Disable cluster auto-provisioning by setting auto-provision = false when using \
             operator-managed provisioning."
                .into(),
        ))
    }
}

/// Checks if the specified pod is in Running phase.
/// Returns true if the pod exists and is Running, false otherwise.
pub async fn is_pod_running(pod_api: &Api<Pod>, pod_name: &str) -> Result<bool, Error> {
    match pod_api.get_opt(pod_name).await? {
        Some(pod) => {
            let phase = pod.status.as_ref().and_then(|s| s.phase.as_deref());
            let is_running = phase == Some("Running");
            debug!(
                pod = pod_name,
                phase = phase.unwrap_or("unknown"),
                is_running,
                "Checked pod status"
            );
            Ok(is_running)
        }
        None => {
            debug!(pod = pod_name, "Pod does not exist yet");
            Ok(false)
        }
    }
}

/// Runs the gRPC provisioning call against the restate-0 pod.
/// Returns Ok(()) if provisioning succeeded or if the cluster was already provisioned.
pub async fn run_provisioning(namespace: &str) -> Result<(), Error> {
    use node_ctl_svc::ProvisionClusterRequest;
    use node_ctl_svc::node_ctl_svc_client::NodeCtlSvcClient;
    use tonic::transport::Channel;

    // Build the endpoint to the restate-0 pod via the headless service
    let endpoint = format!("http://restate-0.restate-cluster.{namespace}.svc.cluster.local:5122");

    info!(endpoint = %endpoint, "Connecting to Restate node for provisioning");

    // Create the gRPC channel
    let channel = Channel::from_shared(endpoint.clone())
        .map_err(|e| Error::ProvisioningFailed(format!("Invalid endpoint URL: {e}")))?
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .connect()
        .await
        .map_err(|e| Error::ProvisioningFailed(format!("Failed to connect to {endpoint}: {e}")))?;

    // Create the client
    let mut client = NodeCtlSvcClient::new(channel);

    // Create the provisioning request with default values
    let request = ProvisionClusterRequest {
        dry_run: false,
        num_partitions: None,
        partition_replication: None,
        log_provider: None,
        log_replication: None,
        target_nodeset_size: None,
    };

    info!("Calling ProvisionCluster RPC");

    match client.provision_cluster(request).await {
        Ok(response) => {
            let response = response.into_inner();
            if response.dry_run {
                warn!("Unexpected dry_run response from ProvisionCluster");
            }
            info!("Cluster provisioned successfully");
            Ok(())
        }
        Err(status) if status.code() == Code::AlreadyExists => {
            info!("Cluster was already provisioned");
            Ok(())
        }
        Err(status) => {
            warn!(
                code = ?status.code(),
                message = %status.message(),
                "ProvisionCluster RPC failed"
            );
            Err(Error::ProvisioningFailed(format!(
                "ProvisionCluster RPC failed: {} ({})",
                status.message(),
                status.code()
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_config_no_config() {
        let result = validate_config_for_provisioning(None);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            Error::InvalidRestateConfig(_)
        ));
    }

    #[test]
    fn test_validate_config_empty() {
        let result = validate_config_for_provisioning(Some(""));
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            Error::InvalidRestateConfig(_)
        ));
    }

    #[test]
    fn test_validate_config_no_auto_provision() {
        let config = r#"
            [server]
            bind-address = "0.0.0.0:8080"
        "#;
        let result = validate_config_for_provisioning(Some(config));
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            Error::InvalidRestateConfig(_)
        ));
    }

    #[test]
    fn test_validate_config_auto_provision_false() {
        let config = r#"
            auto-provision = false
        "#;
        assert!(validate_config_for_provisioning(Some(config)).is_ok());
    }

    #[test]
    fn test_validate_config_auto_provision_true() {
        let config = r#"
            auto-provision = true
        "#;
        let result = validate_config_for_provisioning(Some(config));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, Error::InvalidRestateConfig(_)));
    }

    #[test]
    fn test_validate_config_invalid_toml() {
        let config = "this is not valid [toml";
        let result = validate_config_for_provisioning(Some(config));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, Error::InvalidRestateConfig(_)));
    }
}
