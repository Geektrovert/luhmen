#!/usr/bin/env python3
"""Exercise the embedded guest helper without changing host or guest services."""
from pathlib import Path
import os
import signal
import tempfile
import threading
import types
import unittest
from unittest.mock import patch


class ShutdownTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='luhmen-shutdown-test-')
        self.addCleanup(self.temp.cleanup)
        source = Path(__file__).with_name('docker-shutdown.sh').read_text()
        source = source.split("<<'PY'\n", 1)[1].rsplit('\nPY', 1)[0]
        self.helper = types.ModuleType('docker_shutdown')
        exec(compile(source, 'docker-shutdown.sh', 'exec'), self.helper.__dict__)
        root = Path(self.temp.name)
        self.helper.CONFIG = root / 'daemon.json'
        self.helper.BACKUP = root / 'backup.json'
        self.original = b'{\n  "live-restore": true, "data-root": "/var/lib/docker", "debug": false\n}\n'
        self.temporary = b'{"live-restore":false,"data-root":"/var/lib/docker","debug":false}\n'
        self.helper.CONFIG.write_bytes(self.original)
        self.helper.CONFIG.chmod(0o640)
        self.active = True
        self.live = True
        self.result = 'success'
        self.shims = False
        self.on_stop = None
        self.stopped = False
        for name, replacement in (
            ('service_state', self.state), ('systemctl', self.systemctl),
            ('live_restore', lambda: self.live), ('reload_config', self.reload),
            ('docker_shims_remain', lambda: self.shims),
        ):
            patcher = patch.object(self.helper, name, replacement)
            patcher.start()
            self.addCleanup(patcher.stop)
        patcher = patch.object(self.helper.signal, 'signal')
        patcher.start()
        self.addCleanup(patcher.stop)

    def state(self):
        return {'ActiveState': 'active' if self.active else 'inactive',
                'MainPID': '321' if self.active else '0', 'Result': self.result}

    def reload(self, expected):
        configured = self.helper.json.loads(self.helper.CONFIG.read_bytes()).get('live-restore', False)
        self.assertEqual(configured, expected)
        self.live = configured

    def systemctl(self, *args, **kwargs):
        if args[0] == 'stop':
            self.assertEqual(args, ('stop', 'docker.service', 'docker.socket'))
            self.assertEqual(self.helper.CONFIG.read_bytes(), self.original)
            self.assertFalse(self.live)
            self.stopped = True
            if self.on_stop:
                self.on_stop()
            self.active = False
            return ''
        self.assertEqual(args, ('show', 'docker.socket', '-p', 'ActiveState', '--value'))
        return 'inactive\n'

    def assert_restored(self):
        self.assertEqual(self.helper.CONFIG.read_bytes(), self.original)
        self.assertEqual(self.helper.CONFIG.stat().st_mode & 0o777, 0o640)
        self.assertFalse(self.helper.BACKUP.exists())

    def test_success_restores_config_before_daemon_shutdown(self):
        self.helper.shutdown()
        self.assertTrue(self.stopped)
        self.assertFalse(self.active)
        self.assertFalse(self.live)
        self.assert_restored()

    def test_failure_and_cancellation_restore_running_daemon_configuration(self):
        for cancellation in (False, True):
            with self.subTest(cancellation=cancellation):
                def fail():
                    if cancellation:
                        self.helper.interrupted(signal.SIGTERM, None)
                    raise RuntimeError('injected stop failure')
                self.on_stop = fail
                with self.assertRaises(RuntimeError):
                    self.helper.shutdown()
                self.assertTrue(self.active)
                self.assertTrue(self.live)
                self.assert_restored()

    def test_failed_reload_restores_exact_config(self):
        real_reload = self.reload
        def fail_once(expected):
            if not expected:
                raise RuntimeError('injected reload failure')
            real_reload(expected)
        self.helper.reload_config = fail_once
        with self.assertRaisesRegex(RuntimeError, 'reload failure'):
            self.helper.shutdown()
        self.assertTrue(self.active)
        self.assertTrue(self.live)
        self.assertFalse(self.stopped)
        self.assert_restored()

    def test_retry_repairs_interrupted_temporary_config_and_restored_file(self):
        for temporary_on_disk in (True, False):
            with self.subTest(temporary_on_disk=temporary_on_disk):
                self.active = True
                self.live = False
                self.helper.BACKUP.write_bytes(self.original)
                self.helper.BACKUP.chmod(0o640)
                if temporary_on_disk:
                    self.helper.CONFIG.write_bytes(self.temporary)
                self.helper.shutdown()
                self.assertFalse(self.active)
                self.assert_restored()

    def test_external_edit_is_preserved_with_backup_for_manual_recovery(self):
        edited = b'{"live-restore":true,"debug":true}\n'
        def edit_and_fail():
            self.helper.CONFIG.write_bytes(edited)
            raise RuntimeError('injected concurrent edit')
        self.on_stop = edit_and_fail
        with self.assertRaisesRegex(RuntimeError, 'preserving the external edit'):
            self.helper.shutdown()
        self.assertEqual(self.helper.CONFIG.read_bytes(), edited)
        self.assertEqual(self.helper.BACKUP.read_bytes(), self.original)
        with self.assertRaisesRegex(RuntimeError, 'preserving the external edit'):
            self.helper.shutdown()
        self.assertEqual(self.helper.CONFIG.read_bytes(), edited)

    def test_transitional_daemon_keeps_backup_until_reload_can_be_confirmed(self):
        for state in ('activating', 'deactivating'):
            for recovery in (True, False):
                with self.subTest(state=state, recovery=recovery):
                    self.active = True
                    self.helper.service_state = self.state
                    self.helper.CONFIG.write_bytes(self.original)
                    self.helper.CONFIG.chmod(0o640)
                    self.live = True
                    def transitional():
                        return {'ActiveState': state, 'MainPID': '321', 'Result': 'success'}
                    if recovery:
                        self.helper.BACKUP.write_bytes(self.original)
                        self.helper.BACKUP.chmod(0o640)
                        self.helper.CONFIG.write_bytes(self.temporary)
                        self.live = False
                        self.helper.service_state = transitional
                    else:
                        def fail_during_transition():
                            self.helper.service_state = transitional
                            raise RuntimeError('interrupted during transition')
                        self.on_stop = fail_during_transition
                    with self.assertRaises(RuntimeError):
                        self.helper.shutdown()
                    self.assertEqual(self.helper.CONFIG.read_bytes(), self.original)
                    self.assertTrue(self.helper.BACKUP.exists())
                    self.assertFalse(self.live)
                    self.helper.service_state = self.state
                    self.on_stop = None
                    self.helper.shutdown()
                    self.assertFalse(self.active)
                    self.assert_restored()

    def test_symlink_and_oversized_config_are_rejected_before_stopping(self):
        target = self.helper.CONFIG.with_name('target.json')
        self.helper.CONFIG.rename(target)
        self.helper.CONFIG.symlink_to(target)
        with self.assertRaises(OSError):
            self.helper.shutdown()
        self.assertFalse(self.stopped)
        self.assertEqual(target.read_bytes(), self.original)
        self.helper.CONFIG.unlink()
        self.helper.CONFIG.write_bytes(b' ' * (self.helper.LIMIT + 1))
        with self.assertRaisesRegex(RuntimeError, 'at most 1 MiB'):
            self.helper.shutdown()
        self.assertFalse(self.stopped)

    def test_daemon_shutdown_timeout_and_remaining_shims_are_errors(self):
        self.result = 'timeout'
        with self.assertRaisesRegex(RuntimeError, 'shutdown failed: timeout'):
            self.helper.shutdown()
        self.assert_restored()
        self.result = 'success'
        self.shims = True
        with self.assertRaisesRegex(RuntimeError, 'container processes remain'):
            self.helper.shutdown()

    def test_fifo_is_rejected_without_waiting_for_a_writer(self):
        self.helper.CONFIG.unlink()
        os.mkfifo(self.helper.CONFIG)
        errors = []
        def read():
            try:
                self.helper.shutdown()
            except Exception as error:
                errors.append(error)
        reader = threading.Thread(target=read, daemon=True)
        reader.start()
        reader.join(timeout=0.2)
        blocked = reader.is_alive()
        if blocked:
            os.close(os.open(self.helper.CONFIG, os.O_WRONLY | os.O_NONBLOCK))
            reader.join(timeout=1)
        self.assertFalse(blocked, 'configuration read waited for a FIFO writer')
        self.assertIsInstance(errors[0], RuntimeError)
        self.assertFalse(self.stopped)


if __name__ == '__main__':
    unittest.main()
