# Scripts

Top-level files in this directory are executable build, release, validation, or
integration workflows. `scripts/tests/` contains focused unit tests for script
logic; primary workflows do not live there.

## Knative portability

`test-knative-portability.sh` is a direct end-to-end integration test and demo.
It is not a test of another script.

The workflow:

- builds one application as a normal OCI image and a Lampyr snapshot artifact;
- creates separate stock-Knative and Lampyr Kind clusters;
- applies the same digest-pinned Knative Service to both;
- compares the defaulted API resources and application responses;
- verifies Pod execution, Fiber execution, scaling, reinstall, and repair; and
- removes its temporary clusters and registry.

Run it through the same recipe used by CI:

```bash
just test-knative-portability
```

CI runs this recipe in its own `knative-portability` job. The two disposable
clusters therefore run on a dedicated runner instead of extending another Kind
integration job.

Keep both clusters for an interactive demo:

```bash
LAMPYR_PORTABILITY_KEEP=1 just test-knative-portability
```
