# Knative Deployment Examples

This directory contains examples demonstrating the Knative deployment mode for Restate services.

## Prerequisites

- A Kubernetes cluster with Knative Serving installed.
- The Restate Operator installed.
- A Restate Cluster running (referenced as `cluster: restate`).

## Overview

The examples use the `greeter` service to demonstrate three key deployment patterns:

1.  **In-Place Updates** (`knative-v1.yaml`)
2.  **Versioned Updates** (`knative-v2.yaml`)
3.  **Auto-Versioning** (`knative-auto.yaml`)

## 1. In-Place Updates (`knative-v1.yaml`)

This manifest uses an **explicit tag** (`v1`) to create a stable deployment identity.

*   **Behavior**:
    *   Creates a Knative Configuration and Route named `greeter-v1`.
    *   Registers a Restate deployment with a stable ID.
    *   **Updates**: Changing the image or env vars (while keeping `tag: "v1"`) triggers an in-place update. Knative creates a new Revision (e.g., `greeter-v1-00002`) and gradually shifts traffic to it. The Restate deployment ID remains the same.
*   **Scale-to-Zero**: Enabled (`minScale: 0`).
*   **Poison/Antidote Pattern**:
    *   The service has a "poison" check. Sending `"poison"` to the greeting endpoint will return a 418 error.
    *   To fix this *in-place*, update the manifest to set the `ANTIDOTE` env var to `"cure"` and re-apply. The deployment ID persists, but the behavior changes.

## 2. Versioned Updates (`knative-v2.yaml`)

This manifest demonstrates a **versioned update** by changing the tag to `v2`.

*   **Behavior**:
    *   Creates a *new* Knative Configuration and Route named `greeter-v2`.
    *   Registers a *new* Restate deployment with a new ID.
    *   The old `v1` deployment (and its Configuration) is kept until all its invocations are complete (subject to `revisionHistoryLimit`).
*   **Use Case**: When you want to run multiple versions of a service simultaneously during a migration.

## 3. Auto-Versioning (`knative-auto.yaml`)

This manifest demonstrates **auto-versioning** by omitting the `tag` field.

*   **Behavior**:
    *   The operator generates a tag based on a hash of the pod template (e.g., `greeter-7mz6h89b`).
    *   **Updates**: *Any* change to the template (image, env vars, resources) results in a new hash, a new Configuration, and a new Restate deployment.
*   **Use Case**: CI/CD pipelines where you want every change to be a distinct, immutable deployment without managing tag versions manually.

## Local Development

If using locally built images (e.g., with `ko` or `docker build`):
*   Use the `dev.local/` prefix for your images (e.g., `dev.local/my-service:latest`).
*   Knative is configured to skip tag resolution for this prefix, allowing it to work without pushing to a remote registry.
