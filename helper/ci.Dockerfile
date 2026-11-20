# The helper image fiberd-k8s-controller's helperImage names: the grant
# Pod's init container copies /fiberd-helper/helper into a shared emptyDir,
# and fiberd runs it with -hyperlight-helper. There is no
# /fiberd-helper/guest, because each template's snapshot carries its guest
# kernel. The base is fiberd's proc base, so the init container has a shell
# and cp, and the binary runs against the glibc it will run on.
FROM debian:bookworm-slim@sha256:734a72b3d6142a71993b4bc580ca01d8ca687f77a2c422d96a961d3376b179bc

COPY --chmod=0755 build/hluk-helper /fiberd-helper/helper
