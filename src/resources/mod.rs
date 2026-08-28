pub mod iampolicymembers;
pub mod knative;
pub mod podidentityassociations;
pub mod restatecloudenvironments;
pub mod restateclusters;
pub mod restatedeployments;
pub mod secretproviderclasses;
pub mod securitygrouppolicies;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// What the operator is doing with a resource, written on every reconcile.
//
// Not a doc comment: give the type a description and schemars renders `Option<T>` as an
// `anyOf`, which the API server won't accept. Each status field documents itself instead.
#[derive(Deserialize, Serialize, Clone, Copy, Debug, PartialEq, Eq, JsonSchema)]
pub enum ReconciliationState {
    /// The operator is managing this resource normally.
    Reconciling,
    /// Suspended by the `restate.dev/reconcile: disabled` annotation. The operator won't
    /// change this resource or anything it owns, though deleting it still works normally.
    /// The rest of the status is whatever the last real reconcile left behind.
    Disabled,
    /// The annotation is gone, but the resource isn't ready again yet. Anything changed by
    /// hand during the suspension is being undone. Worth a look if it stays here.
    ResumingReconciliation,
}

impl ReconciliationState {
    /// Every state, in the order they appear in the CRD.
    pub const ALL: [Self; 3] = [
        Self::Reconciling,
        Self::Disabled,
        Self::ResumingReconciliation,
    ];

    /// Used for the status field and for the `Reconciling` condition's reason, so the two
    /// always agree.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reconciling => "Reconciling",
            Self::Disabled => "Disabled",
            Self::ResumingReconciliation => "ResumingReconciliation",
        }
    }
}

/// Schema for a `.status.reconciliation` field.
///
/// Written by hand: the status structs derive plain `JsonSchema`, which renders
/// `Option<ReconciliationState>` as an `anyOf` the API server won't accept. Values come from
/// [`ReconciliationState::ALL`], so this can't drift from the enum.
pub fn reconciliation_schema(_g: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "string",
        "enum": ReconciliationState::ALL.map(ReconciliationState::as_str).to_vec(),
        "nullable": true,
    })
}

#[cfg(test)]
mod tests {
    use super::ReconciliationState;

    /// The CRD's enum comes from `as_str`, but the status is written through serde. If those
    /// two ever disagreed, every status write would start failing validation.
    #[test]
    fn the_schema_values_are_the_ones_serde_writes() {
        for state in ReconciliationState::ALL {
            assert_eq!(
                serde_json::to_value(state).expect("state serializes"),
                serde_json::Value::String(state.as_str().to_owned()),
            );
        }
    }
}
