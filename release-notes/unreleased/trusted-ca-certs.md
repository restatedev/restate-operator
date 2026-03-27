## Trusted CA Certificates

You can now configure custom trusted CA certificates for RestateCluster via `spec.security.trustedCaCerts`.
This is useful when Restate needs to trust internal CAs, for example when accessing an object store with a private certificate authority.

The operator adds an init container that concatenates the system CA bundle with your custom certificates into a single PEM file,
and sets `SSL_CERT_FILE` on the Restate container to point to the combined bundle. Changing the Secret references (name or key) triggers a pod rollout.

```yaml
spec:
  security:
    trustedCaCerts:
      - secretName: internal-ca
        key: ca.pem
```
