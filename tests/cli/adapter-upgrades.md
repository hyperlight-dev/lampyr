---
shell: bash
tags: [cli]
timeout: 30
---

# Test explicit adapter upgrades

These tests do not need a platform, compiler, or VM.

```bash markmason
cd "$E2E_HOME"
ly scaffold workspace upgrade-workspace
cd upgrade-workspace
ly scaffold service api --runtime python
```

## Keep customer changes after an upgrade

Pretend the service came from an older template whose `lampyr_types.py`
differs, so the upgrade has a real template change to merge. Add a customer
edit to another adapter file. Record the hashes of user-owned files:

```bash markmason
python3 - <<'PY'
import hashlib, json
from pathlib import Path
adapter = Path("services/api/.lampyr-adapter")
older = b"# older template types\n"
(adapter / "baseline/lampyr_types.py").write_bytes(older)
(adapter / "lampyr_types.py").write_bytes(older)
path = adapter / "adapter.json"
metadata = json.loads(path.read_text())
metadata["templateVersion"] = "0.0.1"
metadata["files"]["lampyr_types.py"] = "sha256:" + hashlib.sha256(older).hexdigest()
path.write_text(json.dumps(metadata))
PY
printf '\n# customer customization\n' >> services/api/.lampyr-adapter/server.py
sha256sum services/api/handler.py services/api/lampyr.yaml services/api/Dockerfile > owned.before
ly scaffold service api --upgrade
```

```expected-substring
upgraded runtime adapter for api
```

Check that the template change merged, and that the edit and user-owned files
are unchanged:

```bash markmason
cmp services/api/.lampyr-adapter/lampyr_types.py services/api/.lampyr-adapter/baseline/lampyr_types.py && echo merged
! grep -q 'older template types' services/api/.lampyr-adapter/lampyr_types.py && echo replaced
sha256sum --check owned.before
grep '^# customer customization$' services/api/.lampyr-adapter/server.py
```

```expected
merged
replaced
services/api/handler.py: OK
services/api/lampyr.yaml: OK
services/api/Dockerfile: OK
# customer customization
```

## Do not write during the default conflict preview

Set an older pristine version in the service metadata. Keep the installed
template as the merge target:

```bash markmason
python3 - <<'PY'
import hashlib, json
from pathlib import Path
adapter = Path("services/api/.lampyr-adapter")
baseline = b"old template\n"
(adapter / "baseline/server.py").write_bytes(baseline)
(adapter / "server.py").write_text("customer replacement\n")
path = adapter / "adapter.json"
metadata = json.loads(path.read_text())
metadata["templateVersion"] = "0.0.1"
metadata["files"]["server.py"] = "sha256:" + hashlib.sha256(baseline).hexdigest()
path.write_text(json.dumps(metadata))
PY
find services/api -type f -print0 | sort -z | xargs -0 sha256sum > conflict.before
```

The customer and installed files conflict. The default command must fail without
changing any service file:

```bash markmason (exit 1)
ly scaffold service api --upgrade --color never
```

```expected-substring
runtime adapter conflicts; nothing written
```

```bash markmason
find services/api -type f -print0 | sort -z | xargs -0 sha256sum > conflict.after
cmp conflict.before conflict.after
test ! -e services/api/.lampyr-adapter/pending.json
```

## Write and resolve conflict markers

Write the markers and pinned target explicitly:

```bash markmason (exit 1)
ly scaffold service api --upgrade --write-conflicts --color never
```

```expected-substring
runtime adapter conflicts written
```

The command must reject unresolved markers:

```bash markmason (exit 1)
ly scaffold service api --upgrade
```

```expected-substring
unresolved conflict markers
```

Write a resolution and run the command again. Lampyr must keep the resolution:

```bash markmason
printf '# chosen customer resolution\n' > services/api/.lampyr-adapter/server.py
ly scaffold service api --upgrade
test ! -e services/api/.lampyr-adapter/pending.json
test ! -e services/api/.lampyr-adapter/pending-baseline
sha256sum --check owned.before
cat services/api/.lampyr-adapter/server.py
```

```expected-substring
# chosen customer resolution
```

## Reject an unsupported runtime

```bash markmason (exit 1)
ly scaffold service unsupported --runtime wasm
```

```expected-substring
runtime "wasm" is not supported
```
