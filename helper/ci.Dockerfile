# The helper image fiberd-k8s-controller's helperImage names: the grant
# Pod's init container copies /fiberd-helper/helper into a shared emptyDir,
# and fiberd runs it with -hyperlight-helper. There is no
# /fiberd-helper/guest, because each template's snapshot carries its guest
# kernel. The base is fiberd's proc base, so the init container has a shell
# and cp, and the binary runs against the glibc it will run on.
FROM debian:bookworm-slim@sha256:99c0b25c46d0214d5a2091092e7908907dcaecd6a962cde0ac700acee222b6f3

COPY --chmod=0755 build/hluk-helper /fiberd-helper/helper
