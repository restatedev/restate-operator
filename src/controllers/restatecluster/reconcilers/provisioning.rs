use std::time::Duration;

use k8s_openapi::api::core::v1::{EnvVar, Pod};
use kube::Api;
use tonic::Code;
use tracing::{debug, info, warn};

use crate::Error;

// Include the generated protobuf code
pub mod node_ctl_svc {
    tonic::include_proto!("restate.node_ctl_svc");
}

/// Validates that Restate's auto-provision is disabled when using operator-managed provisioning.
///
/// The RESTATE_AUTO_PROVISION environment variable takes precedence over the config file setting.
/// This function checks:
/// 1. If RESTATE_AUTO_PROVISION env var is set to "false", validation passes
/// 2. If RESTATE_AUTO_PROVISION env var is set to any other value, validation fails
/// 3. If env var is not set, falls back to checking config TOML for `auto-provision = false`
///
/// This is required because when the operator is managing provisioning, the Restate
/// node should not try to auto-provision itself and the default is `true`.
pub fn validate_config_for_provisioning(
    config: Option<&str>,
    env: Option<&[EnvVar]>,
) -> Result<(), Error> {
    // Check for RESTATE_AUTO_PROVISION env var first (takes precedence)
    if let Some(env_vars) = env
        && let Some(env_var) = env_vars.iter().find(|e| e.name == "RESTATE_AUTO_PROVISION")
    {
        // Only consider direct value, not valueFrom references
        if let Some(value) = &env_var.value {
            return if value.eq_ignore_ascii_case("false") {
                Ok(())
            } else {
                Err(Error::InvalidRestateConfig(
                    "Cannot use cluster.autoProvision with RESTATE_AUTO_PROVISION != false. \
                     Disable cluster auto-provisioning by setting RESTATE_AUTO_PROVISION=false \
                     or removing the env var and setting auto-provision = false in config."
                        .into(),
                ))
            };
        }
        // If valueFrom is used, we can't validate statically - fall through to config check
    }

    // Fall back to config file check
    let config = match config {
        Some(config) => config,
        None => {
            return Err(Error::InvalidRestateConfig(
                "Cannot use cluster.autoProvision without auto-provision = false in config \
                 or RESTATE_AUTO_PROVISION=false env var. Disable cluster auto-provisioning \
                 by setting auto-provision = false in config or RESTATE_AUTO_PROVISION=false \
                 when using operator-managed provisioning."
                    .into(),
            ));
        }
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
            "Cannot use cluster.autoProvision without auto-provision = false in config \
             or RESTATE_AUTO_PROVISION=false env var. Disable cluster auto-provisioning \
             by setting auto-provision = false in config or RESTATE_AUTO_PROVISION=false \
             when using operator-managed provisioning."
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
pub async fn run_provisioning(namespace: &str, cluster_dns: &str) -> Result<(), Error> {
    use node_ctl_svc::ProvisionClusterRequest;
    use node_ctl_svc::node_ctl_svc_client::NodeCtlSvcClient;
    use tonic::transport::Channel;

    // Build the endpoint to the restate-0 pod via the headless service
    let endpoint = format!("http://restate-0.restate-cluster.{namespace}.svc.{cluster_dns}:5122");

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

    fn make_env_var(name: &str, value: &str) -> EnvVar {
        EnvVar {
            name: name.to_string(),
            value: Some(value.to_string()),
            value_from: None,
        }
    }

    #[test]
    fn test_validate_config_no_config_no_env() {
        let result = validate_config_for_provisioning(None, None);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            Error::InvalidRestateConfig(_)
        ));
    }

    #[test]
    fn test_validate_config_empty() {
        let result = validate_config_for_provisioning(Some(""), None);
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
        let result = validate_config_for_provisioning(Some(config), None);
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
        assert!(validate_config_for_provisioning(Some(config), None).is_ok());
    }

    #[test]
    fn test_validate_config_auto_provision_true() {
        let config = r#"
            auto-provision = true
        "#;
        let result = validate_config_for_provisioning(Some(config), None);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, Error::InvalidRestateConfig(_)));
    }

    #[test]
    fn test_validate_config_invalid_toml() {
        let config = "this is not valid [toml";
        let result = validate_config_for_provisioning(Some(config), None);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, Error::InvalidRestateConfig(_)));
    }

    // Tests for env var precedence

    #[test]
    fn test_env_var_false_overrides_missing_config() {
        let env = vec![make_env_var("RESTATE_AUTO_PROVISION", "false")];
        assert!(validate_config_for_provisioning(None, Some(&env)).is_ok());
    }

    #[test]
    fn test_env_var_false_overrides_config_true() {
        let config = r#"
            auto-provision = true
        "#;
        let env = vec![make_env_var("RESTATE_AUTO_PROVISION", "false")];
        assert!(validate_config_for_provisioning(Some(config), Some(&env)).is_ok());
    }

    #[test]
    fn test_env_var_false_case_insensitive() {
        let env = vec![make_env_var("RESTATE_AUTO_PROVISION", "FALSE")];
        assert!(validate_config_for_provisioning(None, Some(&env)).is_ok());

        let env = vec![make_env_var("RESTATE_AUTO_PROVISION", "False")];
        assert!(validate_config_for_provisioning(None, Some(&env)).is_ok());
    }

    #[test]
    fn test_env_var_true_fails() {
        let env = vec![make_env_var("RESTATE_AUTO_PROVISION", "true")];
        let result = validate_config_for_provisioning(None, Some(&env));
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            Error::InvalidRestateConfig(_)
        ));
    }

    #[test]
    fn test_env_var_true_overrides_config_false() {
        // Env var takes precedence, so even if config says false, env var true should fail
        let config = r#"
            auto-provision = false
        "#;
        let env = vec![make_env_var("RESTATE_AUTO_PROVISION", "true")];
        let result = validate_config_for_provisioning(Some(config), Some(&env));
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            Error::InvalidRestateConfig(_)
        ));
    }

    #[test]
    fn test_other_env_vars_ignored() {
        // Other env vars should not affect validation
        let config = r#"
            auto-provision = false
        "#;
        let env = vec![
            make_env_var("SOME_OTHER_VAR", "true"),
            make_env_var("RESTATE_CLUSTER_NAME", "test"),
        ];
        assert!(validate_config_for_provisioning(Some(config), Some(&env)).is_ok());
    }

    #[test]
    fn test_empty_env_list_falls_back_to_config() {
        let config = r#"
            auto-provision = false
        "#;
        let env: Vec<EnvVar> = vec![];
        assert!(validate_config_for_provisioning(Some(config), Some(&env)).is_ok());
    }

    #[test]
    fn test_env_var_with_value_from_falls_back_to_config() {
        // If env var uses valueFrom instead of value, fall back to config check
        let config = r#"
            auto-provision = false
        "#;
        let env = vec![EnvVar {
            name: "RESTATE_AUTO_PROVISION".to_string(),
            value: None,                          // No direct value
            value_from: Some(Default::default()), // Uses valueFrom
        }];
        assert!(validate_config_for_provisioning(Some(config), Some(&env)).is_ok());
    }
}
