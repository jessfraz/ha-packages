#!/bin/sh
set -eu

# Supervisor creates /data and options.json as root. Only startup needs root;
# the server and worker run as the fixed application user.
if [ "$(id -u)" -eq 0 ]; then
    chown shipments:shipments /data
    if [ -f /data/options.json ]; then
        chown shipments:shipments /data/options.json
    fi
    exec gosu shipments "$0" "$@"
fi

umask 077
if [ -f /data/options.json ]; then
    set -- --options /data/options.json "$@"
fi
if [ "${SHIPMENTS_ENABLE_INGRESS:-false}" = true ]; then
    set -- --ingress-bind 0.0.0.0:8099 --ingress-peer 172.30.32.2 "$@"
fi

exec shipments "$@"
