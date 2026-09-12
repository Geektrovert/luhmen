#!/bin/sh
# Record the configuration used when systemd starts each Docker unit. /run is
# cleared at boot, so a previous boot cannot certify the current daemon's state.
set -eu
umask 077

action=${1:?missing action}
unit=${2:?missing unit}
case "$unit" in service|socket) ;; *) exit 2 ;; esac
state=/run/luhmen-docker

signature() {
    unit_hash=$(sha256sum "/etc/systemd/system/docker.$unit" "$0")
    if [ "$unit" = service ]; then
        binary=$(readlink -f /usr/local/bin/dockerd)
        config_hash=$(sha256sum "${binary%/*}/.complete" /etc/docker/daemon.json)
        environment_hash=missing
        if [ -f /etc/environment ]; then
            environment_hash=$(sha256sum /etc/environment)
        fi
        printf '%s\n' "$unit_hash" "$binary" "$config_hash" "$environment_hash" | sha256sum
    else
        printf '%s\n' "$unit_hash" | sha256sum
    fi
}

case "$action" in
signature)
    signature
    ;;
prepare)
    install -d -m 0700 "$state"
    rm -f "$state/$unit.active"
    value=$(signature)
    temporary=$(mktemp "$state/$unit.XXXXXX")
    trap 'rm -f "$temporary"' EXIT
    trap 'exit 1' HUP INT TERM
    printf '%s\n' "$value" > "$temporary"
    mv "$temporary" "$state/$unit.pending"
    ;;
commit)
    mv "$state/$unit.pending" "$state/$unit.active"
    ;;
clear)
    rm -f "$state/$unit.pending" "$state/$unit.active"
    ;;
*) exit 2 ;;
esac
