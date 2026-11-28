#!/usr/bin/env python3
"""Real isolated daemon recovery acceptance, with a local fake inference provider.

Usage: python3 tests/e2e/reload_recovery_owner.py /absolute/path/to/jcode [case]
Never contacts the installed daemon or changes installed builds.
"""
import hashlib
import json
import os
import re
from pathlib import Path
import select
import socket
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def eventually(label, check, seconds=20):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        value = check()
        if value:
            return value
        time.sleep(.05)
    raise AssertionError('timeout: ' + label)


class Wire:
    def __init__(self, path):
        deadline = time.monotonic() + 10
        while True:
            self.socket = socket.socket(socket.AF_UNIX)
            try:
                self.socket.connect(str(path))
                break
            except (FileNotFoundError, ConnectionRefusedError):
                self.socket.close()
                if time.monotonic() >= deadline:
                    raise
                time.sleep(.025)
        self.buffer = b''
        self.seen = []

    def send(self, value):
        self.socket.sendall(json.dumps(value).encode() + b'\n')

    def event(self, check, seconds=20):
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            while b'\n' in self.buffer:
                line, self.buffer = self.buffer.split(b'\n', 1)
                value = json.loads(line)
                self.seen.append(value)
                observed = value.get('event', value) if value.get('type') == 'turn_event' else value
                if check(observed):
                    return observed
            if select.select([self.socket], [], [], .1)[0]:
                data = self.socket.recv(1024 * 1024)
                if not data:
                    raise AssertionError('daemon disconnected')
                self.buffer += data
        raise AssertionError('wire timeout: ' + str([v.get('type') for v in self.seen[-20:]]))

    def close(self):
        self.socket.close()


class Fixture:
    def __init__(self, binary, case):
        self.temp = tempfile.TemporaryDirectory(prefix='recovery-owner-', dir=os.environ['JCODE_SCRATCH_DIR'])
        self.root = Path(self.temp.name)
        self.runtime = self.root / 'run'
        self.runtime.mkdir()
        (self.runtime / 'owner').touch()
        os.link(binary, self.runtime / 'jcode')
        bridge_source = Path(binary).with_name('jcode-harness-api-bridge')
        if bridge_source.exists():
            os.link(bridge_source, self.runtime / bridge_source.name)
        self.binary = str(self.runtime / 'jcode')
        self.case = case
        self.calls = []
        self.lock = threading.Lock()
        self.release = threading.Event()
        self.wires = []
        self.bridge = None
        owner = self

        class Provider(BaseHTTPRequestHandler):
            def log_message(self, *_):
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
                if not req.get('tools'):
                    body = json.dumps({'id': 'metadata', 'choices': [{'message': {'role': 'assistant', 'content': 'Fixture recovery'}, 'finish_reason': 'stop'}]}).encode()
                    self.send_response(200)
                    self.send_header('content-type', 'application/json')
                    self.send_header('content-length', str(len(body)))
                    self.end_headers()
                    self.wfile.write(body)
                    return
                with owner.lock:
                    owner.calls.append(messages)
                    ordinal = len(owner.calls)
                ask_first = ordinal == 1 and owner.case.startswith('ask')
                if ordinal == 1 and not ask_first:
                    owner.release.wait(120)
                self.send_response(200)
                self.send_header('content-type', 'text/event-stream')
                self.end_headers()
                chunks = [
                    {'id': 'fixture', 'object': 'chat.completion.chunk', 'model': 'fake-model', 'choices': [{'index': 0, 'delta': {'role': 'assistant', 'content': 'recovery completed'}}]},
                    {'id': 'fixture', 'object': 'chat.completion.chunk', 'model': 'fake-model', 'choices': [{'index': 0, 'delta': {}, 'finish_reason': 'stop'}]},
                ]
                if ask_first:
                    question = {'questions': [{'question': 'Continue recovery?', 'header': 'Recovery', 'options': [{'label': 'Continue (Recommended)'}, {'label': 'Cancel'}]}]}
                    chunks = [
                        {'id': 'fixture', 'choices': [{'index': 0, 'delta': {'role': 'assistant', 'tool_calls': [{'index': 0, 'id': 'fixture-ask', 'type': 'function', 'function': {'name': 'ask', 'arguments': json.dumps(question)}}]}}]},
                        {'id': 'fixture', 'choices': [{'index': 0, 'delta': {}, 'finish_reason': 'tool_calls'}]},
                    ]
                try:
                    for chunk in chunks:
                        self.wfile.write(b'data: ' + json.dumps(chunk).encode() + b'\n\n')
                        self.wfile.flush()
                    self.wfile.write(b'data: [DONE]\n\n')
                except (BrokenPipeError, ConnectionResetError):
                    pass

        self.provider = ThreadingHTTPServer(('127.0.0.1', 0), Provider)
        threading.Thread(target=self.provider.serve_forever, daemon=True).start()
        self.home = self.runtime / 'data'
        for path in [self.home, self.root / 'home']:
            path.mkdir()
        self.path = self.runtime / 'app.sock'
        self.env = {
            'PATH': '/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin',
            'HOME': str(self.root / 'home'), 'USER': os.environ.get('USER', 'fixture'),
            'TERM': 'xterm-256color', 'TMPDIR': str(self.runtime),
            'XDG_RUNTIME_DIR': str(self.runtime), 'JCODE_HOME': str(self.home),
            'JCODE_RUNTIME_DIR': str(self.runtime), 'JCODE_SOCKET': str(self.path),
            'JCODE_TEST_SESSION': '1', 'JCODE_DEBUG_CONTROL': '1', 'JCODE_VERIFY_EXEC_RELOAD': '1',
            'JCODE_PROVIDER': 'openai-compatible', 'JCODE_MODEL': 'fake-model',
            'JCODE_OPENAI_COMPAT_API_BASE': f'http://127.0.0.1:{self.provider.server_port}/v1',
            'OPENAI_COMPAT_API_KEY': 'fixture-only', 'JCODE_OPENAI_COMPAT_DEFAULT_MODEL': 'fake-model',
            'JCODE_NO_TELEMETRY': '1', 'JCODE_TELEMETRY': 'off', 'JCODE_NO_AUTO_UPDATE': '1',
        }
        if case in ('stop', 'new-prompt', 'crash-claimed', 'desktop'):
            self.env['JCODE_TEST_HEADLESS_STARTUP_RECOVERY_DELAY_MS'] = '8000' if case == 'desktop' else '2000'
        self.log = open(self.root / 'daemon.log', 'w+')
        self.start()

    def start(self):
        self.daemon = subprocess.Popen([self.binary, '--no-update', '--no-selfdev', '--debug-socket',
                                        'serve', '--socket', str(self.path)],
                                       env=self.env, cwd=self.root, stdout=self.log, stderr=self.log, start_new_session=True)
        eventually('socket ready', lambda: self.path.exists())

    def connect(self, target=None):
        wire = Wire(self.path)
        self.wires.append(wire)
        req = {'type': 'subscribe', 'id': 1, 'working_dir': str(self.root),
               'continue_on_disconnect': True, 'supports_turn_identity': True,
               'history_base': {'count': 0, 'fingerprint': 0, 'recent_messages': 0}}
        if target:
            req['target_session_id'] = target
        wire.send(req)
        history = wire.event(lambda e: e.get('type') == 'history' or (not target and e.get('type') == 'session'))
        if target:
            assert history['messages'] == [], 'bounded attach must negotiate no transcript'
        return wire, history['session_id']

    def interrupt_and_reload(self):
        wire, session = self.connect()
        wire.send({'type': 'message', 'id': 2, 'content': f'Finish fixture {self.case}'})
        eventually('original inference held', lambda: len(self.calls) == 1)
        if self.case.startswith('ask'):
            wire.event(lambda e: e.get('type') == 'ask_user_request')
        self.initial_starts = self.turn_starts(session)
        assert len(self.initial_starts) == 1, 'original turn must have a canonical TURN_START'
        debug = Wire(self.runtime / 'app-debug.sock')
        try:
            debug.send({'type': 'debug_command', 'id': 3, 'command': 'server:reload:verify-exec'})
            result = debug.event(lambda e: e.get('id') == 3)
            assert result['ok'], result
        finally:
            debug.close()
        reload_id = json.loads(result['output'])['request_id']
        trace = self.home / 'reload-traces' / (reload_id + '.jsonl')
        eventually('real exec reload', lambda: trace.exists() and '"exec_start"' in trace.read_text())
        for wire in self.wires:
            wire.close()
        self.wires.clear()
        return session

    def record(self, session):
        try:
            return json.loads((self.home / 'reload-recovery' / (session + '.json')).read_text())
        except (OSError, ValueError):
            return None

    def turn_starts(self, session):
        files = [self.root / 'daemon.log', *list((self.home / 'logs').rglob('*.log'))]
        starts = set()
        for path in files:
            for line in path.read_text(errors='replace').splitlines():
                if 'event=TURN_START' in line and f'session={session} ' in line:
                    runtime = re.search(r'\bruntime=(\S+)', line)
                    turn = re.search(r'\bturn=(\S+)', line)
                    if runtime and turn:
                        starts.add((runtime[1], turn[1]))
        return starts

    def recovery_starts(self, session):
        return self.turn_starts(session) - self.initial_starts

    def continuation_count(self):
        with self.lock:
            return max(0, len(self.calls) - 1)

    def close(self):
        if self.bridge:
            self.bridge.terminate()
            self.bridge.wait(timeout=5)
        for wire in self.wires:
            wire.close()
        self.release.set()
        self.daemon.terminate()
        try:
            self.daemon.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.daemon.kill()
            self.daemon.wait()
        self.provider.shutdown()
        self.log.close()
        self.temp.cleanup()


def run(binary, case):
    fixture = Fixture(binary, case)
    try:
        session = fixture.interrupt_and_reload()
        if case == 'no-client':
            eventually('continuation with no clients', lambda: fixture.continuation_count() == 1, 10)
        elif case.startswith('ask'):
            eventually('ask recovery intent', lambda: fixture.record(session))
            time.sleep(1)
            assert fixture.continuation_count() == 0, 'unanswered ask must not invoke the model'
            assert not fixture.recovery_starts(session), 'unattached unanswered ask must not admit a model turn'
            wire, _ = fixture.connect(session)
            if case == 'ask-new-prompt':
                wire.send({'type': 'message', 'id': 99, 'content': 'New authoritative user prompt'})
                eventually('ask superseded', lambda: fixture.record(session)['status'] == 'superseded')
                eventually('new prompt bypasses old ask', lambda: fixture.continuation_count() == 1)
                time.sleep(1)
                assert fixture.continuation_count() == 1
                print(json.dumps({'case': case, 'result': 'PASS', 'status': 'superseded'}))
                return
            ask = wire.event(lambda e: e.get('type') == 'ask_user_request')
            response = {} if case == 'ask-cancel' else {'answers': [{'selected': ['Continue (Recommended)']}]}
            wire.send({'type': 'ask_user_response', 'id': 88, 'request_id': ask['request_id'], 'response': response})
            if case == 'ask-cancel':
                eventually('declined ask cancellation', lambda: fixture.record(session)['status'] == 'cancelled')
                time.sleep(1)
                assert fixture.continuation_count() == 0
                print(json.dumps({'case': case, 'result': 'PASS', 'status': 'cancelled'}))
                return
            eventually('answered ask continues once', lambda: fixture.continuation_count() == 1)
        elif case == 'desktop':
            api = fixture.runtime / 'api.sock'
            bridge = str(Path(fixture.binary).with_name('jcode-harness-api-bridge'))
            fixture.bridge = subprocess.Popen([bridge, str(api), str(fixture.path)], env=fixture.env, stdout=fixture.log, stderr=fixture.log)
            eventually('isolated bridge ready', lambda: api.exists())
            target = Path.home() / '.jcode/shared-build/target'
            candidates = [p for pattern in ('debug/deps/jcode_desktop_harness-*', 'justrust-slots/*/debug/deps/jcode_desktop_harness-*') for p in target.glob(pattern) if p.is_file() and os.access(p, os.X_OK)]
            test_binary = os.environ.get('JCODE_RECOVERY_DESKTOP_TEST_BIN') or str(max(candidates, key=lambda p: p.stat().st_mtime))
            env = dict(fixture.env, JCODE_RECOVERY_ACCEPTANCE_API_SOCKET=str(api), JCODE_RECOVERY_ACCEPTANCE_SESSION=session)
            result = subprocess.run([test_binary, 'real_daemon_bridge_desktop_recovery_without_user_input', '--ignored', '--nocapture'], env=env, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=45)
            assert result.returncode == 0 and 'DESKTOP_RECOVERY_RUNNING' in result.stdout, result.stdout
            eventually('Desktop observes daemon-owned continuation', lambda: fixture.continuation_count() == 1)
        elif case in ('stop', 'new-prompt', 'crash-claimed'):

            claimed = eventually('durable claim', lambda: fixture.record(session) if fixture.record(session) and fixture.record(session)['status'] == 'claimed' else None)
            logical_id = claimed['continuation_turn_id']
            assert fixture.continuation_count() == 0
            if case == 'crash-claimed':
                fixture.daemon.kill()
                fixture.daemon.wait(timeout=5)
                fixture.env.pop('JCODE_TEST_HEADLESS_STARTUP_RECOVERY_DELAY_MS')
                fixture.path.unlink(missing_ok=True)
                fixture.start()
                eventually('claimed crash resumes once', lambda: fixture.continuation_count() == 1)
                final = fixture.record(session)
                assert final['continuation_turn_id'] == logical_id and final['attempts'] >= 2, final
            else:
                wire, _ = fixture.connect(session)
                if case == 'stop':
                    wire.send({'type': 'cancel', 'id': 77})
                    outcome = 'cancelled'
                else:
                    wire.send({'type': 'message', 'id': 77, 'content': 'New authoritative user prompt'})
                    outcome = 'superseded'
                eventually(outcome, lambda: fixture.record(session)['status'] == outcome)
                time.sleep(3)
                assert fixture.continuation_count() == (0 if case == 'stop' else 1), fixture.calls
                assert len(fixture.recovery_starts(session)) == (0 if case == 'stop' else 1)
                print(json.dumps({'case': case, 'result': 'PASS', 'status': outcome, 'continuation_turn_id': logical_id}))
                return
        elif case == 'two-clients':
            fixture.connect(session)
            fixture.connect(session)
            eventually('single continuation for two clients', lambda: fixture.continuation_count() == 1)
        else:
            raise ValueError(case)
        time.sleep(1)
        assert fixture.continuation_count() == 1, fixture.calls
        record = fixture.record(session)
        assert record and record['status'] == 'started', record
        assert record['role'] == 'interrupted_peer', record
        starts = fixture.recovery_starts(session)
        assert len(starts) == 1, f'exactly one canonical continuation TURN_START: {starts}'
        print(json.dumps({'case': case, 'result': 'PASS', 'session': session,
                          'continuation_turn_id': record.get('continuation_turn_id'), 'attempts': record.get('attempts'), 'turn_starts': len(starts)}))
    except Exception:
        print('fixture:', fixture.root, file=sys.stderr)
        print('daemon:', (fixture.root / 'daemon.log').read_text(errors='replace')[-8000:], file=sys.stderr)
        for path in (fixture.home / 'logs').glob('*.log'):
            print(path.read_text(errors='replace')[-16000:], file=sys.stderr)
        raise
    finally:
        fixture.close()


if __name__ == '__main__':
    binary = Path(sys.argv[1]).resolve()
    with tempfile.TemporaryDirectory(prefix='recovery-suite-', dir=os.environ['JCODE_SCRATCH_DIR']) as directory:
        pinned = Path(directory) / 'jcode'
        os.link(binary, pinned)
        bridge = binary.with_name('jcode-harness-api-bridge')
        if bridge.exists():
            os.link(bridge, pinned.with_name(bridge.name))
        digest = hashlib.sha256()
        with pinned.open('rb') as artifact:
            for chunk in iter(lambda: artifact.read(1024 * 1024), b''):
                digest.update(chunk)
        print(json.dumps({'candidate_sha256': digest.hexdigest()}), flush=True)
        for case in sys.argv[2:] or ['desktop', 'no-client', 'two-clients', 'stop', 'new-prompt', 'crash-claimed', 'ask', 'ask-cancel', 'ask-new-prompt']:
            run(str(pinned), case)
