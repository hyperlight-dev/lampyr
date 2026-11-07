---
title: Separate Service Handlers from vendored Runtime Adapters
description: Keep service code handler-focused while generated adapters remain editable and upgradeable.
---

# Separate Service Handlers from vendored Runtime Adapters

**Status:** Proposed

Lampyr will scaffold user-owned Service Handlers separately from tracked,
editable Runtime Adapters generated under `.lampyr-adapter/`. The adapter owns
the protocol server, root-filesystem build implementation, and language-native
contract types; the user retains `lampyr.yaml` and the final `Dockerfile`.
This keeps the ordinary developer experience handler-focused while preserving
transparent, reproducible builds and an escape hatch for advanced users.

Canonical Service Templates will live outside `docs/`. `docs/examples` will be
generated snapshots checked against those templates. Adapter upgrades will be
explicit through `ly scaffold service NAME --upgrade`: clean three-way merges
apply immediately, conflicts are previewed without writes, and
`--write-conflicts` writes markers for user resolution. Builds never rewrite
adapter source automatically.

The initial contract covers Node.js, Python, Go, Rust, and .NET with
language-idiomatic hooks. Wasm is deferred to [#2](https://github.com/hyperlight-dev/lampyr/issues/2), Bring Your Own Services to
Standalone Applications, and gRPC to a later interface design. This guest-side adapter is a
near-term step toward [#6](https://github.com/hyperlight-dev/lampyr/issues/6), where a stable Hyperlight host-function ABI can
change host behavior without rebuilding user binaries or user service images.

## Consequences

- Generated adapters and pristine merge baselines are committed with each
  service, increasing source size but making upgrades portable across clones.
- Customized or outdated adapters remain buildable with warnings and
  best-effort support.
- Ordinary services no longer expose a `protocols` block or runtime `command`;
  handler hooks and event policy express the service interface.
