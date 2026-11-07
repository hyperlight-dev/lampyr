---
title: Support customer-owned Standalone Applications
description: Preserve the full Hyperlight lifecycle while customers own the listening process.
---

# Support customer-owned Standalone Applications

**Status:** Proposed

Lampyr will support both Handler Applications and Standalone Applications.
Handler Applications use Lampyr's generated Runtime Adapter. Standalone
Applications provide a customer-owned command and listening process while still
building into a Hyperlight-isolated Lampyr Service with the full build, deploy,
route, scale, status, logs, audit, and delete lifecycle.

Standalone scaffolding will use `ly scaffold service NAME --runtime RUNTIME
--standalone` and record the choice under `application.type: standalone`.
Every generated standalone file is customer-owned and Lampyr never
merge-upgrades it. Templates will separate application implementation from
server/bootstrap source, and documentation will contain paired Handler and
Standalone examples for each supported non-Wasm runtime.

This decision deliberately excludes arbitrary Kubernetes workloads and
existing deployable OCI application images. A Standalone Application must
satisfy Lampyr's source-to-rootfs-to-snapshot build path and listen on the
assigned service port. Bring Your Own Container support would weaken the
hardware-isolated Lampyr Service boundary and is not part of this model.

## Consequences

- Lampyr validates the model and command but treats standalone server behavior
  as customer-owned.
- Event delivery policy may be configured, but Lampyr cannot inspect whether a
  standalone server implements the event endpoint.
- Standalone source receives no adapter upgrade mechanism or modified-adapter
  diagnostics.
