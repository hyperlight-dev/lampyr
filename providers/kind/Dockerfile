FROM python:3.12-slim-bookworm@sha256:34386ef0cb081344d7ec1c103ba398e6e9f64e9ab3a1509accc92a4e24a07258 AS dependencies
COPY requirements.txt /requirements.txt
RUN pip install --no-cache-dir --no-compile --require-hashes --target /site -r /requirements.txt

FROM --platform=$BUILDPLATFORM golang:1.26.7-bookworm@sha256:e8c859f5632dcfde7b32d2012b4351728f6437930887c2f6a91ea242459e5514 AS tools
ARG TARGETOS
ARG TARGETARCH
ADD --checksum=sha256:8b00b127eb567f30b028cb032d236d990404e9fd83ce7798db7f9c7a305fab34 \
    https://github.com/kubernetes-sigs/kind/archive/refs/tags/v0.33.0.tar.gz /kind.tar.gz
RUN mkdir /src /out && tar -xf /kind.tar.gz -C /src \
    && cd /src/kind-0.33.0 \
    && CGO_ENABLED=0 GOOS="$TARGETOS" GOARCH="$TARGETARCH" GOTOOLCHAIN=local \
        go build -trimpath -buildmode=pie -ldflags='-s -w' -o /out/kind .
ADD --checksum=sha256:b45d935ab8f5faff8b0a03eb6c5c658636f47bc8170d892152b8eb57f5931315 \
    https://github.com/docker/cli/archive/refs/tags/v29.4.1.tar.gz /docker-cli.tar.gz
RUN mkdir -p /go/src/github.com/docker/cli \
    && tar -xf /docker-cli.tar.gz --strip-components=1 -C /go/src/github.com/docker/cli \
    && cd /go/src/github.com/docker/cli \
    && CGO_ENABLED=0 GOOS="$TARGETOS" GOARCH="$TARGETARCH" GO111MODULE=off GOTOOLCHAIN=local \
        go build -trimpath -buildmode=pie \
        -ldflags='-s -w -X github.com/docker/cli/cli/version.Version=29.4.1' -o /out/docker ./cmd/docker

FROM ghcr.io/hyperlight-dev/hyperlight-unikraft/python-shell@sha256:e2f65a42a7db22ed8721e29b1146f2e60a4da8ad0e7bc43f0038a922e81373c0 AS rootfs
COPY --from=dependencies /site/ /usr/local/lib/python3.12/site-packages/
COPY --from=tools /out/kind /out/docker /usr/local/bin/
COPY templates/ /templates/
COPY --from=common process.py /usr/local/lib/python3.12/site-packages/process.py
