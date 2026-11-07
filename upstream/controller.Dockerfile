# The fiberd-k8s-controller image, which Lampyr publishes until the
# controller has a Dockerfile and release of its own
# (Azure/fiberd-k8s-controller#26). just
# upstream::prepare-controller-image builds the manager from the commit
# upstream/pins.env pins; the image's description names it. It runs as
# the nonroot user, 65532, which the controller's Deployment requires, and
# its arguments come from that Deployment.
FROM gcr.io/distroless/static-debian12:nonroot@sha256:afa5c872c891853ca7fcf1f12c3edb23f7eeef36189728842dd51042ff57f7ab

COPY build/fiberd-k8s-controller /fiberd-k8s-controller
USER 65532:65532

ENTRYPOINT ["/fiberd-k8s-controller"]
