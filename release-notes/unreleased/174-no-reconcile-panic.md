# Release Notes for #174 follow-up: no panic on the Knative reconcile path

## Bug Fix

### What Changed

`RegistrationAction::AlreadyLatest` now carries the deployment id it was decided from, so the
Knative reconciler reads it out of the verdict instead of unwrapping the `Option` it had just
passed in.

The planner reaches `AlreadyLatest` only after matching a recorded id against Restate's usage
map, so the id is necessarily present — but that reasoning lived in an `.expect()` at the call
site rather than in the type:

```rust
let recorded_id = recorded_id.cloned().expect("AlreadyLatest implies an id");
```

The invariant held, so this was not reachable in practice. It was still a panic sited on the
reconcile path, where the cost of being wrong is not an error condition but the controller
task unwinding, and it was only reachable there because the id had to be recovered separately
from the decision that depended on it. Moving the id into the variant makes the bad state
unrepresentable and removes the assertion entirely.

The ReplicaSet path compared against the variant rather than destructuring it, so it changes
to `matches!` and is otherwise untouched.

### Why This Matters

A `RestateDeployment` in the steady state — already latest, nothing to tell Restate — is the
most frequently taken branch in the Knative reconciler. An assertion there is the one least
likely to be exercised by anything but production.

### Impact on Users

None. No behaviour change, no CRD change, and no new status field or annotation: the panic was
unreachable, and this removes the possibility rather than a symptom.

### Related Issues

- Issue #174: Support rolling RestateDeployments back to a previously registered revision
- Review feedback on #178 (`reconcilers/knative.rs`): "i think we shouldn't panic here, it's on
  the reconcile path"
