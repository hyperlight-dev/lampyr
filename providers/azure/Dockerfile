ARG TARGETARCH

FROM python:3.12-slim-bookworm@sha256:34386ef0cb081344d7ec1c103ba398e6e9f64e9ab3a1509accc92a4e24a07258 AS dependencies
COPY requirements.txt /requirements.txt
RUN pip install --no-cache-dir --no-compile --require-hashes --target /site -r /requirements.txt && \
    rm -rf /site/psutil /site/psutil-*.dist-info && \
    python3 -m compileall -q -j1 /site > /dev/null

FROM alpine:3.20@sha256:d9e853e87e55526f6b2917df91a2115c36dd7c696a35be12163d44e6e2a4b6bc AS launcher
RUN apk add --no-cache gcc musl-dev libssl3 libcrypto3 zlib
COPY az.c /src/az.c
RUN gcc -O2 -Wall -Wextra -static-pie -fPIE -o /az /src/az.c && \
    mkdir -p /runtime/bin /runtime/lib /runtime/usr/lib && \
    cp /az /runtime/bin/az && \
    cp -L /lib/ld-musl-*.so.1 /runtime/lib/ && \
    cp -L /usr/lib/libstdc++.so.6 /usr/lib/libgcc_s.so.1 /runtime/usr/lib/ && \
    cp -L /lib/libssl.so.3 /lib/libcrypto.so.3 /lib/libz.so.1 /runtime/usr/lib/

FROM alpine:3.20@sha256:d9e853e87e55526f6b2917df91a2115c36dd7c696a35be12163d44e6e2a4b6bc AS bicep-amd64
ADD --chmod=755 --checksum=sha256:7507ce8dd0c17d3e7fe36e6a5e7591a49f3d665e4371d852122f1550d2558ecf \
    https://github.com/Azure/bicep/releases/download/v0.39.26/bicep-linux-musl-x64 /runtime/usr/local/bin/bicep

FROM python:3.12-slim-bookworm@sha256:34386ef0cb081344d7ec1c103ba398e6e9f64e9ab3a1509accc92a4e24a07258 AS bicep-arm64
RUN apt-get update -q && \
    apt-get install -y --no-install-recommends libstdc++6 zlib1g && \
    rm -rf /var/lib/apt/lists/*
ADD --chmod=755 --checksum=sha256:e4871dd4f18c48f0729ac30524d52192a98a8bf861a9a93a8e8af0b62b0d2ba7 \
    https://github.com/Azure/bicep/releases/download/v0.39.26/bicep-linux-arm64 /bicep
RUN mkdir -p /runtime/lib /runtime/usr/local/bin && \
    cp /bicep /runtime/usr/local/bin/bicep && \
    cp -L /lib/aarch64-linux-gnu/ld-linux-aarch64.so.1 /runtime/lib/ld-linux-aarch64.so.1 && \
    ldd /bicep | awk '/=> \// { print $3 } /^\// { print $1 }' | while read -r library; do \
        destination="/runtime$library"; \
        mkdir -p "$(dirname "$destination")"; \
        cp -L "$library" "$destination"; \
    done

FROM bicep-${TARGETARCH} AS bicep

FROM ghcr.io/hyperlight-dev/hyperlight-unikraft/python-shell@sha256:e2f65a42a7db22ed8721e29b1146f2e60a4da8ad0e7bc43f0038a922e81373c0 AS rootfs
COPY --from=dependencies /site/ /usr/local/lib/python3.12/site-packages/
COPY eventgrid/lampyr_eventgrid.py /usr/local/lib/python3.12/site-packages/lampyr_eventgrid.py
COPY eventgrid/eventgrid-source.yaml /templates/eventgrid-source.yaml
COPY --from=launcher /runtime/ /
COPY --from=bicep /runtime/ /
ENV AZURE_CORE_COLLECT_TELEMETRY=no \
    AZURE_EXTENSION_USE_DYNAMIC_INSTALL=no \
    AZURE_BICEP_USE_BINARY_FROM_PATH=true \
    DOTNET_SYSTEM_GLOBALIZATION_INVARIANT=1 \
    DOTNET_EnableWriteXorExecute=0 \
    COMPlus_EnableDiagnostics=0 \
    DOTNET_GCHeapHardLimitPercent=0x32 \
    DOTNET_gcServer=0 \
    DOTNET_gcConcurrent=0 \
    DOTNET_DefaultStackSize=0x40000 \
    DOTNET_ThreadPool_ForceMinWorkerThreads=1 \
    DOTNET_ThreadPool_ForceMaxWorkerThreads=4

COPY templates/ /templates/

FROM launcher AS probes
COPY tests/stack.c /src/stack.c
COPY tests/mremap.c /src/mremap.c
COPY tests/memory.c /src/memory.c
RUN gcc -O2 -Wall -Wextra -Werror -static-pie -fPIE -pthread -o /stack /src/stack.c && \
    gcc -O2 -Wall -Wextra -Werror -static-pie -fPIE -o /mremap /src/mremap.c && \
    gcc -O2 -Wall -Wextra -Werror -static-pie -fPIE -o /memory /src/memory.c

FROM rootfs AS proof
COPY tests/main.bicep /proof/main.bicep
COPY --from=probes /stack /proof/stack
COPY --from=probes /mremap /proof/mremap
COPY --from=probes /memory /proof/memory
