use std::{collections::BTreeMap, path::PathBuf};

use k8s_openapi::api::core::v1::{CSIVolumeSource, KeyToPath, SecretVolumeSource, Volume};
use kube::{
    api::{DeleteParams, ObjectMeta, Patch, PatchParams},
    Api,
};
use tracing::{debug, warn};

use crate::{
    secretproviderclasses::{SecretProviderClass, SecretProviderClassSpec},
    Context, Error, RequestSigningPrivateKey, SecretProviderSigningKeySource,
    SecretSigningKeySource,
};

use super::object_meta;

const SECRET_PROVIDER_CLASS_NAME: &str = "request-signing-key-v1";

#[derive(thiserror::Error, Debug)]
pub enum InvalidSigningKeyError {
    #[error("Invalid signing protocol version; only 'v1' is supported")]
    InvalidVersion,
    #[error("Multiple sources provided for signing private key; only one of 'secret', 'secretProvider' can be provided")]
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
                warn!("Ignoring secret provider signing key source as the SecretProviderClass CRD is not installed");
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
    if !ctx.secret_provider_class_installed {
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
