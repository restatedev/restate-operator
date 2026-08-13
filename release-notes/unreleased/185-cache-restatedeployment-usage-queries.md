# Release Notes for Issue #185: Share deployment-usage answers between RestateDeployments

## Behavioral Change

### What Changed

The deployment-usage query the operator runs before registering or removing a version is
now cached for 60 seconds, keyed on the Restate admin endpoint it was asked of.

The answer that query returns — which registered deployments are still an endpoint for
some service, and which still have unfinished invocations — describes the Restate
environment, not the RestateDeployment that happened to ask. Every resource registered
against the same endpoint gets the same map back. Previously each one fetched its own
copy, so a namespace holding N RestateDeployments paid N identical scans across every
partition on each rollout. Now the first reader pays for the query and the rest read the
answer it produced.

A cache hit is deliberately served ahead of the per-endpoint rate limit added alongside
this change. A hit sends no request, so there is nothing to rate limit, and making it wait
for a permit would leave in place the very queue the cache exists to remove — with queries
spaced 30 seconds apart, the last of N resources would otherwise wait N × 30 seconds to be
told what the first already knew.

Two flavours of the query exist, and they are not interchangeable. The flavour used during
deletion computes the count of unfinished-but-unpinned invocations for real, where the
rollout flavour selects a constant zero because a rollout reads that work through the
"latest for service" flag instead. So a deletion's answer may be reused by a rollout, but
never the other way around: doing so would report queued work as absent and allow a
deployment to be removed out from under it.

### Why This Matters

The query is the most expensive thing the operator asks of Restate, and it gets more
expensive as an environment accumulates invocations. It was also being multiplied by a
factor nobody chose — the number of RestateDeployments sharing an environment — at exactly
the moment the environment is busiest, since a rollout is what triggers the reconciles.

### Impact on Users

- Registration and cleanup decisions may now act on an answer up to 60 seconds old.
  Invocation counts fall as work drains, so a stale answer normally over-states how busy a
  deployment is, which defers a removal rather than bringing one forward. The operator's
  own writes are not left to expire: registering or deregistering a deployment immediately
  drops the cached answer for that endpoint, so the next reader re-asks.
- Cleanup of a large number of RestateDeployments sharing one Restate environment should
  complete substantially faster, because they no longer queue behind one another's queries.
- No manifest, Helm value, or migration change is required.

### Related Issues

- Issue #185: Use vqueues for pinned RestateDeployment accounting when capability is proven
