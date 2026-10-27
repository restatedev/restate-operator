# Fix BYOC in-process service registration

## Bug Fix

### What Changed

In-process deployments in BYOC regions register through
`https://proxy.<region>.restate.cloud:9082` instead of the tunnel listener on port
9080. The environment, revision-specific tunnel name, and service path are
preserved. Hosted regions and external tunnel mode retain their existing URLs.

### Why This Matters

A pod can establish its outbound tunnel while registration still times out
against the wrong listener. The service then remains unavailable at ingress,
which returns a service-not-found response.

### Impact on Users

BYOC deployments using `tunnelMode: in-process` retry registration through the
proxy endpoint after the operator is upgraded. No manifest changes are required.

### Migration Guidance

Upgrade the operator, wait for the affected RestateDeployment to become ready,
and retry requests that previously failed because the service was unregistered.
