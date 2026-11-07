FROM python:3.12-slim-bookworm@sha256:34386ef0cb081344d7ec1c103ba398e6e9f64e9ab3a1509accc92a4e24a07258 AS dependencies
COPY --from=kind requirements.txt /requirements.txt
RUN pip install --no-cache-dir --no-compile --require-hashes --target /site -r /requirements.txt

FROM --platform=$BUILDPLATFORM golang:1.26.7-bookworm@sha256:e8c859f5632dcfde7b32d2012b4351728f6437930887c2f6a91ea242459e5514 AS tools
ARG TARGETOS
ARG TARGETARCH
ADD --checksum=sha256:969cce82c4871bb829be798655abe2b6709b77cc0f42f7ff69293c621dfbbab0 \
    https://github.com/k3d-io/k3d/archive/refs/tags/v5.9.0.tar.gz /k3d.tar.gz
RUN mkdir /src /out && tar -xf /k3d.tar.gz -C /src \
    && cd /src/k3d-5.9.0 \
    && CGO_ENABLED=0 GOOS="$TARGETOS" GOARCH="$TARGETARCH" GOTOOLCHAIN=local \
        go build -trimpath -buildmode=pie \
        -ldflags='-s -w -X github.com/k3d-io/k3d/v5/version.Version=v5.9.0 -X github.com/k3d-io/k3d/v5/version.K3sVersion=v1.35.5-k3s1' \
        -o /out/k3d .
ADD --checksum=sha256:b45d935ab8f5faff8b0a03eb6c5c658636f47bc8170d892152b8eb57f5931315 \
    https://github.com/docker/cli/archive/refs/tags/v29.4.1.tar.gz /docker-cli.tar.gz
RUN mkdir -p /go/src/github.com/docker/cli \
    && tar -xf /docker-cli.tar.gz --strip-components=1 -C /go/src/github.com/docker/cli \
    && cd /go/src/github.com/docker/cli \
    && CGO_ENABLED=0 GOOS="$TARGETOS" GOARCH="$TARGETARCH" GO111MODULE=off GOTOOLCHAIN=local \
        go build -trimpath -buildmode=pie \
        -ldflags='-s -w -X github.com/docker/cli/cli/version.Version=29.4.1' -o /out/docker ./cmd/docker

FROM ghcr.io/hyperlight-dev/hyperlight-unikraft/python-shell@sha256:e2f65a42a7db22ed8721e29b1146f2e60a4da8ad0e7bc43f0038a922e81373c0 AS rootfs
WORKDIR /workspace
LABEL com.azure.containerizationassist.createdby="containerization-assist"
# Exported as a guest filesystem, not run as a container or network service.
HEALTHCHECK NONE
USER 10001:10001
COPY --from=dependencies /site/ /usr/local/lib/python3.12/site-packages/
COPY --from=tools /out/k3d /out/docker /usr/local/bin/
COPY --from=common process.py /usr/local/lib/python3.12/site-packages/process.py
COPY templates/ /templates/
