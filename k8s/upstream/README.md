# Bootstrap manifests

These files are unmodified upstream release assets. `sources.json` records each
source URL, version, SPDX license identifier, and SHA-256 checksum. All current
assets are Apache-2.0. Platform packaging checks every file and includes the
source record. Bootstrap reads these files from the platform root and checks
their pinned checksums. There is no download fallback.

To update an asset, download the required release file. Review its changes and
update its record in `sources.json`. Run the packaging and bootstrap tests.
The CLI must be built with the updated source record.

Fiberd uses the CapacityGrant and GrantPolicy CRDs in the bundled
`build/fiberd-k8s-controller.yaml`. That manifest is built from the controller
commit in `upstream/pins.env`, which is also in the platform archive.
Do not add the retired standalone Grant CRD.

Container images and other runtime dependencies still need separate access.
