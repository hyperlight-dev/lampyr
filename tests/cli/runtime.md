---
shell: bash
tags: [cli]
timeout: 30
---

# Create a service with a selected runtime

Create a Python Lampyr Service.
The Service name does not depend on the directory name.
Use the test runner's temporary directory:

```bash markmason
cd "$E2E_HOME"
ly scaffold workspace runtime-workspace
cd runtime-workspace
```

## Create the service

Select Python.
Use `hello-python` as the Service name:

```bash markmason
ly scaffold service hello-python --runtime python
```

```expected-substring
created Lampyr Service hello-python
```

## Check the generated files

Examine the application files and the build files.
The rootfs build and artifact packaging use separate Dockerfiles:

```bash markmason
ls -1 services/hello-python/.lampyr-adapter/Dockerfile.rootfs services/hello-python/.lampyr-adapter/adapter.json services/hello-python/.lampyr-adapter/baseline/server.py services/hello-python/.lampyr-adapter/requirements.txt services/hello-python/.lampyr-adapter/server.py services/hello-python/Dockerfile services/hello-python/handler.py services/hello-python/lampyr.yaml
```

```expected
services/hello-python/.lampyr-adapter/Dockerfile.rootfs
services/hello-python/.lampyr-adapter/adapter.json
services/hello-python/.lampyr-adapter/baseline/server.py
services/hello-python/.lampyr-adapter/requirements.txt
services/hello-python/.lampyr-adapter/server.py
services/hello-python/Dockerfile
services/hello-python/handler.py
services/hello-python/lampyr.yaml
```

The manifest records the Service policy.
The adapter metadata records the guest entrypoint.
Examine the manifest:

```bash markmason
head -n 4 services/hello-python/lampyr.yaml
```

```expected
apiVersion: lampyr.dev/v1alpha1
kind: Project
name: hello-python
runtime: python
```

Read the guest entrypoint from the adapter metadata:

```bash markmason
python3 -c 'import json; print(json.load(open("services/hello-python/.lampyr-adapter/adapter.json"))["entrypoint"])'
```

```expected
/entrypoint.py
```

## Examine compiled Service files

Compiled Services provide application handlers in separate files.
Create the Services and examine their handler files:

```bash markmason
for runtime in go rust dotnet; do
  ly scaffold service "hello-$runtime" --runtime "$runtime" >/dev/null
done
ls services/hello-go/handler.go services/hello-rust/handler.rs services/hello-dotnet/Handler.cs
```

```expected
services/hello-dotnet/Handler.cs
services/hello-go/handler.go
services/hello-rust/handler.rs
```

The adapter owns the entrypoints and native build files.
Check the metadata and the baseline files:

```bash markmason
python3 - <<'PY'
import json
from pathlib import Path
for runtime in ("go", "rust", "dotnet"):
    service = Path("services") / f"hello-{runtime}"
    manifest = (service / "lampyr.yaml").read_text()
    assert "\ncommand:" not in manifest and "\nprotocols:" not in manifest
    metadata = json.loads((service / ".lampyr-adapter/adapter.json").read_text())
    assert metadata["runtime"] == runtime
    assert all((service / ".lampyr-adapter/baseline" / name).is_file()
               for name in metadata["files"])
    print(runtime, metadata["entrypoint"])
PY
```

```expected
go /entrypoint
rust /entrypoint
dotnet /app/LampyrService
```
