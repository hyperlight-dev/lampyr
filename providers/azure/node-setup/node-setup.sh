#!/bin/sh
set -eu

action=${1:-}
host_root=${HOST_ROOT:-/host}
chroot_command=${CHROOT:-chroot}

case "$action" in
  install)
    if [ -c "$host_root/dev/kvm" ]; then
      exit 0
    fi

    if ! "$chroot_command" "$host_root" /bin/sh -c \
      'modprobe kvm && (modprobe kvm_intel || modprobe kvm_amd)'; then
      echo "lampyr: could not load the node KVM modules" >&2
      exit 1
    fi

    if [ ! -c "$host_root/dev/kvm" ]; then
      echo "lampyr: /dev/kvm is unavailable; use a node size with nested virtualization" >&2
      exit 1
    fi
    ;;
  *)
    echo "usage: lampyr-node-setup install" >&2
    exit 2
    ;;
esac
