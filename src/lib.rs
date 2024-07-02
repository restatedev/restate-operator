use reconcilers::signing_key::InvalidSigningKeyError;
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
            Error::InvalidSigningKeyError(_) => "InvalidSigningKeyError",
        }
    }
}

/// Expose all controller components used by main
pub mod controller;

pub use crate::controller::*;

/// Log and trace integrations
pub mod telemetry;

/// Metrics
mod metrics;

pub use metrics::Metrics;

/// Reconcilers
mod reconcilers;

/// External CRDs
mod podidentityassociations;
mod secretproviderclasses;
mod securitygrouppolicies;
