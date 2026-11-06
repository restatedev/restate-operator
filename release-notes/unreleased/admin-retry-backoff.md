# Release Notes: Back off RestateDeployment retries against the admin API

## Behavioral Change

### What Changed

- When a RestateDeployment's calls to the Restate admin API fail or are rejected, the
  operator now retries after 30 seconds at first and backs off to 5 minutes the longer
  the failure lasts. It used to retry every 30 seconds indefinitely. Other reconcile
  failures, such as a missing Secret, still retry after about 30 seconds.
- Retry intervals and the routine 5-minute re-check of healthy deployments are jittered
  by up to 20%, so deployments on the same Restate cluster no longer call its admin API
  in lockstep.
- Admin API requests now time out after 120 seconds (10 seconds to connect). Before,
  a request to an unresponsive admin API could hold its reconcile indefinitely.
- A ReplicaSet without a status now reports `ReplicaSetNoStatus` as the `Ready`
  condition's reason; the reason and message were swapped.

### Why This Matters

A Restate cluster that is struggling or unreachable was retried at a fixed rate by every
RestateDeployment pointing at it, which adds load at the moment the cluster can least
absorb it.

### Impact on Users

- A deployment that has been failing for a while takes up to about 6 minutes to notice
  that the admin API has recovered, unless a change to the resource or its pods triggers
  a reconcile sooner.
- Pods that are still starting are not treated as failures and keep their existing
  re-check interval.
- While a RestateDeployment is being deleted, failed admin API calls back off by how
  long the deletion has been running.
- No manifest, Helm value, or migration change is required.
