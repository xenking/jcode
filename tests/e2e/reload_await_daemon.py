#!/usr/bin/env python3
"""Exercise a real daemon exec reload with a pending background swarm await.

Usage: python3 tests/e2e/reload_await_daemon.py /absolute/path/to/jcode
All inference is served by an in-process fake HTTP provider. Runtime, HOME and
socket are isolated under JCODE_SCRATCH_DIR. No shared daemon is contacted.
"""
import json
import os
import shutil
from pathlib import Path
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


def main(binary):
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
                for name in ('old', 'new'):
                    if '[hold:' + name + ']' in all_text and not (controls / (name + '.release')).exists():
                        (controls / (name + '.held')).touch()
                        eventually('release ' + name, lambda: (controls / (name + '.release')).exists(), 180)
                text = 'fake provider completed'
                self.send_response(200)
                self.send_header('content-type', 'text/event-stream')
                self.end_headers()
                for chunk in (
                    {'id': 'x', 'object': 'chat.completion.chunk', 'model': 'fake-model', 'choices': [{'index': 0, 'delta': {'role': 'assistant', 'content': text}}]},
                    {'id': 'x', 'object': 'chat.completion.chunk', 'model': 'fake-model', 'choices': [{'index': 0, 'delta': {}, 'finish_reason': 'stop'}], 'usage': {'prompt_tokens': 1, 'completion_tokens': 1, 'total_tokens': 2}},
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
        staged = runtime / 'jcode'
        shutil.copy2(binary, staged)
        (runtime / 'owner').touch()
        daemon_log = open(root / 'daemon.log', 'w+')
        daemon = subprocess.Popen([staged, '--no-update', '--no-selfdev', '--debug-socket', 'serve', '--socket', str(daemon_socket)],
                                  env=env, cwd=root, stdout=daemon_log, stderr=daemon_log, start_new_session=True)
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
                    if value.get('type') == 'error':
                        raise AssertionError(str(value))
                    if predicate(value):
                        return value
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
            def restart(id):
                nonlocal sock, stream
                # Dispatch the same non-selfdev pinned exec handler as rollout,
                # without staging or promoting a different installed version.
                with socket.socket(socket.AF_UNIX) as debug:
                    debug.connect(str(debug_socket))
                    debug.sendall(b'{"type":"debug_command","id":8,"command":"server:reload:verify-exec"}\n')
                    with debug.makefile() as reply:
                        response = json.loads(reply.readline())
                assert response['ok'], response
                reload_id = json.loads(response['output'])['request_id']
                trace = jhome / 'reload-traces' / (reload_id + '.jsonl')
                # Keep the old connection attached until exec is committed.
                eventually('exec marker', lambda: trace.exists() and '"exec_start"' in trace.read_text())
                stream.close()
                sock.close()
                sock = None
                eventually('replacement socket marker', lambda: '"socket_ready"' in trace.read_text())
                sock = eventually('replacement socket', connect, 60)
                sock.settimeout(1)
                stream = sock.makefile('rb', buffering=0)
                send({'type': 'subscribe', 'id': id, 'target_session_id': coordinator, 'allow_session_takeover': True,
                      'continue_on_disconnect': True, 'working_dir': str(root)})
                event(lambda e: e.get('type') == 'history' and e.get('session_id') == coordinator, 'resubscribe')
                send({'type': 'ping', 'id': id})
                identity = event(lambda e: e.get('type') == 'pong', 'new generation')['daemon_identity']
                assert identity['reload_id'] == reload_id and identity['pid'] == daemon.pid, identity
            def members(id):
                send({'type': 'comm_list', 'id': id, 'session_id': coordinator})
                return event(lambda e: e.get('type') == 'comm_members' and e.get('id') == id, 'member list')['members']
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
            assert not {old, new}.intersection(m['session_id'] for m in members(11))
            restart(12)
            restored = {m['session_id'] for m in members(13)}
            assert not {old, new}.intersection(restored), f'stopped workers returned after exec: {restored}'
            print(f'PASS: exec resumed {new} and its await exactly once, then did not resurrect stopped workers')
        except Exception:
            daemon_log.flush()
            print('daemon log:', (root / 'daemon.log').read_text(errors='replace')[-12000:], file=sys.stderr)
            print('jcode log:', '\n'.join(p.read_text(errors='replace') for p in (jhome / 'logs').glob('jcode-*.log')), file=sys.stderr)
            print('runtime files:', [str(p.relative_to(root)) for p in root.rglob('*') if p.is_file()], file=sys.stderr)
            raise
        finally:
            for name in ('old', 'new'):
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
    main(str(Path(sys.argv[1]).resolve()))
