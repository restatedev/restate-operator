use std::{collections::BTreeMap, path::PathBuf};

use k8s_openapi::api::core::v1::{CSIVolumeSource, KeyToPath, SecretVolumeSource, Volume};
use kube::{
    Api,
    api::{DeleteParams, ObjectMeta, Patch, PatchParams},
};
use tracing::{debug, warn};

use crate::{
    Error,
    controllers::restatecluster::controller::Context,
    resources::{
        restateclusters::{
            RequestSigningPrivateKey, SecretProviderSigningKeySource, SecretSigningKeySource,
        },
        secretproviderclasses::{SecretProviderClass, SecretProviderClassSpec},
    },
};

use super::object_meta;

const SECRET_PROVIDER_CLASS_NAME: &str = "request-signing-key-v1";

#[derive(thiserror::Error, Debug)]
pub enum InvalidSigningKeyError {
    #[error("Invalid signing protocol version; only 'v1' is supported")]
    InvalidVersion,
    #[error(
        "Multiple sources provided for signing private key; only one of 'secret', 'secretProvider' can be provided"
    )]
    MultipleSourcesProvided,
}

pub async fn reconcile_signing_key(
    ctx: &Context,
    namespace: &str,
    base_metadata: &ObjectMeta,
    private_key: Option<&RequestSigningPrivateKey>,
) -> Result<Option<(Volume, PathBuf)>, Error> {
    let spc_api: Api<SecretProviderClass> = Api::namespaced(ctx.client.clone(), namespace);

    let private_key = if let Some(private_key) = private_key {
        private_key
    } else {
        // No private key configuration, clean up
        remove_secret_provider_class(ctx, namespace, &spc_api).await?;
        return Ok(None);
    };

    match private_key.version.as_str() {
        "v1" => {}
        _ => return Err(InvalidSigningKeyError::InvalidVersion.into()),
    }

    match (
        private_key.secret.as_ref(),
        private_key.secret_provider.as_ref(),
    ) {
        (Some(secret), None) => {
            remove_secret_provider_class(ctx, namespace, &spc_api).await?;

            Ok(Some(reconcile_signing_key_secret(secret)))
        }
        (None, Some(secret_provider)) => {
            if !ctx.manage_secret_provider_classes {
                // Unlike a missing CRD, this is an explicit opt-out, so fail rather than quietly
                // rolling the cluster out without the signing key it asked for.
                return Err(Error::NotReady {
                    message: "secretProvider signing requires SecretProviderClass management; enable it or use a Kubernetes Secret signing source".into(),
                    reason: "SecretProviderClassesDisabled".into(),
                    requeue_after: None,
                });
            }
            if ctx.secret_provider_class_installed {
                Ok(Some(
                    reconcile_signing_key_secret_provider(
                        namespace,
                        base_metadata,
                        secret_provider,
                        &spc_api,
                    )
                    .await?,
                ))
            } else {
                warn!(
                    "Ignoring secret provider signing key source as the SecretProviderClass CRD is not installed"
                );
                Ok(None)
            }
        }
        (Some(_), Some(_)) => Err(InvalidSigningKeyError::MultipleSourcesProvided.into()),
        (None, None) => {
            // No private key configuration, clean up
            remove_secret_provider_class(ctx, namespace, &spc_api).await?;
            Ok(None)
        }
    }
}

pub fn reconcile_signing_key_secret(secret: &SecretSigningKeySource) -> (Volume, PathBuf) {
    let path = "private.pem";
    (
        Volume {
            name: "request-signing-private-key-secret".into(),
            secret: Some(SecretVolumeSource {
                secret_name: Some(secret.secret_name.clone()),
                items: Some(vec![KeyToPath {
                    key: secret.key.clone(),
                    path: path.into(),
                    mode: Some(0o400),
                }]),
                ..Default::default()
            }),
            ..Default::default()
        },
        path.into(),
    )
}

pub async fn reconcile_signing_key_secret_provider(
    namespace: &str,
    base_metadata: &ObjectMeta,
    secret_provider: &SecretProviderSigningKeySource,
    spc_api: &Api<SecretProviderClass>,
) -> Result<(Volume, PathBuf), Error> {
    let spc = SecretProviderClass {
        metadata: object_meta(base_metadata, SECRET_PROVIDER_CLASS_NAME),
        spec: SecretProviderClassSpec {
            parameters: secret_provider.parameters.clone(),
            provider: secret_provider.provider.clone(),
            secret_objects: None,
        },
    };

    let params: PatchParams = PatchParams::apply("restate-operator").force();
    debug!(
        "Applying SecretProviderClass {} in namespace {}",
        SECRET_PROVIDER_CLASS_NAME, namespace
    );
    spc_api
        .patch(SECRET_PROVIDER_CLASS_NAME, &params, &Patch::Apply(&spc))
        .await?;

    Ok((
        Volume {
            name: "request-signing-private-key-secret-provider".into(),
            csi: Some(CSIVolumeSource {
                driver: "secrets-store.csi.k8s.io".into(),
                read_only: Some(true),
                volume_attributes: Some(BTreeMap::from([(
                    "secretProviderClass".into(),
                    SECRET_PROVIDER_CLASS_NAME.into(),
                )])),
                ..Default::default()
            }),
            ..Default::default()
        },
        secret_provider.path.clone(),
    ))
}

pub async fn remove_secret_provider_class(
    ctx: &Context,
    namespace: &str,
    spc_api: &Api<SecretProviderClass>,
) -> Result<(), Error> {
    if !ctx.manage_secret_provider_classes || !ctx.secret_provider_class_installed {
        return Ok(());
    }
    debug!(
        "Ensuring SecretProviderClass {} in namespace {} does not exist",
        SECRET_PROVIDER_CLASS_NAME, namespace
    );
    match spc_api
        .delete(SECRET_PROVIDER_CLASS_NAME, &DeleteParams::default())
        .await
    {
        Err(kube::Error::Api(kube::error::ErrorResponse { code: 404, .. })) => Ok(()),
        Err(err) => Err(err.into()),
        Ok(_) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use kube::runtime::reflector;

    use super::*;
    use crate::Metrics;
    use crate::controllers::State;

    /// A context whose apiserver fails every request, along with how many it received. The
    /// SecretProviderClass CRD counts as installed, so only the switches keep requests away.
    fn context_with_failing_client(
        manage_network_policies: bool,
        manage_secret_provider_classes: bool,
    ) -> (Arc<Context>, Arc<AtomicUsize>) {
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = requests.clone();
        let service = tower::service_fn(move |_: http::Request<kube::client::Body>| {
            counter.fetch_add(1, Ordering::SeqCst);
            async {
                Err::<http::Response<kube::client::Body>, _>(std::io::Error::other("no apiserver"))
            }
        });
        let state = State::new(
            None,
            false,
            manage_network_policies,
            manage_secret_provider_classes,
            "restate-operator".into(),
            None,
            None,
            "tunnel:latest".into(),
            "cluster.local".into(),
            "alpine:3.21".into(),
            None,
            None,
        );
        let ctx = Context::new(
            kube::Client::new(service, "default"),
            Metrics::default(),
            state,
            reflector::store().0,
            reflector::store().0,
            false,
            true,
        );
        (ctx, requests)
    }

    #[tokio::test]
    async fn unmanaged_secret_provider_classes_are_left_alone() {
        let (ctx, requests) = context_with_failing_client(true, false);
        let base_metadata = ObjectMeta::default();

        let none = reconcile_signing_key(&ctx, "test", &base_metadata, None).await;
        assert!(none.unwrap().is_none());

        let secret = RequestSigningPrivateKey {
            version: "v1".into(),
            secret: Some(SecretSigningKeySource {
                key: "private.pem".into(),
                secret_name: "signing-key".into(),
            }),
            secret_provider: None,
        };
        let secret = reconcile_signing_key(&ctx, "test", &base_metadata, Some(&secret)).await;
        assert!(secret.unwrap().unwrap().0.secret.is_some());

        let secret_provider = RequestSigningPrivateKey {
            version: "v1".into(),
            secret: None,
            secret_provider: Some(SecretProviderSigningKeySource {
                path: "private.pem".into(),
                ..Default::default()
            }),
        };
        let secret_provider =
            reconcile_signing_key(&ctx, "test", &base_metadata, Some(&secret_provider)).await;
        assert!(matches!(
            secret_provider,
            Err(Error::NotReady { reason, .. }) if reason == "SecretProviderClassesDisabled"
        ));

        assert_eq!(requests.load(Ordering::SeqCst), 0);
    }
}
