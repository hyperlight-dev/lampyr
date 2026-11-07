# syntax=docker/dockerfile:1
FROM scratch

WORKDIR /

COPY kernel /unikernel/kernel
COPY snapshot /unikernel/snapshot
COPY metadata.json /artifact/

ENTRYPOINT ["/entrypoint.py"]
