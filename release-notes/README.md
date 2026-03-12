# Release Notes Process

This directory contains release notes for the Restate Operator, organized to track changes between releases.

## Structure

```
release-notes/
├── README.md          # This file
├── v2.2.0.md          # Consolidated release notes for v2.2.0
├── v2.3.0.md          # (future releases follow the same pattern)
└── unreleased/        # Release notes for changes not yet released
    └── *.md           # Individual release note files
```

## Adding Release Notes

When making a significant change that affects users, create a release note file in the `unreleased/` directory:

1. **Create a new file** in `unreleased/` with a descriptive name:
   - Format: `<issue-number>-<short-description>.md`
   - Example: `94-configurable-cluster-dns.md`

2. **Structure your release note** with the following sections:
   ```markdown
   # Release Notes for Issue #<number>: <Title>

   ## Behavioral Change / New Feature / Bug Fix / Breaking Change

   ### What Changed
   Brief description of what changed

   ### Why This Matters
   Explain the impact and reasoning

   ### Impact on Users
   - How this affects existing deployments
   - How this affects new deployments
   - Any migration considerations

   ### Migration Guidance
   Steps users should take if needed, including:
   - Helm value changes
   - CRD field changes
   - Environment variable or CLI flag changes

   ### Related Issues
   - Issue #XXX: Description
   ```

3. **Commit the release note** with your changes:
   ```bash
   git add release-notes/unreleased/<your-file>.md
   git commit -m "Add release notes for <change>"
   ```

## Release Process

When creating a new release:

1. **Review all unreleased notes**: Check `unreleased/` for all pending release notes

2. **Create a consolidated release notes file** (`v<version>.md`) with the following structure:

   ```markdown
   # Restate Operator v<version> Release Notes

   ## Highlights
   - 3-5 bullet points summarizing the most important changes

   ## Table of Contents
   (Links to all sections below)

   ## Breaking Changes
   (Items sorted by impact, most impactful first)

   ## Deprecations

   ## New Features
   (Items sorted by impact, most impactful first)

   ## Improvements
   ### CRDs
   ### Helm Chart
   ### Network Policies
   ### Observability
   ### Stability

   ## Bug Fixes
   (Grouped by area: RestateCluster, RestateDeployment, RestateCloudEnvironment, Helm, etc.)
   ```

3. **Consolidation guidelines**:
   - Sort items by impact within each category (most impactful first)
   - Preserve all migration guidance, configuration examples, and code snippets
   - Keep all related issue/PR links
   - For items spanning multiple categories (e.g., both breaking change and new feature), place in the primary category with full context

4. **Delete the individual unreleased files** after consolidation:
   ```bash
   rm release-notes/unreleased/*.md
   ```

5. **Use the consolidated notes** to prepare:
   - GitHub release description
   - Documentation updates

## Guidelines

### When to Write a Release Note

Write a release note for:
- **Breaking changes**: Any change that requires user action or breaks existing functionality (CRD schema changes, removed Helm values, changed defaults)
- **Behavioral changes**: Changes to defaults, reconciliation behavior, or network policies
- **New features**: User-facing features or capabilities (new CRD fields, new Helm values, new CLI flags)
- **Important bug fixes**: Fixes that significantly impact reliability or security
- **Deprecations**: CRD fields, Helm values, or APIs being deprecated

### When NOT to Write a Release Note

Skip release notes for:
- Internal refactoring with no user impact
- Test changes
- Documentation-only changes (unless significant)
- Minor dependency updates
- Build system changes

### Writing Style

- **Be clear and concise**: Users should quickly understand the change
- **Focus on impact**: Explain what users need to know and do
- **Provide examples**: Include Helm values, CRD snippets, or CLI examples
- **Link to documentation**: Reference detailed docs when available
- **Be honest about breaking changes**: Don't hide backwards-incompatible changes

## Examples

### Breaking Change Example
```markdown
# Release Notes for Issue #123: Remove deprecated networkPolicy field

## Breaking Change

### What Changed
The deprecated `spec.security.networkPolicy` field has been removed from
the RestateCluster CRD. Use `spec.security.networkPolicies` (plural) instead.

### Impact on Users
- Existing RestateCluster resources using the old field will fail validation on upgrade
- The replacement field has been available since v2.0.0

### Migration Guidance
Update your RestateCluster manifests before upgrading:

\```yaml
# Old (removed)
spec:
  security:
    networkPolicy:
      enabled: true

# New
spec:
  security:
    networkPolicies:
      enabled: true
\```

### Related Issues
- Issue #123: Remove deprecated networkPolicy field
```

### New Feature Example
```markdown
# Release Notes for Issue #456: Add support for custom annotations on StatefulSet

## New Feature

### What Changed
Added `spec.compute.annotations` field to the RestateCluster CRD, allowing
users to set custom annotations on the managed StatefulSet.

### Impact on Users
- Existing deployments: No impact, field is optional
- New deployments: Can now set annotations for integrations that require them
  (e.g., Vault agent injection, Datadog, Prometheus scraping)

### Migration Guidance
Add annotations to your RestateCluster:

\```yaml
spec:
  compute:
    annotations:
      vault.hashicorp.com/agent-inject: "true"
      prometheus.io/scrape: "true"
\```

### Related Issues
- Issue #456: Add support for custom annotations on StatefulSet
```
