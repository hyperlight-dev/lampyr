---
shell: bash
tags: [cli]
timeout: 30
---

# Protect projects and reject invalid settings

The failures below are deliberate. Each expected failure checks both its exit
status and its explanation, without starting a VM or contacting a cluster.

```bash markmason
cd "$E2E_HOME"
ly scaffold workspace errors-workspace
cd errors-workspace
```

## Keep existing application code

An existing handler belongs to the user:

```bash markmason
mkdir -p services/customized
printf 'my existing handler\n' > services/customized/handler.js
```

Initialization must refuse to replace it:

```bash markmason (exit 1)
ly scaffold service customized
```

```expected
ly: handler.js already exists
```

The original content is still present:

```bash markmason
cat services/customized/handler.js
```

```expected
my existing handler
```

## Reject an invalid local port

Port zero is not a usable local service port:

```bash markmason (exit 1)
ly dev --port 0
```

```expected
ly: port must be between 1 and 65535
```

## Validate execution policy before starting

Create a service and request reuse with an invalid idle timeout:

```bash markmason
ly scaffold service invalid-execution
```

```expected-substring
created Lampyr Service invalid-execution
```

```bash markmason
printf '\nexecution:\n  mode: reuse\n  idleTimeout: 0s\n' >> services/invalid-execution/lampyr.yaml
```

A reused instance needs a positive idle timeout:

```bash markmason (exit 1)
ly dev services/invalid-execution
```

```expected
ly: execution.idleTimeout must be a positive duration
```
