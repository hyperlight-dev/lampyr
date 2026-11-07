FROM scratch
COPY kernel /unikernel/kernel
COPY snapshot /unikernel/snapshot
COPY metadata.json /artifact/
ENTRYPOINT ["/app/server.js"]
