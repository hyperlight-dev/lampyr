---
title: Use Workspace and Lampyr Home as the filesystem model
description: Customer source lives in a Workspace while private runtime data lives in Lampyr Home.
---

# Use Workspace and Lampyr Home as the filesystem model

The public filesystem model separates optional customer-owned Workspaces from
private machine-local Lampyr Home. A standalone Lampyr Service does not require
a Workspace. A Workspace groups Services, named infrastructure sources, and
editable Kubernetes manifests copied from the installed Lampyr runtime when
the Workspace is scaffolded.

Downloaded platform/provider assets, tools, ownership state, and authentication
remain Lampyr Home details; user-facing platform-root and data-directory
controls are removed to avoid exposing implementation taxonomy. Cluster
lifecycle reads the Workspace's manifest copies so customers can customize
them without modifying Lampyr Home.

Cluster commands accept a Workspace root and derive a named Infrastructure
Source at `infra/PROVIDER/NAME`; the term Workspace never refers to that nested
provider directory.

Workspace Kubernetes manifests cover the shared Lampyr platform. Provider-only
node preparation and integration manifests remain inside their provider
artifacts so a Workspace never presents an editable copy that runtime behavior
ignores.
