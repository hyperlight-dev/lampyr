FROM scratch
COPY build/fiber-router /usr/local/bin/fiber-router
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/fiber-router"]
