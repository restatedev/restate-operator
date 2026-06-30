# AGENTS.md

Instructions for AI coding agents working on this repository.

## Making CRD / Schema Changes

CRD types are Rust structs in `src/resources/*.rs`. Everything under `crd/` is generated from them.
Never hand-edit files in `crd/` -- always regenerate.

After changing a CRD struct, regenerate in this order (or run `just generate-all`):

1. `just generate` -- regenerates the CRD YAML (`crd/*.yaml`). This is the authoritative schema and the
   only artifact shipped to users (the Helm chart symlinks it). Pure Rust; no external tools. Always
   run and commit it.
2. `just generate-pkl` -- regenerates the Pkl bindings (`crd/*.pkl`), a convenience for Pkl users.
   Requires the `pkl` CLI on PATH; reads the CRD YAML, so run step 1 first.
3. `just generate-examples` -- regenerates `crd/examples/*.yaml` from the Pkl examples. Requires `pkl`.

### Pkl toolchain notes

- The `k8s.contrib.crd` generator package is pinned by version **and sha256** in each
  `crd/pklgen/generate-*.pkl`. Keep it pinned: generator versions differ in class naming and output
  layout, so an unpinned bump silently churns or breaks the committed bindings. To bump it, change the
  version in all three files, regenerate, and update the sha256 (pkl prints the expected checksum on a
  mismatch).
- The generators set `source = "./crd/<name>.yaml"` (a CWD-relative path; the recipe runs from the repo
  root). pkl rewrites this to an absolute `file://` URI in the generated header comment, so the recipe
  normalizes it back to the repo-relative form -- keep that `sed` step.
- Tested with pkl 0.31.1.

## Release Notes

When making changes, check whether the change warrants a release note by reviewing the guidelines in `release-notes/README.md`. If it does, create a release note file in `release-notes/unreleased/` as part of the same change.

## Trusted CA Certs Init Container

The trusted CA certs feature (`spec.security.trustedCaCerts`) uses an init container that reads the system CA bundle from `/etc/ssl/certs/ca-certificates.crt` (Debian/Alpine path). If the Restate server base image is changed to a different distro (e.g. RHEL uses `/etc/pki/tls/certs/ca-bundle.crt`), the `SYSTEM_CA_BUNDLE` constant in `src/controllers/restatecluster/reconcilers/compute.rs` must be updated.
