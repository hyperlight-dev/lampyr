set shell := ["bash", "-euo", "pipefail", "-c"]
set unstable := true

mod cli 'cli/Justfile'
mod azure 'providers/azure/Justfile'
mod kind 'providers/kind/Justfile'
mod providers 'providers/Justfile'
mod k3d 'providers/k3d/Justfile'
mod helper 'helper/Justfile'
mod upstream 'upstream/Justfile'

repo-root := justfile_directory()
exe-suffix := if os() == "windows" { ".exe" } else { "" }
developer-workspace := env_var_or_default("LAMPYR_WORKSPACE", repo-root / "build/workspace")

# Kind release installed by `just install-kind` in every workflow (CI, release
# gates, Copilot); providers/kind/templates/kind-config.yaml pins its node image.
kind-version := "v0.33.0"

default:
    @just --list

# Download and verify the pinned upstream Hyperlight runtime and matching kernel.
setup-hluk: cli::setup-hluk

# Pull a checksum-pinned upstream filesystem image for a runtime.
setup-rootfs runtime platform="linux/amd64":
    just cli::setup-rootfs "{{runtime}}" "{{platform}}"

# Build the serving controller, the one binary of the fiberd image; grant
# Pods run stock fiberd and the helper.
build-fiberd:
    mkdir -p build
    CGO_ENABLED=0 go build -o build/serving-controller ./cmd/serving-controller

# Build the standalone upstream-protocol consumer, not a Knative activator fork.
build-control-plane:
    mkdir -p build
    CGO_ENABLED=0 go build -o build/fiber-router ./cmd/fiber-router

# Prepare the Linux/amd64 fiberd image context without publishing it.
prepare-fiberd-image:
    mkdir -p build
    GOOS=linux GOARCH=amd64 CGO_ENABLED=0 go build -trimpath -ldflags="-s -w" -o build/serving-controller ./cmd/serving-controller
    python3 "{{repo-root}}/scripts/check_preview_images.py" --root "{{repo-root}}" --image fiberd --require-binaries

# Prepare the Linux/amd64 fiber-router image context without publishing it.
prepare-fiber-router-image:
    mkdir -p build
    GOOS=linux GOARCH=amd64 CGO_ENABLED=0 go build -trimpath -ldflags="-s -w" -o build/fiber-router ./cmd/fiber-router
    python3 "{{repo-root}}/scripts/check_preview_images.py" --root "{{repo-root}}" --image fiber-router --require-binaries

# The helper records a template's --version facts; zygotectl and grant-issuer
# come from a fiberd checkout that builds Hyperlight snapshot artifacts.

# Build the helper, zygotectl and grant-issuer that fiberd templates need.
[positional-arguments]
fiberd-template-tools fiberd: helper::prepare-image
    CGO_ENABLED=0 go -C "$1" build -trimpath -o "{{repo-root}}/build/zygotectl" ./cmd/zygotectl
    CGO_ENABLED=0 go -C "$1" build -trimpath -o "{{repo-root}}/build/grant-issuer" ./cmd/grant-issuer

# zygotectl and grant-issuer come from the fiberd upstream/pins.env pins.

# Build the helper, zygotectl and grant-issuer into build/ for this checkout's ly.
template-tools: (upstream::source "fiberd")
    just --justfile "{{repo-root}}/Justfile" fiberd-template-tools "{{repo-root}}/build/upstream/fiberd"

# A platform package carries the helper, built from this checkout, and
# zygotectl and grant-issuer for each of its targets, from the pinned
# fiberd (scripts/package-platform.py).

# Build the fiberd template tools every platform package carries.
platform-template-tools: helper::prepare-image upstream::template-tools

# Build the developer-facing Lampyr CLI.
build-ly: cli::build build-dev-supervisor
    mkdir -p build
    cp "{{repo-root}}/cli/target/debug/ly{{exe-suffix}}" "{{repo-root}}/build/ly{{exe-suffix}}"

# Build the packaged native local-home companion (not a user toolchain dependency).
build-dev-supervisor:
    mkdir -p build
    CGO_ENABLED=0 go build -o build/ly-dev ./cmd/ly-dev

# Build the Azure-owned Event Grid controller and adapter.
build-eventgrid: azure::build-eventgrid

# Prepare every Linux/amd64 preview image context without publishing it.
prepare-preview-images: prepare-fiberd-image prepare-fiber-router-image helper::prepare-image upstream::prepare-fiberd-image upstream::prepare-controller-image azure::prepare-preview-images
    python3 "{{repo-root}}/scripts/check_preview_images.py" --root "{{repo-root}}" --require-binaries

# Package all preview release assets, including the verified installer entrypoint.
package-preview provider-artifacts="": (cli::package-preview provider-artifacts) package-installer

# Package all assets for a vX.Y.Z release after checking every version reference.
[positional-arguments]
[no-cd]
package-release tag provider-artifacts="": (release-version-check tag)
    tag="$1"; tag="${tag#tag=}"; \
    just --justfile "{{repo-root}}/Justfile" cli::package-release "${tag#v}" "$2"
    just --justfile "{{repo-root}}/Justfile" package-installer

# Add the checksummed installer entrypoint to the release assets.
[private]
package-installer:
    install -m 0755 "{{repo-root}}/scripts/install-ly.sh" "{{repo-root}}/build/dist/install-ly.sh"
    cd "{{repo-root}}/build/dist" && sha256sum install-ly.sh > install-ly.sh.sha256

# Set every committed release version reference to X.Y.Z (see the release skill).
release-version-set version:
    version="{{version}}"; \
    python3 "{{repo-root}}/scripts/release-version.py" set "${version#version=}"

# Check version references agree; with a vX.Y.Z tag, also require they match it.
release-version-check tag="":
    tag="{{tag}}"; tag="${tag#tag=}"; \
    args=(); [[ -z "$tag" ]] || args=(--tag "$tag"); \
    python3 "{{repo-root}}/scripts/release-version.py" check "${args[@]}"

# Verify and mirror the private preview images into a destination registry.
preview-mirror-images destination:
    destination="{{destination}}"; \
    "{{repo-root}}/scripts/mirror-preview-images.sh" "${destination#destination=}"

# Check preview image definitions and their focused regression tests.
test-preview-images:
    python3 "{{repo-root}}/scripts/tests/test_preview_images.py"
    python3 "{{repo-root}}/scripts/tests/test_preview_mirror.py"
    python3 "{{repo-root}}/scripts/check_preview_images.py" --root "{{repo-root}}"

# Check the release gates' parsing of grants, audit records and metrics.
test-release-gates:
    python3 "{{repo-root}}/scripts/tests/test_fiberd_grants.py"
    python3 "{{repo-root}}/scripts/tests/test_disruption_gate.py"

# Check preview and versioned publication policy and the authenticated installer.
test-preview-release: test-preview-images release-version-check test-release-gates
    python3 "{{repo-root}}/scripts/tests/test_preview_workflow.py"
    python3 "{{repo-root}}/scripts/tests/test_release_version.py"
    python3 "{{repo-root}}/scripts/tests/test_cluster_inputs.py"
    bash -n "{{repo-root}}/scripts/publish-preview-release.sh"
    bash -n "{{repo-root}}/scripts/publish-version-release.sh"
    bash -n "{{repo-root}}/scripts/require-new-release.sh"
    "{{repo-root}}/scripts/tests/test-install-ly.sh"

# Verify committed examples match every supported runtime template.
examples-check: build-ly cli::test-handler-contracts
    "{{repo-root}}/scripts/check-generated-examples.sh"

# Exercise every committed runtime example through local Hyperlight development.
test-examples-local: setup-hluk build-ly
    "{{repo-root}}/scripts/test-examples-local.sh"

# Exercise local examples using build artifacts supplied by CI.
test-examples-local-prebuilt:
    "{{repo-root}}/scripts/test-examples-local.sh"

# Install the locked website dependencies without touching platform tooling.
site-install:
    npm --prefix site ci

# Start the local website server until interrupted with Ctrl+C.
site-run: site-install
    npm --prefix site run dev

# Build and validate the static website, canonical docs, routes, and dependencies.
site-check: site-install
    npm --prefix site test
    npm --prefix site run check
    npm --prefix site run build
    npm --prefix site audit --audit-level=high

# Build the production GitHub Pages artifact.
site-build: site-install
    npm --prefix site run build

# Install the pinned Kind release into /usr/local/bin after verifying its checksum.
install-kind:
    [ "$(uname -sm)" = "Linux x86_64" ] || { echo "install-kind supports linux-amd64 only" >&2; exit 1; }; \
    dir=$(mktemp -d); \
    trap 'rm -rf "$dir"' EXIT; \
    for asset in kind-linux-amd64 kind-linux-amd64.sha256sum; do \
        curl --fail --location --output "$dir/$asset" \
            "https://kind.sigs.k8s.io/dl/{{kind-version}}/$asset"; \
    done; \
    (cd "$dir" && sha256sum --check kind-linux-amd64.sha256sum); \
    sudo install -m 0755 "$dir/kind-linux-amd64" /usr/local/bin/kind

# Install the pinned Markdown test runner unless that version is already present.
install-mm:
    if [ -x "{{repo-root}}/build/tools/mm" ] && \
        [ "$("{{repo-root}}/build/tools/mm" --version)" = "mm 0.5.0" ]; then exit 0; fi; \
    [ "$(uname -sm)" = "Linux x86_64" ] || { echo "install-mm supports linux-amd64 only" >&2; exit 1; }; \
    dir=$(mktemp -d); \
    trap 'rm -rf "$dir"' EXIT; \
    curl --fail --silent --show-error --location --output "$dir/mm.tar.gz" \
        https://github.com/jsturtevant/markmason/releases/download/v0.5.0/markmason-v0.5.0-x86_64-unknown-linux-gnu.tar.gz; \
    echo "f625336764be2fdc85500b4ed516560c634dff0d099cd4eeabdf2790f1655869  $dir/mm.tar.gz" | sha256sum --check; \
    mkdir -p "{{repo-root}}/build/tools"; \
    tar --extract --gzip --file "$dir/mm.tar.gz" --directory "{{repo-root}}/build/tools" mm

prepare-workspace workspace=developer-workspace: build-ly
    if [[ ! -f "{{workspace}}/lampyr.workspace.yaml" ]]; then build/ly scaffold workspace "{{workspace}}"; fi

# Install the complete preview platform into a Kind cluster.
cluster-install workspace=developer-workspace: (prepare-workspace workspace)
    build/ly cluster install --provider kind --allow-docker --workspace "{{workspace}}"

# Install Lampyr using the independently packaged k3d provider.
cluster-install-k3d workspace=developer-workspace: (prepare-workspace workspace)
    build/ly cluster install --provider k3d --allow-docker --workspace "{{workspace}}"

# Test the provider lifecycle and every committed runtime example on k3d.
test-k3d-integration: build-ly k3d::build cli::runtime-images
    bash "{{repo-root}}/scripts/test-k3d-integration.sh"

# Report the health of every required preview platform component.
cluster-health workspace=developer-workspace: build-ly
    build/ly cluster health --workspace "{{workspace}}"

# Remove preview platform resources.
cluster-uninstall workspace=developer-workspace: build-ly
    build/ly cluster uninstall --workspace "{{workspace}}"

# Create or configure an AKS cluster with the latest regional Kubernetes version.
cluster-install-azure config name="lampyr" workspace=developer-workspace: (prepare-workspace workspace)
    build/ly cluster install --provider azure --name "{{name}}" --provider-config "{{config}}" --approve-provider-manifests --workspace "{{workspace}}"

# Report AKS and Lampyr platform health.
cluster-health-azure name="lampyr" workspace=developer-workspace: build-ly
    build/ly cluster health --context "aks-{{name}}" --workspace "{{workspace}}"


# Remove Lampyr from AKS while retaining the cluster by default.
cluster-uninstall-azure name="lampyr" delete-cluster="false" workspace=developer-workspace: build-ly
    args=(--provider azure --name "{{name}}" --context "aks-{{name}}" --approve-provider-manifests --workspace "{{workspace}}"); \
    if [[ "{{delete-cluster}}" == "true" ]]; then args+=(--delete-cluster); fi; \
    build/ly cluster uninstall "${args[@]}"

# Build every locally produced binary.
build: setup-hluk build-fiberd build-control-plane build-ly build-eventgrid

# Check the native CLI, the helper and the Go components.
lint: cli::lint helper::lint azure::lint lint-go check-license-headers

# Report suspicious constructs in the Go components.
lint-go:
    go vet ./cmd/... ./internal/...

# Verify every first-party Rust and Go file carries the license header.
check-license-headers:
    "{{repo-root}}/hack/check-license-headers.sh"

# Verify every GitHub Action is pinned to its latest released commit.
check-action-versions:
    "{{repo-root}}/hack/update-actions.sh" --check

# Run focused unit tests for Lampyr.
test-unit:
    go test ./cmd/... ./internal/...
    python3 "{{repo-root}}/scripts/tests/test_worker_workspace.py"

# Install the pinned Semgrep version for the optional test check.
install-test-linters:
    python3 -m venv "{{repo-root}}/build/lint-venv"
    "{{repo-root}}/build/lint-venv/bin/python" -m pip install --quiet -r "{{repo-root}}/scripts/lint-requirements.txt"

# Reject disabled tests across languages with Semgrep.
check-no-skipped-tests: install-test-linters
    "{{repo-root}}/build/lint-venv/bin/semgrep" scan --config "{{repo-root}}/scripts/tests/lint-fixtures/disabled-tests.yml" --metrics off --disable-version-check --disable-nosem --strict --error
    "{{repo-root}}/build/lint-venv/bin/semgrep" --test --metrics off --disable-version-check scripts/tests/lint-fixtures
    python3 "{{repo-root}}/scripts/tests/test_contract_selection.py"

# Create a disposable Kind cluster and exercise the complete Lampyr lifecycle.
test-kind-integration: setup-hluk build-fiberd build-control-plane build-ly kind::build
    LAMPYR_TEST_ROOTFS="{{repo-root}}/providers/kind/build/rootfs/rootfs.cpio" just cli::test-runtime
    LAMPYR_CI_RELEASE_GATES=1 "{{repo-root}}/scripts/test-kind-integration.sh"

# Create a disposable AKS cluster and verify Blob Event Grid delivery to a Lampyr Service.
test-azure-eventgrid-integration: build-ly azure::build azure::build-eventgrid
    "{{repo-root}}/scripts/test-azure-eventgrid-integration.sh"

# Exercise the full Kind integration using build artifacts supplied by CI.
test-kind-integration-prebuilt: kind::build
    LAMPYR_TEST_ROOTFS="{{repo-root}}/providers/kind/build/rootfs/rootfs.cpio" just cli::test-runtime
    LAMPYR_CI_SUITE=full LAMPYR_CI_RELEASE_GATES=1 "{{repo-root}}/scripts/test-kind-integration.sh"

# Exercise runtime behavior in the parallel Kind CI shard.
test-kind-runtime-integration-prebuilt: kind::build
    LAMPYR_CI_SUITE="${LAMPYR_CI_SUITE:-runtime}" LAMPYR_CI_RELEASE_GATES="${LAMPYR_CI_RELEASE_GATES:-1}" "{{repo-root}}/scripts/test-kind-integration.sh"

# Apply one identical Knative Service to stock Pods and Lampyr Fibers.
test-knative-portability: setup-hluk build-fiberd build-control-plane build-ly kind::build
    LAMPYR_PORTABILITY_PREBUILT=1 "{{repo-root}}/scripts/test-knative-portability.sh"

# Exercise provider and existing-cluster lifecycle using the CI-prepared Kind artifact.
test-kind-lifecycle-integration-prebuilt:
    test -f "{{repo-root}}/providers/kind/build/rootfs/rootfs.cpio"
    LAMPYR_CI_SUITE="${LAMPYR_CI_SUITE:-lifecycle}" "{{repo-root}}/scripts/test-kind-integration.sh"

# Create Kind outside Lampyr and exercise installation and one service lifecycle.
test-existing-kind-integration: build-ly cli::runtime-images
    "{{repo-root}}/scripts/test-existing-kind-integration.sh"

# Make LAMPYR_CLUSTER_INPUTS this checkout's default controller inputs, served on `port`.
[positional-arguments]
use-cluster-inputs port:
    bash "{{repo-root}}/scripts/use-cluster-inputs.sh" "$1"

# Build the CLI and exercise the first-service walkthrough.
test-walkthrough: install-mm build-ly kind::build cli::runtime-images
    PATH="{{repo-root}}/build:$PATH" "{{repo-root}}/build/tools/mm" "{{repo-root}}/docs/walkthrough.md" --cwd "{{repo-root}}"

# Exercise the focused executable CLI intent documents.
test-cli: install-mm build-ly
    PATH="{{repo-root}}/build:$PATH" "{{repo-root}}/build/tools/mm" "{{repo-root}}/tests/cli" --cwd "{{repo-root}}"

# Run the installed platform's cached p95 latency gate.
release-latency-gate service url requests="200":
    python3 scripts/release-gate.py \
        --service "{{service}}" \
        --url "{{url}}" \
        --requests "{{requests}}" \
        --concurrency "1"

# Run the 10,000-request activation storm and leak release gate.
release-reliability-gate service url requests="10000" concurrency="16":
    python3 scripts/release-gate.py \
        --service "{{service}}" \
        --url "{{url}}" \
        --requests "{{requests}}" \
        --concurrency "{{concurrency}}" \
        --p95-ms "0"

# Exercise agent restart, placement failure, policy rejection, and control-plane outage.
release-disruption-gate service url:
    python3 scripts/disruption-gate.py \
        --service "{{service}}" \
        --url "{{url}}"

# Run the repository test suites used in CI.
test: test-unit test-cli examples-check test-preview-release cli::test cli::test-intent helper::test azure::test kind::test k3d::test test-examples-local test-kind-integration test-k3d-integration test-existing-kind-integration test-knative-portability test-walkthrough

# Reproduce the build and focused unit and CLI checks.
ci-unit: build test-unit test-cli examples-check test-preview-release cli::test cli::test-intent helper::test azure::test-unit kind::test-unit k3d::test-unit

# Reproduce the build and regression suite from a clean checkout.
ci: ci-unit azure::test kind::test k3d::test test-kind-integration test-k3d-integration test-existing-kind-integration test-knative-portability test-walkthrough
