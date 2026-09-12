#!/bin/sh
# Run through limactl shell before VM shutdown, including on older guests.
set -eu
if ! command -v python3 >/dev/null 2>&1; then
    printf 'Graceful Docker shutdown requires python3 in the guest; VM power-off was not requested.\n' >&2
    exit 1
fi
exec python3 - <<'PY'
import fcntl
import http.client
import json
import os
from pathlib import Path
import signal
import socket
import stat
import subprocess
import sys
import tempfile
import time

CONFIG = Path('/etc/docker/daemon.json')
BACKUP = Path('/etc/docker/.luhmen-shutdown-daemon.json')
LIMIT = 1024 * 1024


def read_file(path):
    with os.fdopen(os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK), 'rb') as stream:
        metadata = os.fstat(stream.fileno())
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_size > LIMIT:
            raise RuntimeError(f'{path} must be a regular file of at most 1 MiB')
        data = stream.read(LIMIT + 1)
        if len(data) > LIMIT:
            raise RuntimeError(f'{path} exceeds 1 MiB')
        return data, metadata


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f'duplicate Docker configuration key: {key}')
        result[key] = value
    return result


def disabled_config(original):
    config = json.loads(original, object_pairs_hook=unique_object)
    if not isinstance(config, dict):
        raise RuntimeError('Docker configuration must be a JSON object')
    if 'live-restore' in config and not isinstance(config['live-restore'], bool):
        raise RuntimeError('Docker live-restore must be a boolean')
    config['live-restore'] = False
    data = (json.dumps(config, separators=(',', ':')) + '\n').encode()
    if len(data) > LIMIT:
        raise RuntimeError('Temporary Docker configuration exceeds 1 MiB')
    return data


def sync_directory():
    descriptor = os.open(CONFIG.parent, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def write_file(path, data, metadata, expected):
    descriptor, temporary = tempfile.mkstemp(prefix='.luhmen-shutdown-', dir=path.parent)
    try:
        with os.fdopen(descriptor, 'wb') as stream:
            os.fchown(stream.fileno(), metadata.st_uid, metadata.st_gid)
            os.fchmod(stream.fileno(), stat.S_IMODE(metadata.st_mode))
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
        if read_file(path)[0] != expected:
            raise RuntimeError(f'{path} changed during shutdown; preserving the external edit')
        os.replace(temporary, path)
        sync_directory()
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def save_backup(original, metadata):
    descriptor, temporary = tempfile.mkstemp(prefix='.luhmen-shutdown-', dir=BACKUP.parent)
    try:
        with os.fdopen(descriptor, 'wb') as stream:
            os.fchown(stream.fileno(), metadata.st_uid, metadata.st_gid)
            os.fchmod(stream.fileno(), stat.S_IMODE(metadata.st_mode))
            stream.write(original)
            stream.flush()
            os.fsync(stream.fileno())
        # Publish a complete backup atomically without replacing an existing one.
        os.link(temporary, BACKUP)
        sync_directory()
    finally:
        os.unlink(temporary)


def systemctl(*args, timeout=5):
    result = subprocess.run(['systemctl', *args], capture_output=True, text=True, timeout=timeout)
    if result.returncode:
        raise RuntimeError(f'systemctl {" ".join(args)} failed: {result.stderr.strip()}')
    return result.stdout


def service_state():
    return dict(line.split('=', 1) for line in systemctl(
        'show', 'docker.service', '-p', 'ActiveState', '-p', 'MainPID', '-p', 'Result'
    ).splitlines() if '=' in line)


class DockerConnection(http.client.HTTPConnection):
    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(self.timeout)
        self.sock.connect('/var/run/docker.sock')


def live_restore():
    connection = DockerConnection('localhost', timeout=1)
    try:
        connection.request('GET', '/info')
        response = connection.getresponse()
        data = response.read(LIMIT + 1)
        if response.status != 200 or len(data) > LIMIT:
            raise RuntimeError('Docker /info did not return a bounded successful response')
        value = json.loads(data).get('LiveRestoreEnabled')
        if not isinstance(value, bool):
            raise RuntimeError('Docker /info did not report LiveRestoreEnabled')
        return value
    finally:
        connection.close()


def reload_config(expected):
    state = service_state()
    pid = int(state.get('MainPID', '0'))
    if state.get('ActiveState') != 'active' or pid <= 1:
        raise RuntimeError('Docker is not active enough to reload its configuration')
    os.kill(pid, signal.SIGHUP)
    deadline = time.monotonic() + 8
    while True:
        try:
            if live_restore() == expected:
                return
        except (OSError, ValueError, RuntimeError, http.client.HTTPException):
            pass
        if time.monotonic() >= deadline:
            raise RuntimeError('Docker did not confirm the live-restore configuration reload within 8 seconds')
        time.sleep(0.1)


def docker_shims_remain():
    # A failed daemon can leave live-restored containers behind. Do not power
    # them off merely because the service is inactive and its API is missing.
    for path in Path('/proc').glob('[0-9]*/cmdline'):
        try:
            arguments = path.read_bytes().split(b'\0')
        except FileNotFoundError:
            continue
        if arguments and b'containerd-shim' in arguments[0]:
            if any(arguments[index:index + 2] == [b'-namespace', b'moby']
                   for index in range(len(arguments) - 1)):
                return True
    return False


def restore_file(original, metadata, temporary):
    current = read_file(CONFIG)[0]
    if current == temporary:
        write_file(CONFIG, original, metadata, temporary)
    elif current != original:
        raise RuntimeError('Docker configuration changed during shutdown; preserving the external edit and backup')


def remove_backup():
    BACKUP.unlink()
    sync_directory()


def restore_runtime(original):
    state = service_state()
    if state.get('ActiveState') == 'active':
        reload_config(json.loads(original).get('live-restore', False))
    elif state.get('ActiveState') not in ('inactive', 'failed') or state.get('MainPID') != '0':
        raise RuntimeError('Docker is still transitioning; original configuration restored and recovery backup retained; wait and retry')


def recover_backup():
    if not BACKUP.exists() and not BACKUP.is_symlink():
        return
    original, metadata = read_file(BACKUP)
    restore_file(original, metadata, disabled_config(original))
    restore_runtime(original)
    remove_backup()


def shutdown():
    recover_backup()
    state = service_state()
    active = state.get('ActiveState') == 'active'
    if not active and state.get('ActiveState') not in ('inactive', 'failed'):
        raise RuntimeError(f'Docker is {state.get("ActiveState")}; wait for its current operation and retry')
    original = metadata = temporary = None
    prepared = False
    succeeded = False
    try:
        if active:
            original, metadata = read_file(CONFIG)
            temporary = disabled_config(original)
            if live_restore():
                save_backup(original, metadata)
                prepared = True
                write_file(CONFIG, temporary, metadata, original)
                reload_config(False)
                # Restore the next boot/restart configuration immediately, without
                # another reload. Only this daemon shutdown has live-restore off.
                restore_file(original, metadata, temporary)
            if service_state().get('MainPID') != state.get('MainPID') or live_restore():
                raise RuntimeError('Docker changed during shutdown preparation; retry after it settles')
        systemctl('stop', 'docker.service', 'docker.socket', timeout=150)
        state = service_state()
        socket_state = systemctl('show', 'docker.socket', '-p', 'ActiveState', '--value').strip()
        if state.get('ActiveState') != 'inactive' or socket_state != 'inactive':
            raise RuntimeError('Docker service and socket did not both stop cleanly')
        if active and state.get('Result') != 'success':
            raise RuntimeError(f'Docker service shutdown failed: {state.get("Result")}')
        if docker_shims_remain():
            raise RuntimeError('Docker container processes remain without an active daemon; recover Docker before graceful VM shutdown')
        succeeded = True
    finally:
        if prepared:
            # Finish restoration even if SSH disconnects during the stop.
            for number in (signal.SIGHUP, signal.SIGINT, signal.SIGTERM):
                signal.signal(number, signal.SIG_IGN)
            restore_file(original, metadata, temporary)
            if not succeeded:
                restore_runtime(original)
            remove_backup()


def interrupted(number, frame):
    raise RuntimeError('Docker shutdown cancelled; VM power-off was not requested')


def main():
    descriptor = os.open('/run/luhmen-docker-shutdown.lock',
                         os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
    with os.fdopen(descriptor, 'w') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        for number in (signal.SIGHUP, signal.SIGINT, signal.SIGTERM):
            signal.signal(number, interrupted)
        shutdown()


if __name__ == '__main__':
    try:
        main()
    except Exception as error:
        print(f'Graceful Docker shutdown failed: {error}. VM power-off was not requested.', file=sys.stderr)
        sys.exit(1)
PY
