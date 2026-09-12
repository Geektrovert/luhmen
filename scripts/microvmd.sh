#!/bin/sh
set -eu
set -f

BASE=/var/lib/luhmen/microvms
JAILER_BASE=/var/lib/luhmen/microvm-jails
FIRECRACKER=/usr/local/bin/firecracker
JAILER=/usr/local/bin/jailer
LOCK=/run/luhmen/microvmd.lock
MICROVM_UID_BASE=60000
MICROVM_UID_LIMIT=60999
MEMORY_RESERVE_MIB=512
VMM_MEMORY_MIB=128
CGROUP_PID_LIMIT=512
LOG_LIMIT_BYTES=67108864
id=""
started_pid=""
started_start_time=""
started_jail=""
started_cgroup=""
started_logger=""
started_overlay=""
started_log_pipe=""

mkdir -p "$BASE" "$JAILER_BASE"
chmod 0700 "$BASE" "$JAILER_BASE"

error() {
    cleanup_start
    printf '{"version":1,"ok":false,"error":"%s"}\n' "$1"
    exit 0
}

ok() {
    printf '{"version":1,"ok":true,"data":%s}\n' "$1"
    exit 0
}

valid_id() {
    [ -n "$1" ] || return 1
    [ "$1" != . ] && [ "$1" != .. ] || return 1
    [ "${#1}" -le 64 ] || return 1
    case "$1" in *[!A-Za-z0-9_.-]*) return 1;; esac
}

valid_path() {
    case "$1" in /*) ;; *) return 1;; esac
    case "$1" in *[[:space:]]*|*..*) return 1;; esac
}

number() {
    case "$1" in ""|*[!0-9]*) return 1;; esac
}

list_vms() {
    find "$BASE" -mindepth 1 -maxdepth 1 -type d -print
}

state_value() {
    sed -n "s/^$1=//p" "$2" | head -n 1
}

alive() {
    [ -n "$1" ] && [ -d "/proc/$1" ]
}

matches_firecracker() {
    alive "$1" || return 1
    expected_start_time=${2-}
    if [ -n "$expected_start_time" ]; then
        actual_start_time=$(awk '{print $22}' "/proc/$1/stat" 2>/dev/null || true)
        [ "$actual_start_time" = "$expected_start_time" ] || return 1
    fi
    [ -r "/proc/$1/cmdline" ] || return 1
    cmdline=$(tr '\000' ' ' < "/proc/$1/cmdline" 2>/dev/null || true)
    case "$cmdline" in *firecracker*"/run/fc.sock"*) return 0;; esac
    return 1
}

matches_started_process() {
    alive "$1" || return 1
    expected_start_time=${2-}
    if [ -n "$expected_start_time" ]; then
        actual_start_time=$(awk '{print $22}' "/proc/$1/stat" 2>/dev/null || true)
        [ "$actual_start_time" = "$expected_start_time" ] || return 1
    fi
    [ -r "/proc/$1/cmdline" ] || return 1
    cmdline=$(tr '\000' ' ' < "/proc/$1/cmdline" 2>/dev/null || true)
    case "$cmdline" in *firecracker*|*jailer*) return 0;; esac
    return 1
}

parent_cgroup() {
    cgroup_path=$(awk -F: '$1 == "0" { print $3; exit }' /proc/self/cgroup)
    case "$cgroup_path" in */main) cgroup_path=${cgroup_path%/main};; esac
    printf '%s\n' "${cgroup_path#/}"
}

process_cgroup() {
    awk -F: '$1 == "0" { print $3; exit }' "/proc/$1/cgroup"
}

valid_parent_cgroup() {
    [ -n "$1" ] || return 1
    case "$1" in
        /*|*..*|*[!A-Za-z0-9_./-]*) return 1
        ;;
    esac
}

terminate_cgroup() {
    cgroup_dir=$1
    [ -n "$cgroup_dir" ] && [ -f "$cgroup_dir/cgroup.procs" ] || return 0
    for signal in TERM KILL; do
        cgroup_pids=$(cat "$cgroup_dir/cgroup.procs" 2>/dev/null || true)
        for cgroup_pid in $cgroup_pids; do
            [ "$cgroup_pid" = "$$" ] && continue
            kill -"$signal" "$cgroup_pid" 2>/dev/null || true
        done
        [ "$signal" = TERM ] || break
        for attempt in $(seq 1 20); do
            [ -n "$(cat "$cgroup_dir/cgroup.procs" 2>/dev/null || true)" ] || break
            sleep 0.1
        done
    done
    # cgroup files report size zero even when they contain PIDs. The final
    # process can also take a moment to leave after SIGKILL.
    for attempt in $(seq 1 20); do
        rmdir "$cgroup_dir" 2>/dev/null && return 0
        [ -d "$cgroup_dir" ] || return 0
        sleep 0.05
    done
    return 1
}

host_cpu_count() {
    if command -v nproc >/dev/null 2>&1; then
        nproc
    else
        getconf _NPROCESSORS_ONLN
    fi
}

host_memory_mib() {
    awk '/^MemTotal:/ { printf "%d\n", $2 / 1024; exit }' /proc/meminfo
}

running_resources() {
    used_vcpus=0
    used_memory_mib=0
    for candidate in $(list_vms); do
        candidate_state="$candidate/state"
        [ -f "$candidate_state" ] && [ ! -L "$candidate_state" ] || continue
        candidate_status=$(state_value status "$candidate_state" || true)
        [ "$candidate_status" = running ] || continue
        candidate_pid=$(state_value pid "$candidate_state" || true)
        candidate_start_time=$(state_value start_time "$candidate_state" || true)
        matches_firecracker "$candidate_pid" "$candidate_start_time" || continue
        candidate_vcpus=$(state_value vcpus "$candidate_state" || true)
        candidate_memory_mib=$(state_value memory_mib "$candidate_state" || true)
        number "$candidate_vcpus" && number "$candidate_memory_mib" || continue
        used_vcpus=$((used_vcpus + candidate_vcpus))
        used_memory_mib=$((used_memory_mib + candidate_memory_mib + VMM_MEMORY_MIB))
    done
}

resource_capacity() {
    parent_cpus=$(host_cpu_count 2>/dev/null || true)
    parent_memory_mib=$(host_memory_mib 2>/dev/null || true)
    number "$parent_cpus" && [ "$parent_cpus" -ge 1 ] || return 1
    number "$parent_memory_mib" && [ "$parent_memory_mib" -ge 1 ] || return 1
    running_resources
    available_vcpus=$((parent_cpus - used_vcpus))
    available_memory_mib=$((parent_memory_mib - MEMORY_RESERVE_MIB - used_memory_mib))
}

check_start_resources() {
    resource_capacity || error "could not determine parent VM resources"
    [ "$vcpus" -le "$available_vcpus" ] || error "microVM vcpus exceed available parent CPU capacity"
    [ "$((memory_mib + VMM_MEMORY_MIB))" -le "$available_memory_mib" ] || error "microVM memory would exceed available parent memory"
}

identity_in_use() {
    candidate_uid=$1
    if command -v getent >/dev/null 2>&1; then
        getent passwd "$candidate_uid" >/dev/null 2>&1 && return 0
        getent group "$candidate_uid" >/dev/null 2>&1 && return 0
    fi
    awk -F: -v uid="$candidate_uid" '$3 == uid { found=1 } END { exit !found }' /etc/passwd && return 0
    awk -F: -v gid="$candidate_uid" '$3 == gid { found=1 } END { exit !found }' /etc/group && return 0
    for candidate_vm in $(list_vms); do
        candidate_state="$candidate_vm/state"
        [ -f "$candidate_state" ] && [ ! -L "$candidate_state" ] || continue
        [ "$(state_value uid "$candidate_state" || true)" = "$candidate_uid" ] && return 0
    done
    return 1
}

allocate_identity() {
    for candidate_uid in $(seq "$MICROVM_UID_BASE" "$MICROVM_UID_LIMIT"); do
        identity_in_use "$candidate_uid" || {
            printf '%s\n' "$candidate_uid"
            return 0
        }
    done
    return 1
}

preflight() {
    [ -r /dev/kvm ] && [ -w /dev/kvm ] || error "KVM unavailable: /dev/kvm must be readable and writable"
    [ -f /sys/fs/cgroup/cgroup.controllers ] || error "cgroup v2 unavailable: /sys/fs/cgroup is not a cgroup v2 hierarchy"
    [ -x "$FIRECRACKER" ] || error "Firecracker binary is unavailable"
    [ -x "$JAILER" ] || error "Firecracker jailer is unavailable"
    cgroup_parent=$(parent_cgroup || true)
    valid_parent_cgroup "$cgroup_parent" || error "delegated cgroup parent is unavailable"
    cgroup_parent_dir="/sys/fs/cgroup/$cgroup_parent"
    [ -d "$cgroup_parent_dir" ] || error "delegated cgroup parent directory is unavailable"
    for controller in cpu memory pids; do
        grep -qw "$controller" "$cgroup_parent_dir/cgroup.subtree_control" 2>/dev/null || error "delegated cgroup controller is unavailable: $controller"
    done
}

cleanup_start() {
    if [ -n "$started_pid" ] && matches_started_process "$started_pid" "$started_start_time"; then
        kill -TERM "$started_pid" 2>/dev/null || true
        for attempt in $(seq 1 20); do
            alive "$started_pid" || break
            sleep 0.1
        done
        if alive "$started_pid" && matches_started_process "$started_pid" "$started_start_time"; then
            kill -KILL "$started_pid" 2>/dev/null || true
        fi
    fi
    terminate_cgroup "$started_cgroup" || true
    if [ -n "$started_logger" ]; then kill -TERM "$started_logger" 2>/dev/null || true; fi
    if [ -n "$started_overlay" ]; then rm -f "$started_overlay"; fi
    if [ -n "$started_log_pipe" ]; then rm -f "$started_log_pipe"; fi
    if [ -n "$started_jail" ] && [ "$started_jail" = "$JAILER_BASE/firecracker/$id" ]; then rm -rf "$started_jail"; fi
    started_pid=""
    started_start_time=""
    started_jail=""
    started_cgroup=""
    started_logger=""
    started_overlay=""
    started_log_pipe=""
}

on_exit() {
    exit_status=$?
    cleanup_start
    if [ "$exit_status" -ne 0 ]; then
        printf '{"version":1,"ok":false,"error":"microVM manager operation failed"}\n'
    fi
}

trap on_exit 0
trap 'cleanup_start; exit 129' 1
trap 'cleanup_start; exit 130' 2
trap 'cleanup_start; exit 143' 15

write_state() {
    vm=$1
    shift
    tmp="$vm/.state.$$"
    printf '%s\n' "$@" > "$tmp"
    chmod 0600 "$tmp"
    mv "$tmp" "$vm/state"
}

json_vm() {
    vm=$1
    state="$vm/state"
    id=${vm##*/}
    status=$(state_value status "$state")
    pid=$(state_value pid "$state" || true)
    start_time=$(state_value start_time "$state" || true)
    uid=$(state_value uid "$state" || true)
    gid=$(state_value gid "$state" || true)
    if [ "$status" = running ] && ! matches_firecracker "$pid" "$start_time"; then
        status=stale
    fi
    [ -n "$pid" ] || pid=null
    [ -n "$uid" ] || uid=null
    [ -n "$gid" ] || gid=null
    printf '{"id":"%s","status":"%s","pid":%s,"uid":%s,"gid":%s,"vcpus":%s,"memory_mib":%s}\n' \
        "$id" "$status" "$pid" "$uid" "$gid" "$(state_value vcpus "$state")" "$(state_value memory_mib "$state")"
}

request=""
IFS= read -r request || exit 0
[ "${#request}" -le 4096 ] || error "microVM manager request is too large"
id=""
kernel=""
rootfs=""
vcpus=1
memory_mib=512
force=0

set -- $request
[ "$#" -gt 0 ] || error "action is required"
action=$1
shift
case "$action" in capabilities|inspect|create|start|stop) ;; *) error "unknown action";; esac
seen_fields=" "
for token do
    case "$token" in *=*) ;; *) error "request fields must be key=value pairs";; esac
    key=${token%%=*}
    value_part=${token#*=}
    [ -n "$value_part" ] || error "request field cannot be empty"
    case "$seen_fields" in *" $key "*) error "duplicate request field";; esac
    seen_fields="$seen_fields$key "
    case "$action:$key" in
        inspect:id|create:id|start:id|stop:id) valid_id "$value_part" || error "invalid microVM id"; id=$value_part ;;
        create:kernel) kernel=$value_part ;;
        create:rootfs) rootfs=$value_part ;;
        create:vcpus) vcpus=$value_part ;;
        create:memory_mib) memory_mib=$value_part ;;
        stop:force) force=$value_part ;;
        *) error "unknown request field" ;;
    esac
done

exec 9>"$LOCK"
flock -x -w 5 9 || error "microVM manager is busy; retry the request"

case "$action" in
capabilities)
    kvm=false
    tun=false
    cgroup_v2=false
    kvm_error=null
    tun_error=null
    cgroup_v2_error=null
    delegated_cgroup=false
    delegated_cgroup_error=null
    if [ -r /dev/kvm ] && [ -w /dev/kvm ]; then kvm=true; else kvm_error='"/dev/kvm is missing or not readable and writable"'; fi
    if [ -c /dev/net/tun ]; then tun=true; else tun_error='"/dev/net/tun is unavailable"'; fi
    if [ -f /sys/fs/cgroup/cgroup.controllers ]; then cgroup_v2=true; else cgroup_v2_error='"cgroup v2 is unavailable"'; fi
    capability_cgroup_parent=$(parent_cgroup || true)
    if ! valid_parent_cgroup "$capability_cgroup_parent"; then
        delegated_cgroup_error='"delegated cgroup parent is unavailable"'
    elif [ ! -d "/sys/fs/cgroup/$capability_cgroup_parent" ]; then
        delegated_cgroup_error='"delegated cgroup parent directory is unavailable"'
    else
        delegated_cgroup=true
        for controller in cpu memory pids; do
            if ! grep -qw "$controller" "/sys/fs/cgroup/$capability_cgroup_parent/cgroup.subtree_control" 2>/dev/null; then
                delegated_cgroup=false
                delegated_cgroup_error='"delegated cgroup v2 controllers are unavailable"'
                break
            fi
        done
    fi
    firecracker=false
    jailer=false
    firecracker_error=null
    jailer_error=null
    if [ -x "$FIRECRACKER" ]; then firecracker=true; else firecracker_error='"Firecracker binary is unavailable"'; fi
    if [ -x "$JAILER" ]; then jailer=true; else jailer_error='"Firecracker jailer is unavailable"'; fi
    resources_ready=false
    if resource_capacity; then
        parent_cpus_json=$parent_cpus
        parent_memory_mib_json=$parent_memory_mib
        used_vcpus_json=$used_vcpus
        used_memory_mib_json=$used_memory_mib
        available_vcpus_json=$available_vcpus
        available_memory_mib_json=$available_memory_mib
        [ "$available_vcpus" -ge 1 ] && [ "$available_memory_mib" -ge "$((128 + VMM_MEMORY_MIB))" ] && resources_ready=true
    else
        parent_cpus_json=null
        parent_memory_mib_json=null
        used_vcpus_json=null
        used_memory_mib_json=null
        available_vcpus_json=null
        available_memory_mib_json=null
    fi
    ready=false
    [ "$kvm" = true ] && [ "$cgroup_v2" = true ] && [ "$delegated_cgroup" = true ] && [ "$firecracker" = true ] && [ "$jailer" = true ] && [ "$resources_ready" = true ] && ready=true
    ok "{\"ready\":$ready,\"kvm\":$kvm,\"kvm_error\":$kvm_error,\"tun\":$tun,\"tun_error\":$tun_error,\"cgroup_v2\":$cgroup_v2,\"cgroup_v2_error\":$cgroup_v2_error,\"delegated_cgroup\":$delegated_cgroup,\"delegated_cgroup_error\":$delegated_cgroup_error,\"firecracker\":$firecracker,\"firecracker_error\":$firecracker_error,\"jailer\":$jailer,\"jailer_error\":$jailer_error,\"parent_cpus\":$parent_cpus_json,\"parent_memory_mib\":$parent_memory_mib_json,\"used_vcpus\":$used_vcpus_json,\"used_memory_mib\":$used_memory_mib_json,\"available_vcpus\":$available_vcpus_json,\"available_memory_mib\":$available_memory_mib_json,\"resources_ready\":$resources_ready,\"networking\":false,\"guest_agent\":false}"
    ;;
create)
    valid_id "$id" || error "invalid microVM id"
    valid_path "$kernel" || error "kernel must be an absolute path without traversal"
    valid_path "$rootfs" || error "rootfs must be an absolute path without traversal"
    number "$vcpus" && [ "$vcpus" -ge 1 ] && [ "$vcpus" -le 16 ] || error "vcpus must be between 1 and 16"
    number "$memory_mib" && [ "$memory_mib" -ge 128 ] && [ "$memory_mib" -le 16384 ] || error "memory_mib must be between 128 and 16384"
    [ -f "$kernel" ] && [ ! -L "$kernel" ] || error "kernel must be a regular non-symlink file"
    [ -f "$rootfs" ] && [ ! -L "$rootfs" ] || error "rootfs must be a regular non-symlink file"
    uid=$(allocate_identity) || error "no free per-microVM jailer identity is available"
    gid=$uid
    vm="$BASE/$id"
    [ ! -e "$vm" ] && [ ! -L "$vm" ] || error "microVM already exists"
    mkdir -m 0700 "$vm"
    write_state "$vm" "status=created" "kernel=$kernel" "rootfs=$rootfs" "vcpus=$vcpus" "memory_mib=$memory_mib" "uid=$uid" "gid=$gid"
    ok "$(json_vm "$vm")"
    ;;
start)
    valid_id "$id" || error "invalid microVM id"
    vm="$BASE/$id"
    [ -d "$vm" ] && [ ! -L "$vm" ] || error "microVM does not exist"
    state="$vm/state"
    [ -f "$state" ] && [ ! -L "$state" ] || error "microVM state is missing"
    old_status=$(state_value status "$state")
    old_pid=$(state_value pid "$state" || true)
    old_start_time=$(state_value start_time "$state" || true)
    if [ "$old_status" = running ] && matches_firecracker "$old_pid" "$old_start_time"; then
        ok "$(json_vm "$vm")"
    fi
    preflight
    kernel=$(state_value kernel "$state")
    rootfs=$(state_value rootfs "$state")
    vcpus=$(state_value vcpus "$state")
    memory_mib=$(state_value memory_mib "$state")
    number "$vcpus" && [ "$vcpus" -ge 1 ] && [ "$vcpus" -le 16 ] || error "stored vcpus value is invalid"
    number "$memory_mib" && [ "$memory_mib" -ge 128 ] && [ "$memory_mib" -le 16384 ] || error "stored memory_mib value is invalid"
    uid=$(state_value uid "$state" || true)
    gid=$(state_value gid "$state" || true)
    if ! number "$uid" || ! number "$gid" || [ "$uid" != "$gid" ]; then
        uid=$(allocate_identity) || error "no free per-microVM jailer identity is available"
        gid=$uid
        write_state "$vm" "status=$old_status" "kernel=$kernel" "rootfs=$rootfs" "vcpus=$vcpus" "memory_mib=$memory_mib" "uid=$uid" "gid=$gid"
        state="$vm/state"
    fi
    [ -f "$kernel" ] && [ ! -L "$kernel" ] || error "configured kernel is unavailable"
    [ -f "$rootfs" ] && [ ! -L "$rootfs" ] || error "configured rootfs is unavailable"
    check_start_resources
    overlay="$vm/rootfs.ext4"
    if [ ! -e "$overlay" ] && [ ! -L "$overlay" ]; then
        started_overlay="$vm/.rootfs.$$"
        cp --reflink=auto "$rootfs" "$started_overlay" || error "could not copy the initial rootfs"
        mv "$started_overlay" "$overlay"
        started_overlay=""
    fi
    [ -f "$overlay" ] && [ ! -L "$overlay" ] || error "microVM overlay must be a regular file"
    jail="$JAILER_BASE/firecracker/$id"
    rm -rf "$jail"
    mkdir -p "$jail/root/run"
    started_jail=$jail
    cp "$kernel" "$jail/root/vmlinux" || error "could not copy the guest kernel"
    # Link the private writable disk before the unprivileged VMM can touch the jail.
    # Both directories live on the same guest filesystem. Removing a jail leaves
    # the VM's disk inode and all guest writes intact.
    ln "$overlay" "$jail/root/rootfs.ext4" || error "persistent disk and jail must share a filesystem"
    chown "$uid:$gid" "$jail/root/run" "$jail/root/vmlinux" "$overlay"
    chmod 0700 "$jail/root/run"
    chmod 0400 "$jail/root/vmlinux"
    chmod 0600 "$overlay"
    started_cgroup="/sys/fs/cgroup/$cgroup_parent/firecracker/$id"
    log="$vm/firecracker.log"
    if [ -e "$log" ] || [ -L "$log" ]; then
        [ -f "$log" ] && [ ! -L "$log" ] || error "microVM log must be a regular file"
    fi
    : > "$log"
    log_pipe="$vm/log.pipe"
    rm -f "$log_pipe"
    mkfifo -m 0600 "$log_pipe"
    started_log_pipe=$log_pipe
    # Retain only the first 64 MiB of VMM output while continuing to drain the FIFO.
    (stdbuf -o0 head -c "$LOG_LIMIT_BYTES"; cat >/dev/null) < "$log_pipe" > "$log" 2>&1 9>&- &
    started_logger=$!
    "$JAILER" --id "$id" --exec-file "$FIRECRACKER" --uid "$uid" --gid "$gid" \
        --cgroup-version 2 --chroot-base-dir "$JAILER_BASE" \
        --parent-cgroup "$cgroup_parent/firecracker" \
        --cgroup "cpu.max=$((vcpus * 100000)) 100000" \
        --cgroup "memory.max=$(((memory_mib + VMM_MEMORY_MIB) * 1024 * 1024))" \
        --cgroup "memory.swap.max=0" \
        --cgroup "pids.max=$CGROUP_PID_LIMIT" \
        -- --api-sock /run/fc.sock \
        < /dev/null > "$log_pipe" 2>&1 9>&- &
    pid=$!
    started_pid=$pid
    started_start_time=$(awk '{print $22}' "/proc/$pid/stat" 2>/dev/null || true)
    [ -n "$started_start_time" ] || error "could not identify the jailed VMM process"
    # The child begins in the service cgroup. Wait for jailer to move it before
    # validating limits, rather than racing its first scheduling opportunity.
    for attempt in $(seq 1 100); do
        process_cgroup_rel=$(process_cgroup "$pid" 2>/dev/null || true)
        [ "$process_cgroup_rel" = "/$cgroup_parent/firecracker/$id" ] && break
        matches_started_process "$pid" "$started_start_time" || error "jailer exited before entering its cgroup"
        sleep 0.1
    done
    [ "$process_cgroup_rel" = "/$cgroup_parent/firecracker/$id" ] || error "jailer did not create the expected per-VM cgroup"
    [ -f "$started_cgroup/memory.max" ] && [ -f "$started_cgroup/memory.swap.max" ] && [ -f "$started_cgroup/cpu.max" ] && [ -f "$started_cgroup/pids.max" ] || error "per-VM cgroup limits are unavailable"
    socket="$jail/root/run/fc.sock"
    for attempt in $(seq 1 100); do
        [ -S "$socket" ] && break
        matches_firecracker "$pid" "$started_start_time" || error "Firecracker exited before opening its API socket"
        sleep 0.1
    done
    [ -S "$socket" ] || error "Firecracker API socket did not appear"
    api() {
        curl --fail --silent --show-error --max-time 10 --unix-socket "$socket" \
            -X PUT -H 'Content-Type: application/json' -d "$2" "http://localhost$1" >/dev/null || error "Firecracker rejected the startup configuration"
    }
    api /boot-source "{\"kernel_image_path\":\"/vmlinux\",\"boot_args\":\"keep_bootcon console=ttyS0 reboot=k panic=1\"}"
    api /drives/rootfs "{\"drive_id\":\"rootfs\",\"path_on_host\":\"/rootfs.ext4\",\"is_root_device\":true,\"is_read_only\":false}"
    api /machine-config "{\"vcpu_count\":$vcpus,\"mem_size_mib\":$memory_mib}"
    api /actions '{"action_type":"InstanceStart"}'
    write_state "$vm" "status=running" "pid=$pid" "start_time=$started_start_time" "socket=$socket" "jail=$jail" "cgroup=$process_cgroup_rel" "kernel=$(state_value kernel "$state")" "rootfs=$(state_value rootfs "$state")" "vcpus=$vcpus" "memory_mib=$memory_mib" "uid=$uid" "gid=$gid"
    started_pid=""
    started_start_time=""
    started_jail=""
    started_cgroup=""
    started_logger=""
    rm -f "$log_pipe"
    started_log_pipe=""
    ok "$(json_vm "$vm")"
    ;;
stop)
    valid_id "$id" || error "invalid microVM id"
    [ "$force" = 0 ] || [ "$force" = 1 ] || error "force must be 0 or 1"
    vm="$BASE/$id"
    [ -d "$vm" ] && [ ! -L "$vm" ] || error "microVM does not exist"
    state="$vm/state"
    [ -f "$state" ] && [ ! -L "$state" ] || error "microVM state is missing"
    pid=$(state_value pid "$state" || true)
    start_time=$(state_value start_time "$state" || true)
    jail=$(state_value jail "$state" || true)
    cgroup=$(state_value cgroup "$state" || true)
    # Firecracker's CtrlAltDel action is x86-only. Without a guest agent,
    # stopping this ARM64 VM terminates the VMM and cannot flush guest caches.
    if matches_firecracker "$pid" "$start_time"; then
        if [ "$force" = 1 ]; then
            kill -KILL "$pid" 2>/dev/null || true
        else
            kill -TERM "$pid" 2>/dev/null || true
            for attempt in $(seq 1 40); do
                matches_firecracker "$pid" "$start_time" || break
                sleep 0.1
            done
            if matches_firecracker "$pid" "$start_time"; then kill -KILL "$pid" 2>/dev/null || true; fi
        fi
    fi
    cgroup_parent=$(parent_cgroup || true)
    if valid_parent_cgroup "$cgroup_parent" && [ "$cgroup" = "/$cgroup_parent/firecracker/$id" ]; then
        terminate_cgroup "/sys/fs/cgroup/$cgroup" || error "microVM cgroup is still occupied"
    fi
    if [ -n "$jail" ] && [ "$jail" = "$JAILER_BASE/firecracker/$id" ]; then rm -rf "$jail"; fi
    write_state "$vm" "status=stopped" "kernel=$(state_value kernel "$state")" "rootfs=$(state_value rootfs "$state")" "vcpus=$(state_value vcpus "$state")" "memory_mib=$(state_value memory_mib "$state")" "uid=$(state_value uid "$state")" "gid=$(state_value gid "$state")"
    ok "$(json_vm "$vm")"
    ;;
inspect)
    if [ -n "$id" ]; then
        valid_id "$id" || error "invalid microVM id"
        vm="$BASE/$id"
        [ -d "$vm" ] && [ ! -L "$vm" ] || error "microVM does not exist"
        [ -f "$vm/state" ] && [ ! -L "$vm/state" ] || error "microVM state is missing"
        ok "$(json_vm "$vm")"
    else
        first=true
        printf '{"version":1,"ok":true,"data":['
        for vm in $(list_vms); do
            [ -d "$vm" ] && [ ! -L "$vm" ] || continue
            [ -f "$vm/state" ] && [ ! -L "$vm/state" ] || error "microVM state is missing"
            $first || printf ','
            first=false
            json_vm "$vm" | tr -d '\n'
        done
        printf ']}\n'
    fi
    ;;
*) error "action is required" ;;
esac
