#!/usr/bin/env python3
"""Synthetic integration tests. No provider credentials or inference are used."""
import contextlib
import json
import os
from pathlib import Path
import socket
import sqlite3
import subprocess
import tempfile
import unittest
from unittest.mock import patch
import dial_batch as batch

BINARY = Path(os.environ.get('DIAL_TEST_BINARY', 'target/debug/dial')).resolve()


class BatchTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.source = self.root / 'source'
        self.source.mkdir()
        self.call('git', 'init', '-q')
        self.call('git', 'config', 'user.name', 'Synthetic')
        self.call('git', 'config', 'user.email', 'synthetic@example.invalid')
        (self.source / '.gitignore').write_text('.dial/\n')
        (self.source / 'UserDB.sqlite').write_text('synthetic personal data: never materialize')
        (self.source / 'app.txt').write_text('fixture')
        self.call('git', 'add', '.')
        self.call('git', 'commit', '-qm', 'Fixture')
        self.call(str(BINARY), 'init', '--phase', 'test', '--agents', 'off')
        for key, value in [('ai_cli', 'opencode'), ('opencode_model', 'synthetic/model'),
                           ('opencode_live', 'false'), ('build_cmd', 'true'), ('test_cmd', 'true')]:
            self.call(str(BINARY), 'config', 'set', key, value)
        for task in ['first', 'second', 'third']:
            self.call(str(BINARY), 'task', 'add', task)
        self.plan = {'name':'test-batch', 'phase':'test', 'source_locks':['buglist-run.lock', 'installer-run.lock'],
                     'lanes':[{'task': i, 'port': self.port()} for i in [1,2,3]]}
        self.destination = self.root / 'batch'
        self.bin = self.root / 'bin'
        self.bin.mkdir()
        worker = self.bin / 'opencode'
        worker.write_text('''#!/usr/bin/env python3
import json, os, time
from pathlib import Path
Path('result.txt').write_text(json.dumps({'db': os.environ['BIBLE_STUDY_DB'], 'tmp': os.environ['TMPDIR'], 'start': time.time()}))
time.sleep(0.4)
print('DIAL_COMPLETE: synthetic change')
''')
        worker.chmod(0o755)

    def call(self, *args):
        return batch.run(list(args), self.source)

    def port(self):
        with socket.socket() as sock:
            sock.bind(('127.0.0.1',0))
            return sock.getsockname()[1]

    def prepare(self):
        return batch.prepare(self.source, self.plan, self.destination, BINARY)

    def sql(self, sql):
        with contextlib.closing(sqlite3.connect(self.source / '.dial/test.db')) as conn, conn:
            conn.execute(sql)

    def test_prepare_does_not_launch_or_change_history_and_omits_personal_database(self):
        before = (self.source / '.dial/test.db').read_bytes()
        manifest = self.prepare()
        data = json.loads(manifest.read_text())
        self.assertEqual(data['state'], 'prepared')
        self.assertEqual(before, (self.source / '.dial/test.db').read_bytes())
        for lane in data['lanes']:
            work = Path(lane['path'])
            self.assertFalse((work / 'UserDB.sqlite').exists())
            self.assertFalse((work / 'result.txt').exists())
            self.assertEqual(batch.git(work, 'status', '--porcelain'), '')
            with batch.read_db(work / '.dial/test.db') as conn:
                self.assertEqual(conn.execute('SELECT id,status FROM tasks').fetchall(), [(1,'pending'),(2,'pending'),(3,'pending')])
                self.assertEqual(dict(conn.execute('SELECT key,value FROM config'))['execution_task_id'], str(lane['task']))
        with self.assertRaises(subprocess.CalledProcessError):
            self.call(str(BINARY), 'task', 'next')

    def test_three_fake_workers_are_isolated_and_require_review(self):
        manifest = self.prepare()
        with patch.dict(os.environ, {'PATH': str(self.bin) + os.pathsep + os.environ['PATH']}):
            result = batch.start(manifest)
        self.assertEqual(result['state'], 'awaiting_review')
        starts = []
        for lane in result['lanes']:
            self.assertEqual(lane['exit_code'], 0)
            work = Path(lane['path'])
            artifact = json.loads((work / 'result.txt').read_text())
            self.assertTrue(artifact['db'].startswith(str(work)))
            starts.append(artifact['start'])
            with batch.read_db(work / '.dial/test.db') as conn:
                self.assertEqual(conn.execute("SELECT id FROM tasks WHERE status='completed'").fetchall(), [(lane['task'],)])
        self.assertLess(max(starts) - min(starts), 1.0)
        with batch.read_db(self.source / '.dial/test.db') as conn:
            self.assertEqual(conn.execute("SELECT count(*) FROM tasks WHERE status='pending'").fetchone()[0], 3)
        self.assertTrue((self.source / '.dial/batch-reservation.json').exists())
        batch.release(manifest)
        self.assertFalse((self.source / '.dial/batch-reservation.json').exists())

    def test_active_iteration_dirty_source_and_existing_lock_refused(self):
        (self.source / 'app.txt').write_text('owner edit')
        with self.assertRaisesRegex(batch.Blocked, 'dirty'):
            self.prepare()
        (self.source / 'app.txt').write_text('fixture')
        self.sql("INSERT INTO iterations(task_id,status) VALUES(1,'in_progress')")
        with self.assertRaisesRegex(batch.Blocked, 'active'):
            self.prepare()
        self.sql("DELETE FROM iterations")
        with batch.locks(self.source, self.plan['source_locks']):
            with self.assertRaisesRegex(batch.Blocked, 'owns'):
                self.prepare()
        self.assertFalse(self.destination.exists())

    def test_dependency_duplicate_and_port_collision_refused(self):
        self.sql('INSERT INTO task_dependencies(task_id,depends_on_id) VALUES(2,1)')
        with self.assertRaisesRegex(batch.Blocked, 'dependencies'):
            self.prepare()
        self.sql('DELETE FROM task_dependencies')
        self.plan['lanes'][1]['task'] = 1
        with self.assertRaisesRegex(batch.Blocked, 'unique'):
            self.prepare()
        self.plan['lanes'][1]['task'] = 2
        with socket.socket() as listener:
            listener.bind(('127.0.0.1', self.plan['lanes'][0]['port']))
            listener.listen()
            with self.assertRaisesRegex(batch.Blocked, 'occupied'):
                self.prepare()

    def test_changed_source_queue_and_dirty_lane_prevent_all_launches(self):
        manifest = self.prepare()
        self.sql("UPDATE tasks SET description='changed' WHERE id=1")
        with self.assertRaisesRegex(batch.Blocked, 'queue/configuration changed'):
            batch.start(manifest)
        self.sql("UPDATE tasks SET description='first' WHERE id=1")
        data = json.loads(manifest.read_text())
        (Path(data['lanes'][1]['path']) / 'app.txt').write_text('other edit')
        with self.assertRaisesRegex(batch.Blocked, 'lane has changed'):
            batch.start(manifest)
        for lane in data['lanes']:
            self.assertFalse((Path(lane['path']) / 'result.txt').exists())

    def test_one_failed_lane_is_not_retried_or_accepted(self):
        manifest = self.prepare()
        with patch.object(batch, 'execute_lane', side_effect=lambda lane, binary: 1 if lane['task']==2 else 0) as execute:
            result = batch.start(manifest)
        self.assertEqual(execute.call_count, 3)
        self.assertEqual(result['lanes'][1]['state'], 'failed')
        self.assertTrue((self.source / '.dial/batch-reservation.json').exists())
        with self.assertRaisesRegex(batch.Blocked, 'not prepared'):
            batch.start(manifest)


if __name__ == '__main__':
    unittest.main()
