use controllers::restatecluster::InvalidSigningKeyError;
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

    #[error("A namespace cannot be created for this name as one already exists")]
    NameConflict,

    #[error("Cluster is not yet Ready: {message}")]
    NotReady {
        message: String,
        reason: String,
        requeue_after: Option<Duration>,
    },

    #[error("Service is not yet Ready: {message}")]
    ServiceNotReady {
        message: String,
        reason: String,
        requeue_after: Option<Duration>,
    },

    #[error("Invalid Restate configuration: {0}")]
    InvalidRestateConfig(String),

    #[error(transparent)]
    InvalidSigningKeyError(#[from] InvalidSigningKeyError),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    pub fn metric_label(&self) -> &'static str {
        match self {
            Error::SerializationError(_) => "SerializationError",
            Error::KubeError(_) => "KubeError",
            Error::FinalizerError(_) => "FinalizerError",
            Error::NameConflict => "NameConflict",
            Error::NotReady { .. } => "NotReady",
            Error::ServiceNotReady { .. } => "ServiceNotReady",
            Error::InvalidRestateConfig(_) => "InvalidRestateConfig",
            Error::InvalidSigningKeyError(_) => "InvalidSigningKeyError",
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
