---
title: Keep Azure Event Grid behind the Azure provider
description: Azure owns Event Grid while Lampyr core executes generic provider plans.
---

# Keep Azure Event Grid behind the Azure provider

Azure owns Event Grid configuration, cloud resources, controller and adapter
images, Kubernetes manifests, and source create/status/delete semantics. Lampyr
core enables generic Knative Eventing and executes bounded provider-returned
Kubernetes plans without exposing Kubernetes credentials to guests; Kind has no
Event Grid simulator or Event Grid-specific behavior. This keeps Azure details
out of core while preserving a host-enforced Kubernetes security boundary.
