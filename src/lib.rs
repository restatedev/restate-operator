use controllers::restatecluster::InvalidSigningKeyError;
use k8s_openapi::api::apps::v1::ReplicaSetStatus;
use std::time::Duration;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("SerializationError: {0}")]
    SerializationError(#[from] serde_json::Error),

    #[error("Kube Error: {0}")]
    KubeError(#[from] kube::Error),

    #[error("Finalizer Error: {0}")]
    // NB: awkward type because finalizer::Error embeds the reconciler error (which is this)
    // so boxing this error to break cycles
    FinalizerError(#[from] Box<kube::runtime::finalizer::Error<Error>>),

    #[error("Cluster is not yet Ready: {message}")]
    NotReady {
        message: String,
        reason: String,
        requeue_after: Option<Duration>,
    },

    #[error("RestateDeployment is not yet Ready: {message}")]
    DeploymentNotReady {
        message: String,
        reason: String,
        requeue_after: Option<Duration>,
        replica_set_status: Option<Box<ReplicaSetStatus>>,
    },

    #[error("Invalid Restate configuration: {0}")]
    InvalidRestateConfig(String),

    #[error("The RestateCloudEnvironment {0} does not exist")]
    RestateCloudEnvironmentNotFound(String),

    #[error("The Secret {0} does not exist")]
    SecretNotFound(String),
    #[error("The Secret key {0} in {1} does not exist")]
    SecretKeyNotFound(String, String),

    #[error("The bearer token is invalid")]
    InvalidBearerToken,

    #[error(transparent)]
    InvalidSigningKeyError(#[from] InvalidSigningKeyError),

    #[error("Failed to make Restate admin API call: {0}")]
    AdminCallFailed(reqwest::Error),

    #[error("Encountered a ReplicaSet hash collision, will retry with a new template hash")]
    HashCollision,

    #[error(
        "This RestateDeployment is backing active versions in Restate. If you want to delete the RestateDeployment, either register new endpoints for the relevant services or delete the Restate versions."
    )]
    DeploymentInUse,

    #[error(
        "This RestateDeployment is backing recently-active versions in Restate. It will be removed after the drain delay period."
    )]
    DeploymentDraining { requeue_after: Option<Duration> },

    #[error(transparent)]
    InvalidUrl(#[from] url::ParseError),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    pub fn metric_label(&self) -> &'static str {
        match self {
            Error::SerializationError(_) => "SerializationError",
            Error::KubeError(_) => "KubeError",
            Error::FinalizerError(_) => "FinalizerError",
            Error::NotReady { .. } => "NotReady",
            Error::DeploymentNotReady { .. } => "ServiceNotReady",
            Error::InvalidRestateConfig(_) => "InvalidRestateConfig",
            Error::RestateCloudEnvironmentNotFound(_) => "RestateCloudEnvironmentNotFound",
            Error::SecretNotFound(_) => "SecretNotFound",
            Error::SecretKeyNotFound(_, _) => "SecretKeyNotFound",
            Error::InvalidBearerToken => "InvalidBearerToken",
            Error::InvalidSigningKeyError(_) => "InvalidSigningKeyError",
            Error::AdminCallFailed(_) => "AdminCallFailed",
            Error::HashCollision => "HashCollision",
            Error::DeploymentInUse => "DeploymentInUse",
            Error::DeploymentDraining { .. } => "DeploymentDraining",
            Error::InvalidUrl { .. } => "InvalidUrl",
        }
    }
}

pub mod controllers;

/// Log and trace integrations
pub mod telemetry;

/// Metrics
mod metrics;

pub use metrics::Metrics;

/// External CRDs
pub mod resources;
