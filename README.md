# Restate Operator

## Installing
```bash
helm install restate-operator oci://ghcr.io/restatedev/restate-operator-helm --namespace restate-operator --create-namespace
```

## Releasing
1. Update the app version in charts/restate-operator/Chart.yaml and the version in Cargo.toml eg to `0.0.2`
2. Push a new tag `v0.0.2`
3. Accept the draft release once the workflow finishes
