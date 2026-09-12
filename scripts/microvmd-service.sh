#!/bin/sh
set -eu

self_cgroup=$(awk -F: '$1 == "0" { print $3; exit }' /proc/self/cgroup)
case "$self_cgroup" in
    */main) parent_cgroup=${self_cgroup%/main} ;;
    "") echo "no cgroup v2 path for luhmen-microvmd" >&2; exit 1 ;;
    *)
        parent_cgroup=$self_cgroup
        mkdir -p "/sys/fs/cgroup/$parent_cgroup/main"
        printf '%s\n' "$$" > "/sys/fs/cgroup/$parent_cgroup/main/cgroup.procs"
        ;;
esac

[ -n "$parent_cgroup" ] || { echo "no delegated cgroup parent for luhmen-microvmd" >&2; exit 1; }
printf '%s\n' '+cpu +memory +pids' > "/sys/fs/cgroup/$parent_cgroup/cgroup.subtree_control"
exec /usr/bin/socat UNIX-LISTEN:/run/luhmen/microvmd.sock,fork,unlink-early,mode=0666 EXEC:/usr/local/libexec/luhmen-microvmd
