#!/usr/bin/env python3
"""Prepare and run explicitly assigned tasks in isolated Git worktrees (Linux)."""
import argparse
import concurrent.futures
import contextlib
import fcntl
import json
import os
from pathlib import Path
import re
import signal
import socket
import sqlite3
import subprocess
import sys
import threading
import time


class Blocked(RuntimeError):
    pass


def run(args, cwd):
    return subprocess.check_output(args, cwd=cwd, text=True, stderr=subprocess.STDOUT).strip()


def git(root, *args):
    return run(['git', *args], root)


@contextlib.contextmanager
def read_db(path):
    conn = sqlite3.connect(path.resolve().as_uri() + '?mode=ro', uri=True)
    conn.execute('PRAGMA query_only=ON')
    try:
        yield conn
    finally:
        conn.close()


def save(path, value):
    tmp = path.with_suffix('.tmp')
    tmp.write_text(json.dumps(value, indent=2) + '\n')
    tmp.replace(path)


@contextlib.contextmanager
def locks(source, names):
    with contextlib.ExitStack() as stack:
        for name in sorted(set(['batch.lock', *names])):
            if not re.fullmatch(r'[a-zA-Z0-9_-]+\.lock', name):
                raise Blocked('Invalid lock filename')
            f = stack.enter_context((source / '.dial' / name).open('a'))
            try:
                fcntl.flock(f, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                raise Blocked(f'An existing operation owns {name}') from None
        yield


def validate_plan(plan):
    if not re.fullmatch(r'[a-z0-9][a-z0-9-]{0,40}', plan['name']):
        raise Blocked('Invalid batch name')
    if not re.fullmatch(r'[a-zA-Z0-9_-]+', plan['phase']):
        raise Blocked('Invalid phase')
    lanes = plan['lanes']
    if not 1 <= len(lanes) <= 3:
        raise Blocked('Select one to three lanes')
    ids = [lane['task'] for lane in lanes]
    ports = [lane['port'] for lane in lanes]
    if any(type(i) is not int or i <= 0 for i in ids) or len(set(ids)) != len(ids):
        raise Blocked('Task IDs must be unique positive integers')
    if any(type(p) is not int or not 1024 <= p <= 65535 for p in ports) or len(set(ports)) != len(ports):
        raise Blocked('Live-view ports must be distinct nonprivileged ports')
    if not plan.get('source_locks'):
        raise Blocked('Declare the existing launcher locks')


def preflight(source, plan):
    validate_plan(plan)
    if Path(git(source, 'rev-parse', '--show-toplevel')).resolve() != source:
        raise Blocked('Source must be the repository root')
    if git(source, 'status', '--porcelain', '--untracked-files=all'):
        raise Blocked('Source checkout is dirty; preserve and resolve its existing work')
    if (source / '.dial/current_phase').read_text().strip() != plan['phase']:
        raise Blocked('Source phase changed')
    if (source / '.dial/signal.json').exists():
        raise Blocked('A source worker signal needs review first')
    with read_db(source / '.dial' / (plan['phase'] + '.db')) as conn:
        if conn.execute("SELECT count(*) FROM iterations WHERE status IN ('in_progress','awaiting_approval')").fetchone()[0]:
            raise Blocked('An active/approval-waiting iteration exists')
        if conn.execute("SELECT count(*) FROM tasks WHERE status='in_progress'").fetchone()[0]:
            raise Blocked('An active task exists')
        for lane in plan['lanes']:
            row = conn.execute('SELECT status FROM tasks WHERE id=?', (lane['task'],)).fetchone()
            if row != ('pending',):
                raise Blocked(f"Task {lane['task']} is not pending")
            missing = conn.execute("SELECT td.depends_on_id FROM task_dependencies td LEFT JOIN tasks t ON t.id=td.depends_on_id WHERE td.task_id=? AND (t.status IS NULL OR t.status!='completed')", (lane['task'],)).fetchall()
            if missing:
                raise Blocked(f"Task {lane['task']} has incomplete dependencies: {missing}")
        config = dict(conn.execute('SELECT key,value FROM config'))
        if config.get('ai_cli') != 'opencode' or not config.get('opencode_model'):
            raise Blocked('This coordinator requires the configured OpenCode model')
    return git(source, 'rev-parse', 'HEAD'), config


def check_port(port):
    with socket.socket() as sock:
        try:
            sock.bind(('127.0.0.1', port))
        except OSError:
            raise Blocked(f'Live-view port {port} is occupied') from None


def prepare(source, plan, destination, binary):
    validate_plan(plan)
    with locks(source, plan['source_locks']):
        reservation = source / '.dial/batch-reservation.json'
        if reservation.exists():
            raise Blocked('A batch is already reserved; inspect it first')
        base, config = preflight(source, plan)
        if destination.exists() or source == destination or source in destination.parents:
            raise Blocked('Destination must be a new directory outside the source checkout')
        # Require a binary containing the pin and reservation guards, without inference.
        help_text = run([str(binary), 'batch-capabilities'], source)
        if help_text != 'task-pin-v1 source-reservation-v1':
            raise Blocked('The selected Dial binary does not support isolated batches')
        for lane in plan['lanes']:
            check_port(lane['port'])
        destination.mkdir(parents=True)
        manifest_path = destination / 'batch.json'
        manifest = dict(plan, source=str(source), base=base, binary=str(binary),
                        state='preparing', model=config['opencode_model'], lanes=[])
        save(manifest_path, manifest)
        save(reservation, {'manifest': str(manifest_path), 'base': base})
        try:
            with read_db(source / '.dial' / (plan['phase'] + '.db')) as src:
                with contextlib.closing(sqlite3.connect(destination / 'source-snapshot.db')) as snapshot:
                    src.backup(snapshot)
            for lane in plan['lanes']:
                lane = dict(lane, path=str(destination / f"task-{lane['task']}"),
                            branch=f"batch/{plan['name']}/task-{lane['task']}", state='preparing')
                manifest['lanes'].append(lane)
                save(manifest_path, manifest)
                git(source, 'worktree', 'add', '--no-checkout', '-b', lane['branch'], lane['path'], base)
                work = Path(lane['path'])
                # Never materialize a tracked personal database from Git history.
                gitdir = Path(git(work, 'rev-parse', '--absolute-git-dir'))
                (gitdir / 'info').mkdir(exist_ok=True)
                (gitdir / 'info/sparse-checkout').write_text('/*\n!/UserDB.sqlite\n')
                git(work, '-c', 'core.sparseCheckout=true', '-c', 'core.sparseCheckoutCone=false', 'read-tree', '-mu', 'HEAD')
                dial = work / '.dial'
                dial.mkdir(exist_ok=True)
                if git(work, 'check-ignore', '.dial/probe') != '.dial/probe':
                    raise Blocked('Worktree must ignore .dial runtime state')
                (dial / 'current_phase').write_text(plan['phase'])
                with read_db(destination / 'source-snapshot.db') as src:
                    with contextlib.closing(sqlite3.connect(dial / (plan['phase'] + '.db'))) as conn, conn:
                        src.backup(conn)
                        for key, value in {'execution_task_id': str(lane['task']),
                                           'opencode_live_port': str(lane['port'])}.items():
                            conn.execute('INSERT OR REPLACE INTO config(key,value) VALUES (?,?)', (key,value))
                for sub in ['tmp', 'audio']:
                    (dial / 'batch-worker' / sub).mkdir(parents=True)
                lane['state'] = 'prepared'
                save(manifest_path, manifest)
            manifest['state'] = 'prepared'
        except BaseException:
            manifest['state'] = 'prepare_failed'
            save(manifest_path, manifest)
            raise
        save(manifest_path, manifest)
        return manifest_path


def lane_environment(work, task):
    env = dict(os.environ)
    for key in ['OPENAI_API_KEY', 'OPENAI_API_BASE', 'OPENAI_BASE_URL', 'CARGO_TARGET_DIR']:
        env.pop(key, None)
    runtime = work / '.dial/batch-worker'
    env.update(BIBLE_STUDY_DB=str(runtime / 'UserDB.sqlite'),
               BIBLE_AUDIO_DIR=str(runtime / 'audio'), TMPDIR=str(runtime / 'tmp'),
               BIBLE_STUDY_BUGLIST_TASK=f'{task:02}')
    return env


CHILDREN = set()
CHILD_LOCK = threading.Lock()
START_LOCK = threading.Lock()
STOP = threading.Event()


def stop_children(_signum, _frame):
    STOP.set()
    with CHILD_LOCK:
        for child in CHILDREN:
            if child.poll() is None:
                os.killpg(child.pid, signal.SIGTERM)


def execute_lane(lane, binary):
    work = Path(lane['path'])
    # timeout owns only this command's process group. A failure is never retried.
    with (work / '.dial/batch-attempt.log').open('wb') as log:
        if STOP.is_set():
            return 130
        # Serialize only cold server startup; implementation remains concurrent.
        # OpenCode shares host-level initialization state even across worktrees.
        with START_LOCK:
            child = subprocess.Popen(['timeout', '--signal=TERM', '--kill-after=15s', '45m',
                                      binary, 'auto-run', '--cli', 'opencode', '--max', '1',
                                      '--max-attempts', '1'], cwd=work,
                                     env=lane_environment(work, lane['task']),
                                     stdout=subprocess.PIPE, stderr=subprocess.STDOUT, start_new_session=True)
            with CHILD_LOCK:
                CHILDREN.add(child)
                if STOP.is_set() and child.poll() is None:
                    os.killpg(child.pid, signal.SIGTERM)
            with read_db(work / '.dial' / ((work / '.dial/current_phase').read_text().strip() + '.db')) as conn:
                live = dict(conn.execute('SELECT key,value FROM config')).get('opencode_live') == 'true'
            if live:
                deadline = time.monotonic() + 30
                while child.poll() is None and time.monotonic() < deadline and not STOP.is_set():
                    try:
                        with socket.create_connection(('127.0.0.1', lane['port']), timeout=0.2):
                            break
                    except OSError:
                        time.sleep(0.1)
        written = 0
        for chunk in iter(lambda: child.stdout.read1(65536), b''):
            keep = chunk[:max(0, 8 * 1024 * 1024 - written)]
            log.write(keep)
            log.flush()
            written += len(keep)
        child.stdout.close()
        result = child.wait()
        with CHILD_LOCK:
            CHILDREN.discard(child)
        return result


def start(manifest_path, jobs=3):
    if jobs not in (1, 2, 3):
        raise Blocked("Worker concurrency must be between 1 and 3")
    manifest = json.loads(manifest_path.read_text())
    source = Path(manifest['source'])
    with locks(source, manifest['source_locks']):
        reservation = json.loads((source / '.dial/batch-reservation.json').read_text())
        if reservation['manifest'] != str(manifest_path) or manifest['state'] != 'prepared':
            raise Blocked('Batch is not prepared or its reservation differs')
        base, _ = preflight(source, manifest)
        with read_db(source / '.dial' / (manifest['phase'] + '.db')) as current, read_db(manifest_path.parent / 'source-snapshot.db') as snapshot:
            for table in ['tasks', 'task_dependencies', 'iterations', 'config']:
                if current.execute(f'SELECT * FROM {table} ORDER BY 1').fetchall() != snapshot.execute(f'SELECT * FROM {table} ORDER BY 1').fetchall():
                    raise Blocked('Source queue/configuration changed since preparation')
        if base != manifest['base']:
            raise Blocked('Source HEAD changed; review and prepare a fresh batch')
        # All lanes pass preflight before any worker starts.
        for lane in manifest['lanes']:
            work = Path(lane['path'])
            lane_base = lane.get('reviewed_checkpoint', base)
            if git(work, 'rev-parse', 'HEAD') != lane_base or git(work, 'status', '--porcelain', '--untracked-files=all'):
                raise Blocked('A lane has changed since preparation')
            if lane_base != base:
                git(work, 'merge-base', '--is-ancestor', base, lane_base)
            if (work / '.dial/signal.json').exists():
                raise Blocked('A lane contains a previous worker signal')
            with read_db(work / '.dial' / (manifest['phase'] + '.db')) as conn:
                config = dict(conn.execute('SELECT key,value FROM config'))
                if config.get('execution_task_id') != str(lane['task']) or config.get('opencode_model') != manifest['model'] or config.get('opencode_live_port') != str(lane['port']):
                    raise Blocked('A lane configuration changed')
            with read_db(work / '.dial' / (manifest['phase'] + '.db')) as conn:
                if conn.execute("SELECT status FROM tasks WHERE id=?", (lane['task'],)).fetchone() != ('pending',) or conn.execute("SELECT count(*) FROM iterations WHERE status IN ('in_progress','awaiting_approval')").fetchone()[0]:
                    raise Blocked('Lane queue is no longer fresh')
            check_port(lane['port'])
            frontend = work / 'modules/core/frontend/package.json'
            if frontend.exists() and not (frontend.parent / 'node_modules').is_dir():
                raise Blocked(f'Install lane frontend dependencies first: {work}')
        STOP.clear()
        previous_handlers = {sig: signal.signal(sig, stop_children) for sig in [signal.SIGINT, signal.SIGTERM]}
        manifest['concurrency'] = jobs
        manifest['state'] = 'running'
        save(manifest_path, manifest)
        with concurrent.futures.ThreadPoolExecutor(max_workers=jobs) as pool:
            futures = {pool.submit(execute_lane, lane, manifest['binary']): lane for lane in manifest['lanes']}
            for future in concurrent.futures.as_completed(futures):
                lane = futures[future]
                try:
                    lane['exit_code'] = future.result()
                    lane['state'] = 'awaiting_review' if lane['exit_code'] == 0 else 'failed'
                except Exception as error:
                    lane['state'] = 'failed'
                    lane['error'] = str(error)
                save(manifest_path, manifest)
        for sig, handler in previous_handlers.items():
            signal.signal(sig, handler)
        manifest['state'] = 'interrupted' if STOP.is_set() else 'awaiting_review'
        save(manifest_path, manifest)
        return manifest


def release(manifest_path):
    manifest = json.loads(manifest_path.read_text())
    source = Path(manifest['source'])
    with locks(source, manifest['source_locks']):
        if manifest['state'] == 'running':
            raise Blocked('Running or interrupted batch requires process/iteration inspection')
        reservation = source / '.dial/batch-reservation.json'
        if json.loads(reservation.read_text())['manifest'] != str(manifest_path):
            raise Blocked('Reservation belongs to another batch')
        manifest['state'] = 'released_after_review'
        save(manifest_path, manifest)
        reservation.unlink()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest='command', required=True)
    p = sub.add_parser('prepare', help='Create worktrees and queue snapshots; never launch workers')
    p.add_argument('--source', type=Path, required=True)
    p.add_argument('--plan', type=Path, required=True)
    p.add_argument('--destination', type=Path, required=True)
    p.add_argument('--binary', type=Path, required=True)
    for name in ['start', 'status', 'release']:
        p = sub.add_parser(name)
        p.add_argument('manifest', type=Path)
        if name == 'start':
            p.add_argument('--jobs', type=int, choices=(1, 2, 3), default=3,
                           help='Maximum simultaneous workers (1 runs lanes sequentially)')
        if name == 'release':
            p.add_argument('--reviewed', action='store_true', required=True)
    args = parser.parse_args()
    try:
        if args.command == 'prepare':
            print(prepare(args.source.resolve(), json.loads(args.plan.read_text()), args.destination.resolve(), args.binary.resolve()))
        elif args.command == 'start':
            result = start(args.manifest.resolve(), jobs=args.jobs)
            print(json.dumps(result, indent=2))
            return 1 if any(lane['state'] == 'failed' for lane in result['lanes']) else 0
        elif args.command == 'status':
            print(args.manifest.read_text())
        else:
            release(args.manifest.resolve())
    except (Blocked, OSError, sqlite3.Error, subprocess.CalledProcessError, ValueError, KeyError) as error:
        print(f'BLOCKED: {error}', file=sys.stderr)
        return 1
    return 0


if __name__ == '__main__':
    sys.exit(main())
