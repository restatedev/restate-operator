# Kafka ingress integration example

Runs [`ingress-integration-kafka`](https://github.com/restatedev/ingress-integration-kafka)
against a `RestateCluster` with a `RestateKafkaIntegration`, so records on a Kafka topic
become Restate invocations.

`kafka.yaml` is a throwaway single-broker KRaft Kafka for trying this out -- it is not a
production Kafka deployment.

```bash
kubectl apply --server-side -f kafka.yaml
kubectl apply --server-side -f cluster.yaml
kubectl apply --server-side -f integration.yaml

kubectl get rki -o wide
kubectl logs -l app.kubernetes.io/name=restate-kafka-integration
```

## The NetworkPolicy bit

A `RestateCluster` denies all inbound traffic to its ingress port (8080) unless
`spec.security.networkPeers.ingress` names a peer. The integration dials *in* to that port,
so without a peer it will simply never connect.

`cluster.yaml` shows the peer to add: it selects pods labelled
`app.kubernetes.io/name: restate-kafka-integration` in any namespace. The operator puts that
label on the pods it creates, along with `allow.restate.dev/<cluster-name>` (which is what
lets the cluster's *egress* policy reach back out).

If your cluster sets `spec.security.disableNetworkPolicies: true`, none of this applies.
