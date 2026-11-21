#!/usr/bin/env python3
"""Exercise a real daemon exec reload with a pending background swarm await.

Usage: python3 tests/e2e/reload_await_daemon.py /absolute/path/to/jcode [--stop-tree]
All inference is served by an in-process fake HTTP provider. Runtime, HOME and
socket are isolated under JCODE_SCRATCH_DIR. No shared daemon is contacted.
"""
import json
import os
import shutil
from pathlib import Path
import shlex
import socket
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def eventually(label, f, seconds=45):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        value = f()
        if value:
            return value
        time.sleep(.1)
    raise AssertionError('timeout: ' + label)


def main(binary, stop_tree=False):
    scratch = Path(os.environ['JCODE_SCRATCH_DIR'])
    with tempfile.TemporaryDirectory(prefix='sr2-reload-', dir=scratch) as tmp:
        root = Path(tmp)
        home, runtime, controls = (root / x for x in ('home', 'run', 'controls'))
        jhome = runtime / 'data'
        for path in (home, runtime, controls, jhome):
            path.mkdir()
        calls = []
        lock = threading.Lock()

        class Provider(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def do_GET(self):
                body = json.dumps({'data': [{'id': 'fake-model', 'object': 'model'}]}).encode()
                self.send_response(200)
                self.send_header('content-length', str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def do_POST(self):
                req = json.loads(self.rfile.read(int(self.headers['content-length'])))
                messages = req.get('messages', [])
                with lock:
                    calls.append(messages)
                all_text = json.dumps(messages)
                explicit_resume = '[stop-tree-resumed]' in all_text
                for name in ('old', 'new'):
                    if '[hold:' + name + ']' in all_text and not (controls / (name + '.release')).exists():
                        (controls / (name + '.held')).touch()
                        eventually('release ' + name, lambda: (controls / (name + '.release')).exists(), 180)
                tool = None
                if stop_tree and not explicit_resume and '[stop-tree-parent]' in all_text:
                    previous = {call['id'] for msg in messages for call in msg.get('tool_calls', []) or []}
                    if 'stop-tree-todo' not in previous:
                        tool = ('stop-tree-todo', 'todo', {'add': [{
                            'name': 'Stop tree open work', 'status': 'in_progress',
                            'priority': 'high', 'confidence': 'validated', 'phase': 1, 'lane': 'worker',
                        }]})
                    elif 'stop-tree-bash' not in previous and (controls / 'legacy-ready').exists():
                        tool = ('stop-tree-bash', 'bash', {
                            'command': f'touch {shlex.quote(str(controls / "bg.started"))}; '
                                       f'sleep 15; touch {shlex.quote(str(controls / "bg.escaped"))}',
                            'intent': 'Exercise cancellation of a running background process',
                            'run_in_background': True, 'notify': True, 'wake': True,
                        })
                self.send_response(200)
                self.send_header('content-type', 'text/event-stream')
                self.end_headers()
                if stop_tree and not explicit_resume and tool is None and '[stop-tree-' in all_text:
                    name = 'parent' if '[stop-tree-parent]' in all_text else 'child'
                    (controls / (name + '.held')).touch()
                    try:
                        while not (controls / 'stop-tree.release').exists():
                            self.wfile.write(b': hold\n\n')
                            self.wfile.flush()
                            time.sleep(.05)
                    except (BrokenPipeError, ConnectionResetError):
                        (controls / (name + '.closed')).touch()
                    return
                delta = {'role': 'assistant', 'content': 'fake provider completed'}
                reason = 'stop'
                if tool:
                    call_id, name, arguments = tool
                    delta = {'role': 'assistant', 'tool_calls': [{'index': 0, 'id': call_id,
                             'type': 'function', 'function': {'name': name, 'arguments': json.dumps(arguments)}}]}
                    reason = 'tool_calls'
                for chunk in (
                    {'id': 'x', 'object': 'chat.completion.chunk', 'model': 'fake-model', 'choices': [{'index': 0, 'delta': delta}]},
                    {'id': 'x', 'object': 'chat.completion.chunk', 'model': 'fake-model', 'choices': [{'index': 0, 'delta': {}, 'finish_reason': reason}], 'usage': {'prompt_tokens': 1, 'completion_tokens': 1, 'total_tokens': 2}},
                ):
                    try:
                        self.wfile.write(b'data: ' + json.dumps(chunk).encode() + b'\n\n')
                        self.wfile.flush()
                    except (BrokenPipeError, ConnectionResetError):
                        return
                try:
                    self.wfile.write(b'data: [DONE]\n\n')
                except (BrokenPipeError, ConnectionResetError):
                    pass

        provider = ThreadingHTTPServer(('127.0.0.1', 0), Provider)
        threading.Thread(target=provider.serve_forever, daemon=True).start()
        daemon_socket = runtime / 'app.sock'
        debug_socket = runtime / 'app-debug.sock'
        env = {'PATH': '/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin', 'HOME': str(home),
               'USER': os.environ.get('USER', 'test'), 'TERM': 'xterm-256color',
               'TMPDIR': str(runtime), 'XDG_RUNTIME_DIR': str(runtime), 'JCODE_HOME': str(jhome),
               'JCODE_RUNTIME_DIR': str(runtime), 'JCODE_SOCKET': str(daemon_socket),
               'JCODE_TEST_SESSION': '1', 'JCODE_DEBUG_CONTROL': '1', 'JCODE_VERIFY_EXEC_RELOAD': '1',
               'JCODE_PROVIDER': 'openai-compatible', 'JCODE_MODEL': 'fake-model',
               'JCODE_OPENAI_COMPAT_API_BASE': f'http://127.0.0.1:{provider.server_port}/v1',
               'OPENAI_COMPAT_API_KEY': 'fixture-only', 'JCODE_OPENAI_COMPAT_DEFAULT_MODEL': 'fake-model',
               'JCODE_SWARM_MODEL': '', 'JCODE_NO_TELEMETRY': '1', 'JCODE_TELEMETRY': 'off',
               'JCODE_NO_AUTO_UPDATE': '1'}
        if stop_tree:
            (jhome / 'config.toml').write_text('[features]\nauto_poke = true\n')
        staged = runtime / 'jcode'
        shutil.copy2(binary, staged)
        (runtime / 'owner').touch()
        daemon_log = open(root / 'daemon.log', 'w+')
        def start_daemon():
            return subprocess.Popen([staged, '--no-update', '--no-selfdev', '--debug-socket',
                                     'serve', '--socket', str(daemon_socket)], env=env, cwd=root,
                                    stdout=daemon_log, stderr=daemon_log, start_new_session=True)

        daemon = start_daemon()
        sock = None
        try:
            def connect():
                s = socket.socket(socket.AF_UNIX)
                try:
                    s.connect(str(daemon_socket))
                    return s
                except OSError:
                    s.close()
                    return None
            sock = eventually('daemon socket', connect)
            sock.settimeout(1)
            stream = sock.makefile('rb', buffering=0)
            def send(payload):
                sock.sendall((json.dumps(payload) + '\n').encode())
            observed_members = []
            def event(predicate, label, timeout=45):
                end = time.monotonic() + timeout
                seen = []
                # socket makefile cannot recover from read timeout, use select instead
                import select
                while time.monotonic() < end:
                    if not select.select([sock], [], [], .2)[0]:
                        continue
                    line = stream.readline()
                    if not line:
                        raise AssertionError('daemon disconnected waiting for ' + label)
                    value = json.loads(line)
                    seen.append(value.get('type'))
                    if value.get('type') == 'swarm_status':
                        observed_members.extend(value['members'])
                    if predicate(value):
                        return value
                    if value.get('type') == 'error':
                        raise AssertionError(str(value))
                raise AssertionError(f'timeout {label}: {seen}')
            send({'type': 'subscribe', 'id': 1, 'working_dir': str(root), 'continue_on_disconnect': True})
            coordinator = event(lambda e: e.get('type') in ('history', 'session_id', 'session') and e.get('session_id'), 'coordinator')['session_id']
            def spawn(id, name):
                send({'type': 'comm_spawn', 'id': id, 'session_id': coordinator, 'working_dir': str(root),
                      'spawn_mode': 'headless', 'label': name, 'initial_message': '[hold:' + name + '] do work'})
                worker = event(lambda e: e.get('type') == 'comm_spawn_response' and e.get('id') == id, 'spawn')['new_session_id']
                eventually(name + ' held', lambda: (controls / (name + '.held')).exists())
                return worker
            def await_all(id):
                send({'type': 'comm_await_members', 'id': id, 'session_id': coordinator,
                      'target_status': ['ready', 'completed', 'stopped', 'failed'], 'session_ids': [],
                      'mode': 'all', 'timeout_secs': 180, 'background': True, 'notify': True, 'wake': True})
                return event(lambda e: e.get('type') == 'comm_await_members_response' and e.get('id') == id, 'await')
            def states():
                result = []
                for path in (runtime / 'jcode-await-members').glob('*.json'):
                    try:
                        result.append(json.loads(path.read_text()))
                    except (ValueError, OSError):
                        pass
                return result
            def wake_count():
                with lock:
                    return sum('Swarm await finished' in json.dumps([m for m in msgs if m.get('role') == 'user'][-2:]) for msgs in calls)
            def debug_command(command, session_id=None):
                with socket.socket(socket.AF_UNIX) as debug:
                    debug.settimeout(10)
                    debug.connect(str(debug_socket))
                    request = {'type': 'debug_command', 'id': 8, 'command': command}
                    if session_id:
                        request['session_id'] = session_id
                    debug.sendall((json.dumps(request) + '\n').encode())
                    with debug.makefile() as reply:
                        line = reply.readline()
                        return json.loads(line) if line else {'ok': False, 'output': 'empty debug response'}

            def attach_coordinator(id):
                nonlocal sock, stream
                sock = eventually('replacement socket', connect, 60)
                sock.settimeout(1)
                stream = sock.makefile('rb', buffering=0)
                send({'type': 'subscribe', 'id': id, 'target_session_id': coordinator, 'allow_session_takeover': True,
                      'continue_on_disconnect': True, 'working_dir': str(root)})
                event(lambda e: e.get('type') in ('history', 'session') and e.get('session_id') == coordinator,
                      'resubscribe')

            def restart(id):
                nonlocal sock
                # Dispatch the same non-selfdev pinned exec handler as rollout,
                # without staging or promoting a different installed version.
                response = debug_command('server:reload:verify-exec')
                assert response['ok'], response
                reload_id = json.loads(response['output'])['request_id']
                trace = jhome / 'reload-traces' / (reload_id + '.jsonl')
                # Keep the old connection attached until exec is committed.
                eventually('exec marker', lambda: trace.exists() and '"exec_start"' in trace.read_text())
                stream.close()
                sock.close()
                sock = None
                def replacement_ready():
                    records = trace.read_text()
                    assert daemon.poll() is None, f'exec exited with code {daemon.returncode}: {records}'
                    return '"socket_ready"' in records
                eventually('replacement socket marker', replacement_ready)
                attach_coordinator(id)
                send({'type': 'ping', 'id': id})
                identity = event(lambda e: e.get('type') == 'pong', 'new generation')['daemon_identity']
                assert identity['reload_id'] == reload_id and identity['pid'] == daemon.pid, identity
            def members(id):
                send({'type': 'comm_list', 'id': id, 'session_id': coordinator})
                return event(lambda e: e.get('type') == 'comm_members' and e.get('id') == id, 'member list')['members']

            if stop_tree:
                send({'type': 'message', 'id': 2, 'content': 'Persist fixture coordinator'})
                event(lambda e: e.get('type') == 'done' and e.get('id') == 2, 'coordinator transcript')
                workers = []
                for id, name in enumerate(('parent', 'child'), 3):
                    send({'type': 'comm_spawn', 'id': id, 'session_id': coordinator,
                          'working_dir': str(root), 'spawn_mode': 'headless',
                          'label': 'stop-tree-' + name, 'initial_message': f'[stop-tree-{name}] do work'})
                    workers.append(event(lambda e: e.get('type') == 'comm_spawn_response' and e.get('id') == id,
                                         name)['new_session_id'])
                    eventually(name + ' held', lambda: (controls / (name + '.held')).exists())
                parent, child = workers
                todo = jhome / 'todos' / (parent + '.json')
                assert 'Stop tree open work' in todo.read_text()
                # New recursive spawns are disabled. Restore an old topology
                # from the same durable snapshots older daemons wrote.
                daemon.terminate()
                daemon.wait(timeout=15)
                stream.close()
                sock.close()
                sock = None
                snapshot_path, = (runtime / 'durable-state' / 'swarm').glob('*.json')
                snapshot = json.loads(snapshot_path.read_text())
                nested = next(m for m in snapshot['members'] if m['session_id'] == child)
                assert nested['report_back_to_session_id'] == coordinator
                nested['report_back_to_session_id'] = parent
                for m in snapshot['members']:
                    if m['session_id'] in workers:
                        m['status'] = 'running'
                        m.pop('terminal_since_unix_ms', None)
                snapshot_path.write_text(json.dumps(snapshot))
                # Wait for both original HTTP connections to close before
                # measuring cancellation of the restarted workers' requests.
                for name in ('parent', 'child'):
                    eventually(name + ' stream closed on cold shutdown', lambda: (controls / (name + '.closed')).exists())
                    (controls / (name + '.closed')).unlink()
                    (controls / (name + '.held')).unlink()
                (controls / 'legacy-ready').touch()
                daemon = start_daemon()
                attach_coordinator(5)
                for fixture in ('bg.started', 'parent.held', 'child.held'):
                    eventually(fixture, lambda: (controls / fixture).exists())
                with lock:
                    before = len(calls)
                send({'type': 'comm_stop', 'id': 6, 'session_id': coordinator, 'target_session': parent})
                event(lambda e: e.get('type') == 'done' and e.get('id') == 6, 'stop tree')
                failures = []
                for fixture in ('parent.closed', 'child.closed'):
                    try:
                        eventually(fixture, lambda: (controls / fixture).exists(), 2)
                    except AssertionError:
                        failures.append(f'{fixture}: stop did not close the streaming provider connection')
                for id, session in enumerate(workers, 7):
                    cancelled = debug_command('cancel', session)
                    if not cancelled['ok']:
                        failures.append(f'cancel {session}: {cancelled}')
                    send({'type': 'comm_message', 'id': id, 'from_session': coordinator,
                          'to_session': session, 'message': 'This DM must not wake a stopped worker', 'wake': True})
                    event(lambda e: e.get('id') == id and e.get('type') in ('done', 'error'), 'DM to stopped worker')
                # Longer than the continuation wake and fixture process lifetime.
                time.sleep(16)
                if (controls / 'bg.escaped').exists():
                    failures.append('background process ran after stop')
                with lock:
                    if len(calls) != before:
                        failures.append(f'{len(calls) - before} model requests after stop')
                    before = len(calls)
                try:
                    restart(9)
                except AssertionError as error:
                    failures.append(str(error))
                    raise AssertionError('\n'.join(failures)) from error
                time.sleep(5)
                restored = {m['session_id']: m['status'] for m in members(10)}
                if any(restored.get(worker) != 'stopped' for worker in workers):
                    failures.append(f'expected stopped workers after exec: {restored}')
                with lock:
                    if len(calls) != before:
                        failures.append(f'{len(calls) - before} model requests after exec')
                for session in workers:
                    cancelled = debug_command('cancel', session)
                    if not cancelled['ok']:
                        failures.append(f'cancel after exec {session}: {cancelled}')
                assert not failures, '\n'.join(failures)
                send({'type': 'comm_task_control', 'id': 11, 'session_id': coordinator,
                      'action': 'resume', 'task_id': '', 'target_session': parent,
                      'message': '[stop-tree-resumed] explicit coordinator resume'})
                event(lambda e: e.get('type') == 'comm_task_control_response' and e.get('id') == 11,
                      'explicit coordinator resume')
                def saw_prompt(text):
                    with lock:
                        return any(text in json.dumps(messages) for messages in calls)
                eventually('resumed parent reaches provider', lambda: saw_prompt('explicit coordinator resume'))
                assert next(m for m in members(12) if m['session_id'] == child)['status'] == 'stopped'
                send({'type': 'comm_stop', 'id': 13, 'session_id': coordinator, 'target_session': parent})
                event(lambda e: e.get('type') == 'done' and e.get('id') == 13, 'stop after explicit resume')

                destroyed = debug_command('destroy_session:' + parent)
                assert destroyed['ok'] and 'destroyed' in destroyed['output'], destroyed
                unknown = debug_command('destroy_session:missing-fixture-session')
                assert not unknown['ok'] and 'Unknown session_id' in unknown['output'], unknown
                send({'type': 'subscribe', 'id': 14, 'target_session_id': child, 'allow_session_takeover': True,
                      'continue_on_disconnect': True, 'working_dir': str(root)})
                event(lambda e: e.get('type') in ('history', 'session') and e.get('session_id') == child,
                      'attach stopped child')
                with lock:
                    before = len(calls)
                time.sleep(2)
                with lock:
                    assert len(calls) == before, 'attaching without input woke the child'
                send({'type': 'message', 'id': 15, 'content': '[stop-tree-resumed] explicit user input'})
                event(lambda e: e.get('type') == 'done' and e.get('id') == 15, 'explicit user input')
                assert saw_prompt('explicit user input')
                print('PASS: worker, descendant and job stopped; no requests after stop or exec; debug and explicit resume/input work')
                return

            old = spawn(2, 'old')
            assert await_all(3)['background_started']
            (controls / 'old.release').touch()
            eventually('old completion', lambda: any(s.get('final_response', {}).get('completed') for s in states() if s.get('final_response')))
            eventually('old wake', lambda: wake_count() == 1)
            new = spawn(4, 'new')
            pending = await_all(5)
            assert pending['background_started'], pending
            assert {m['session_id'] for m in pending['members']} == {old, new}, pending
            eventually('pending persisted', lambda: any(s.get('final_response') is None for s in states()))
            def held_calls():
                with lock:
                    return sum('[hold:new] do work' in json.dumps(msgs) for msgs in calls)
            restart(7)
            eventually('worker reaches the provider in the new generation', lambda: held_calls() == 2)
            assert next(m for m in members(8) if m['session_id'] == new)['status'] == 'running'
            time.sleep(2)
            assert wake_count() == 1, 'old completion replayed on reload'
            assert any(s.get('final_response') is None for s in states()), f'pending await vanished during reload: {states()}'
            assert not any(m['session_id'] == new and m['status'] == 'ready' for m in observed_members), observed_members
            (controls / 'new.release').touch()
            final = eventually('resumed await completes', lambda: next((s for s in states() if s.get('final_response') and s['final_response'].get('completed') and any(m['session_id'] == new for m in s['final_response']['members'])), None), 60)
            assert {m['session_id'] for m in final['final_response']['members']} == {old, new}, final
            eventually('new wake', lambda: wake_count() == 2, 60)
            time.sleep(3)
            assert wake_count() == 2, 'completion replayed more than once'
            for id, worker in enumerate((old, new), 9):
                send({'type': 'comm_stop', 'id': id, 'session_id': coordinator,
                      'target_session': worker, 'force': True})
                event(lambda e: e.get('type') == 'done' and e.get('id') == id, 'stop')
            stopped = {m['session_id']: m['status'] for m in members(11)}
            assert all(stopped.get(worker) == 'stopped' for worker in (old, new)), stopped
            restart(12)
            restored = {m['session_id']: m['status'] for m in members(13)}
            assert all(restored.get(worker) == 'stopped' for worker in (old, new)), restored
            print(f'PASS: exec resumed {new} and its await exactly once, then did not resurrect stopped workers')
        except Exception:
            daemon_log.flush()
            print('daemon log:', (root / 'daemon.log').read_text(errors='replace')[-12000:], file=sys.stderr)
            print('jcode log:', '\n'.join(p.read_text(errors='replace') for p in (jhome / 'logs').glob('jcode-*.log')), file=sys.stderr)
            print('runtime files:', [str(p.relative_to(root)) for p in root.rglob('*') if p.is_file()], file=sys.stderr)
            raise
        finally:
            for name in ('old', 'new', 'stop-tree'):
                (controls / (name + '.release')).touch()
            if sock:
                sock.close()
            daemon.terminate()
            try:
                daemon.wait(timeout=5)
            except subprocess.TimeoutExpired:
                daemon.kill()
                daemon.wait()
            provider.shutdown()
            daemon_log.close()


if __name__ == '__main__':
    main(str(Path(sys.argv[1]).resolve()), stop_tree='--stop-tree' in sys.argv[2:])
