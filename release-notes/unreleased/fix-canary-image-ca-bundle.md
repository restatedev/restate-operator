# Release Notes: Fix canary image missing CA bundle

## Bug Fix

### What Changed
The default `canaryImage` has been changed from `busybox:uclibc` to `alpine:3.21`.

### Why This Matters
The `trustedCaCerts` feature uses an init container (the canary image) to concatenate
system CA certificates with custom trusted CAs. The init container reads the system CA
bundle from `/etc/ssl/certs/ca-certificates.crt`, but `busybox:uclibc` does not ship a
CA bundle at that path, causing the init container to fail with:

```
cat: can't open '/etc/ssl/certs/ca-certificates.crt': No such file or directory
```

This made `trustedCaCerts` non-functional with the default canary image.

### Impact on Users
- **Existing deployments using `trustedCaCerts`**: Will work after upgrading. If you
  previously worked around this by setting `canaryImage` to an image with a CA bundle,
  you can remove that override.
- **Existing deployments not using `trustedCaCerts`**: No impact. The canary image is
  also used for Pod Identity and Workload Identity canary jobs, which do not depend on
  the CA bundle and will continue to work with `alpine:3.21`.
- **Custom `canaryImage` overrides**: If you use a custom canary image, ensure it
  includes a CA bundle at `/etc/ssl/certs/ca-certificates.crt` if you plan to use
  `trustedCaCerts`.

### Migration Guidance
No action required. The default will change automatically on upgrade.

If you override `canaryImage` in your Helm values and want to use `trustedCaCerts`,
ensure your image includes a CA certificate bundle:

```yaml
# Image must have /etc/ssl/certs/ca-certificates.crt and provide cat, grep, wget
canaryImage: my-registry.example.com/alpine:3.21
```
