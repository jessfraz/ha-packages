#!/bin/sh
set -eu

# Native restores copy private data as root while retaining restrictive modes.
# Repair only this volume before dropping privileges; never traverse its links
# or a nested mount into somebody else's storage.
if [ "$(id -u)" -eq 0 ]; then
    if [ -L /data ] || [ ! -d /data ]; then
        echo 'Private data must be a real directory' >&2
        exit 1
    fi
    if ! awk '$5 ~ /^\/data\// { exit 1 }' /proc/self/mountinfo; then
        echo 'Private data mount boundary is unavailable or contains nested mounts' >&2
        exit 1
    fi
    find -P /data -xdev -exec chown --no-dereference packages:packages -- {} +
    exec gosu packages "$0" "$@"
fi

umask 077
if [ -f /data/options.json ]; then
    set -- --options /data/options.json "$@"
fi
if [ "${PACKAGES_ENABLE_INGRESS:-false}" = true ]; then
    set -- --ingress-bind 0.0.0.0:8099 --ingress-peer 172.30.32.2 "$@"
fi

exec packages "$@"
