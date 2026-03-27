# AGENTS.md

Instructions for AI coding agents working on this repository.

## Release Notes

When making changes, check whether the change warrants a release note by reviewing the guidelines in `release-notes/README.md`. If it does, create a release note file in `release-notes/unreleased/` as part of the same change.

## Trusted CA Certs Init Container

The trusted CA certs feature (`spec.security.trustedCaCerts`) uses an init container that reads the system CA bundle from `/etc/ssl/certs/ca-certificates.crt` (Debian/Alpine path). If the Restate server base image is changed to a different distro (e.g. RHEL uses `/etc/pki/tls/certs/ca-bundle.crt`), the `SYSTEM_CA_BUNDLE` constant in `src/controllers/restatecluster/reconcilers/compute.rs` must be updated.
