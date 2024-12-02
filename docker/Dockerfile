FROM --platform=$BUILDPLATFORM ghcr.io/restatedev/dev-tools:latest AS planner
COPY .. .
RUN just chef-prepare

FROM --platform=$BUILDPLATFORM ghcr.io/restatedev/dev-tools:latest AS base
COPY --from=planner /restate/recipe.json recipe.json
COPY ../justfile justfile

# avoid sharing sccache port between multiplatform builds - they share a network but not a filesystem, so it won't work
FROM base AS base-amd64
ARG SCCACHE_SERVER_PORT=4226

FROM base AS base-arm64
ARG SCCACHE_SERVER_PORT=4227

FROM base-$TARGETARCH AS builder
ARG SCCACHE_SERVER_PORT
ARG TARGETARCH

ENV RUSTC_WRAPPER=/usr/bin/sccache
ENV SCCACHE_DIR=/var/cache/sccache
# Overrides the behaviour of the release profile re including debug symbols, which in our repo is not to include them.
# Should be set to 'false' or 'true'. See https://doc.rust-lang.org/cargo/reference/environment-variables.html
ARG CARGO_PROFILE_RELEASE_DEBUG=false
RUN just arch=$TARGETARCH libc=musl chef-cook --release --bin restate-operator
COPY .. .
RUN just arch=$TARGETARCH libc=musl build --release --bin restate-operator && mv target/$(just arch=$TARGETARCH libc=musl print-target)/release/restate-operator target/restate-operator

FROM gcr.io/distroless/static:nonroot
COPY --from=builder --chown=nonroot:nonroot /restate/target/restate-operator /app/restate-operator
ENTRYPOINT ["/app/restate-operator"]
