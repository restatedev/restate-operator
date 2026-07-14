# AGENTS.md

Instructions for AI coding agents working on this repository.

## Making CRD / Schema Changes

CRD types are Rust structs in `src/resources/*.rs`. Everything under `crd/` is generated from them.
Never hand-edit files in `crd/` -- always regenerate.

After changing a CRD struct, regenerate in this order (or run `just generate-all`):

1. `just generate` -- regenerates the CRD YAML (`crd/*.yaml`). This is the authoritative schema and the
   only artifact shipped to users. The `restate-operator-crds` subchart
   (`charts/restate-operator-crds/crds/`) symlinks these files into Helm's native `crds/` directory,
   and the operator Helm chart pulls that subchart in as an optional dependency (gated by
   `installCrds`). Pure Rust; no external tools. Always run and commit it.
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
- Use **pkl 0.31.x** (the committed bindings were generated with 0.31.1). The generator *package* is
  hash-pinned, but the pkl CLI is not, and other CLI versions can emit spurious diffs. The pinned
  version lives in the justfile as `pkl_version`, and `just generate-pkl` / `generate-examples` warn
  if your CLI differs.

## Release Notes

When making changes, check whether the change warrants a release note by reviewing the guidelines in `release-notes/README.md`. If it does, create a release note file in `release-notes/unreleased/` as part of the same change.

## Releasing

The release workflow (`.github/workflows/release.yml`) is tag-driven: pushing a
`v<version>` tag builds and publishes the docker image and helm chart under that
version. It bumps nothing. In the release commit you must therefore bump all three
version files together to the new version:

- `Cargo.toml`
- `Cargo.lock` (the `restate-operator` package entry — via `cargo check`, not by hand)
- `charts/restate-operator-helm/Chart.yaml` (the chart version *and* the `restate-operator-crds`
  dependency version)
- `charts/restate-operator-crds/Chart.yaml`

The CRDs ship as the optional `restate-operator-crds` subchart, vendored into the operator chart as a
committed `charts/restate-operator-helm/charts/restate-operator-crds-<version>.tgz` (the release pipeline
does not run `helm dependency build`, so the vendored tgz + `Chart.lock` are what make the published OCI
chart self-contained). After bumping the versions above — or any time `crd/*.yaml` changes the CRD schema —
re-vendor so the published chart carries the current CRDs:

```bash
helm dependency update charts/restate-operator-helm
```

and commit the updated `charts/restate-operator-helm/charts/restate-operator-crds-<version>.tgz` and
`charts/restate-operator-helm/Chart.lock`.

Then consolidate the `release-notes/unreleased/` files into `v<version>.md`. See
`release-notes/README.md` for the full, authoritative release process (bump → consolidate → delete → merge → tag).

## Trusted CA Certs Init Container

The trusted CA certs feature (`spec.security.trustedCaCerts`) uses an init container that reads the system CA bundle from `/etc/ssl/certs/ca-certificates.crt` (Debian/Alpine path). If the Restate server base image is changed to a different distro (e.g. RHEL uses `/etc/pki/tls/certs/ca-bundle.crt`), the `SYSTEM_CA_BUNDLE` constant in `src/controllers/restatecluster/reconcilers/compute.rs` must be updated.
