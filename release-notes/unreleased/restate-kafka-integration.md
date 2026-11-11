# Release Notes: `RestateKafkaIntegration` CRD

## New Feature

### What Changed

A fourth CRD, `RestateKafkaIntegration` (`restate.dev/v1alpha1`, short name `rki`), runs the
[Restate Kafka ingress integration](https://github.com/restatedev/ingress-integration-kafka) --
a container that consumes Kafka topics and turns each record into a Restate invocation. The
operator manages an `apps/v1` `Deployment` of it in the custom resource's own namespace, and a
`ConfigMap` holding the resolved ingress URL plus any inline configuration.

```yaml
apiVersion: restate.dev/v1alpha1
kind: RestateKafkaIntegration
metadata:
  name: orders-to-restate
spec:
  replicas: 2
  restate:
    ingress:
      cluster: restate
  config: |
    bootstrap.servers=kafka.kafka.svc.cluster.local:9092
    group.id=orders-to-restate
    topics=orders
    restate.record.mapper.service=OrderService
    restate.record.mapper.handler=onKafkaEvent
```

Only the Restate destination is modelled as fields:

- `spec.restate.ingress` is the same `cluster` / `cloud` / `service` / `url` union as
  `RestateDeployment`'s `spec.restate.register`, resolved to the **ingress** port (8080) rather
  than the admin port. A `RestateCluster` named `X` resolves to
  `http://restate.X.svc.<clusterDns>:8080`.
- `spec.restate.authToken` is a `{name, key}` reference to a Secret **in the custom resource's
  own namespace**, passed to the container as the `RESTATE_AUTH_TOKEN` environment variable. It
  is required with `ingress.cloud`, and optional otherwise. The token stays in the Secret and is
  never written into a `.properties` file.

Everything Kafka-side is the container's own configuration surface, given as `spec.config` (an
inline `.properties` block) plus `spec.configRefs` (a list of `secretRef` / `configMapRef`
sources). The container merges them -- the operator's own resolved `restate.ingress.url` first,
then `spec.config`, then each `spec.configRefs` entry in order -- with a later source winning on
shared keys. So you can mix a plain-text inline base with a `secretRef` overlay for credentials,
and override the ingress URL if you must. `spec.template` is a partial pod template
strategic-merged over the generated one, for resources, probes, sidecars, node placement and the
like.

### Why This Matters

Running the integration by hand meant writing a `Deployment`, knowing the cluster's in-cluster
ingress URL, and keeping the image tag in sync. This makes it a declarative resource that
tracks the cluster it points at.

Deliberately *not* mirroring the container's configuration options as CRD fields means a new
upstream option is usable the day it ships, instead of waiting for an operator release.
`spec.config` accepting property-key spelling (`auto.offset.reset`, not
`KAFKA_AUTO_OFFSET_RESET`) also means the upstream documentation applies unchanged.

### Impact on Users

- **Existing deployments**: no change. Nothing reconciles differently, and the new controller
  simply has nothing to do until a `RestateKafkaIntegration` exists.
- **New deployments**: the CRD ships in the `restate-operator-crds` chart alongside the other
  three, and the operator's readiness now also waits for it -- so `/ready` reports
  `RestateKafkaIntegration` as pending until the CRD is installed. Users who manage CRDs
  out-of-band must apply the new one, or the operator will stay `NotReady`.
- **RBAC**: the operator now needs cluster-wide access to `apps/deployments`
  (get/list/watch/create/patch/delete), because these children land in the user's namespace
  rather than the operator's. The owned `ConfigMap` is covered by the existing cluster-wide
  `configmaps` get/list/watch/create/patch grant (it is torn down by garbage collection, never
  deleted by the operator). `helm upgrade` applies this; a hand-managed ClusterRole needs
  updating.

### Migration Guidance

- Reinstall or upgrade the CRDs (`helm upgrade` with `installCrds: true`, or apply
  `crd/restatekafkaintegrations.yaml` / the `restate-operator-crds` chart).
- `helm upgrade` the operator chart to pick up the ClusterRole change.
- Optionally set the `kafkaIntegrationImage` Helm value to pin the default image
  (`OPERATOR_KAFKA_INTEGRATION_DEFAULT_IMAGE` / `--kafka-integration-default-image`); it
  currently defaults to `ghcr.io/restatedev/ingress-integration-kafka:latest`, since upstream
  has published no version tags yet.

### Known Gotcha

A `RestateCluster` denies all inbound traffic to its ingress port unless
`spec.security.networkPeers.ingress` names a peer, and the integration dials *in* to that
port. Without a peer it will never connect, and the failure presents as a hang. The operator
labels the integration's pods `app.kubernetes.io/name: restate-kafka-integration` and
`allow.restate.dev/<cluster-name>: "true"`; the former is what an ingress peer should select
(the latter governs the cluster's egress). See `examples/kafka` and the README section.
