<div align="center">
  <img src="site/src/assets/lampyr-mark.svg" alt="Lampyr firefly mark" width="96" height="96" />
  <h1>Lampyr</h1>
  <p><strong>Serverless on Kubernetes where every request runs in its own hardware-isolated micro-VM, restored from a snapshot in milliseconds.</strong></p>
  <p>Lampyr is part of <a href="https://github.com/hyperlight-dev/hyperlight">Hyperlight</a>, a <a href="https://cncf.io/">Cloud Native Computing Foundation</a> sandbox project.</p>
  <p>
    <a href="docs/walkthrough.md">Walkthrough</a>
    ·
    <a href="docs/requirements.md">Requirements</a>
    ·
    <a href="docs/lampyr-service.md">Lampyr Services</a>
    ·
    <a href="docs/cli/README.md"><code>ly</code> CLI</a>
  </p>
</div>

Lampyr is an open-source platform for lightweight services that run
with fast, hardware-isolated VMs. It provides a serverless
workflow on Kubernetes while letting developers use familiar application
models and languages.

Lampyr Services can handle HTTP, streaming, WebSocket, and event-driven
workloads. Service instances scale to zero when idle and activate in short-lived
microVMs when traffic arrives.

Lampyr supports local Kind and k3d clusters and AKS, with
templates for Node.js, Python, Go, Rust, and .NET.
It can also install directly into an existing Kubernetes context without
an infrastructure provider; Kind, k3d, and Azure providers are separate guest artifacts.

## Why Lampyr?

Lampyr is named after Lampyridae, the firefly family: small points of light
that stay dark until it is time to signal. The name fits a platform where
Lampyr Services can scale to zero, then activate inside
short-lived, hardware-isolated microVMs when work arrives.

## Getting started

### Install the CLI

Install `ly` and its platform bundle from the latest release on Linux/amd64 or
macOS/arm64. The installer checks every archive's checksum and GitHub
attestation; see [`ly setup`](docs/cli/setup.md) for the verified install.

```bash
gh release download v0.1.1 --repo hyperlight-dev/lampyr --pattern install-ly.sh
bash install-ly.sh v0.1.1
export PATH="$HOME/.local/bin:$PATH"
ly doctor
```

To build `ly` from a checkout instead, run `just build-ly`.

### Walkthrough

See the [walkthrough](docs/walkthrough.md) for the getting started and
the [local requirements](docs/requirements.md) for host prerequisites and
`ly doctor`. See [Contributing](docs/contributing.md#build-from-source)
when building Lampyr itself.

See the [Kind cluster guide](docs/cluster-kind.md) for prerequisites and
additional install options, or use the [k3d cluster guide](docs/cluster-k3d.md).
For AKS, see the
[AKS cluster guide](docs/cluster-aks.md), including Azure node requirements.

## Website

The Lampyr website and searchable field guide are published at
<https://hyperlight-dev.github.io/lampyr/>.

Website tooling is isolated in [`site/`](site/) and consumes the canonical
Markdown in [`docs/`](docs/) at build time. Validate it without building the
platform:

```bash
just site-check
```

## Documentation

- [Website field guide](https://hyperlight-dev.github.io/lampyr/docs/)
- [Local requirements](docs/requirements.md)
- [Lampyr Service overview](docs/lampyr-service.md)
- [CLI reference](docs/cli/README.md)
- [Product design](docs/designs/product-design.md)
- [Browse all documentation](docs/)

## Build and test

Build all project components and run the full validation suite:

```bash
just build
just test
```

For a quicker development loop, use `just build-ly` to build only the CLI and
`just test-unit` to run focused unit tests.

## Join our community

Please review [CONTRIBUTING.md](./CONTRIBUTING.md) for how to contribute.

We hold **weekly community meetings**, open to everyone.

- **When**: Mondays at 09:00 (PST/PDT). Convert to your local time [here](https://dateful.com/convert/pst-pdt-pacific-time?t=09).
- **Where**: See the [Hyperlight community meeting notes](https://hackmd.io/blCrncfOSEuqSbRVT9KYkg#Agenda) for the agenda and join link.

## Chat with us on the CNCF Slack

The Hyperlight project Slack lives in the CNCF Slack `#hyperlight` channel. [Join the CNCF Slack](https://www.cncf.io/membership-faq/#how-do-i-join-cncfs-slack), then join `#hyperlight`.

## More information

For the broader project, see the main [Hyperlight](https://github.com/hyperlight-dev/hyperlight) repository.

## Code of Conduct

See the [CNCF Code of Conduct](https://github.com/cncf/foundation/blob/main/code-of-conduct.md).

## License

Licensed under the [Apache License, Version 2.0](./LICENSE).
