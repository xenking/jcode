#!/usr/bin/env python3
"""Drive native OpenAI Responses and saved-to-live context policy in isolation."""
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

MODEL = 'gpt-6.1-sol'
ACK = 'native loopback acknowledged'
BASELINE, FIXED = map(lambda arg: Path(arg).resolve(strict=True), sys.argv[1:3])
EVIDENCE = Path(sys.argv[3]).resolve()
EVIDENCE.mkdir(mode=0o700)


def record(name, data):
    (EVIDENCE / name).write_text(json.dumps(data, indent=2) + '\n')


class Responses(BaseHTTPRequestHandler):
    requests = []

    def log_message(self, *_args):
        pass

    def reply(self, status, body, media='application/json'):
        self.send_response(status)
        self.send_header('content-type', media)
        self.send_header('content-length', str(len(body)))
        self.end_headers()
        try:
            self.wfile.write(body)
        except BrokenPipeError:
            if self.command != 'GET':
                raise

    def do_GET(self):
        models = {'data': [{'id': MODEL, 'object': 'model'}]}
        self.reply(200, json.dumps(models).encode())

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers['content-length'])))
        if self.path != '/v1/responses':
            return self.reply(404, b'{}')
        self.requests.append({
            'path': self.path, 'model': request['model'],
            'probe': 'native-compaction-probe' in json.dumps(request['input']),
            'context_management': request.get('context_management'),
        })
        record('requests.json', self.requests)
        response = {
            'id': f'resp_{len(self.requests)}', 'model': MODEL, 'status': 'completed',
            'usage': {'input_tokens': 130000, 'output_tokens': 3, 'total_tokens': 130003},
        }
        events = [{'type': 'response.output_text.delta', 'delta': ACK},
                  {'type': 'response.completed', 'response': response}]
        self.reply(200, ''.join(f'data: {json.dumps(event)}\n\n' for event in events).encode(),
                   'text/event-stream')


def socket_request(path, kind, **fields):
    client = socket.socket(socket.AF_UNIX)
    try:
        client.settimeout(10)
        client.connect(path)
        client.sendall((json.dumps(dict(type=kind, id=1, **fields)) + '\n').encode())
        return client
    except BaseException:
        client.close()
        raise


def await_event(client, kind):
    with client.makefile('rb') as stream:
        for raw in stream:
            event = json.loads(raw)
            if event.get('id') == 1:
                if event['type'] == kind:
                    return event
                if event['type'] == 'error':
                    raise RuntimeError(str(event))
    raise RuntimeError(f'Daemon closed socket before {kind}')


def check_meter(path, session_id, state, threshold):
    deadline = time.monotonic() + 8
    while True:
        with socket_request(path, 'session_context_breakdown', session_id=session_id,
                            summary_only=True) as client:
            meter = await_event(client, 'context_breakdown')['meter']
        if meter['state'] == state or time.monotonic() >= deadline:
            break
        time.sleep(0.05)
    record(state + '-meter.json', meter)
    assert meter['state'] == state and meter['compaction_mode'] == 'provider_native', meter
    assert meter['compaction_threshold_tokens'] == threshold, meter
    assert meter['model_window_tokens'] == meter['effective_budget_tokens'] == 1_000_000, meter
    assert meter['session_cap_tokens'] is None and meter['occupied_tokens'] == 130_000, meter


def prove(root, server):
    for part in ['home', 'data', 'work']:
        (root / part).mkdir()
    path = str(root / 'daemon.sock')
    registered_home = str(root / 'home')
    env = {
        'PATH': os.defpath, 'HOME': registered_home,
        'JCODE_HOME': str(root / 'data'), 'JCODE_RUNTIME_DIR': str(root),
        'XDG_CONFIG_HOME': registered_home + '/.config', 'XDG_CACHE_HOME': registered_home + '/.cache',
        'TMPDIR': str(root), 'JCODE_TEST_SESSION': '1', 'JCODE_NO_MENUBAR': '1',
        'JCODE_NO_TELEMETRY': '1', 'JCODE_TELEMETRY': 'off', 'JCODE_NO_AUTO_UPDATE': '1',
        'GIT_CONFIG_NOSYSTEM': '1', 'GIT_CONFIG_GLOBAL': '/dev/null', 'GIT_TERMINAL_PROMPT': '0',
        'OPENAI_API_KEY': 'sk-isolated-compaction-fixture',
        'JCODE_OPENAI_API_BASE': f'http://127.0.0.1:{server.server_port}/v1',
        'JCODE_MAX_RETRIES': '1', 'JCODE_STREAM_IDLE_TIMEOUT_SECS': '5',
    }
    (root / 'data/config.toml').write_text('''[provider]
openai_transport = 'https'
openai_native_compaction_mode = 'auto'
openai_native_compaction_threshold_tokens = 200000
request_compression = 'off'
[compaction]
reactive_threshold = 0.8
reactive_threshold_tokens = 400000
''')
    base = ['--no-update', '--no-selfdev', '--provider', 'openai-api', '--model', MODEL,
            '--socket', path, '--tool-profile', 'none']

    def run(binary, label, expected_threshold, session_id=None):
        resume = ['--resume', session_id] if session_id else []
        start = len(Responses.requests)
        argv = [binary, *base, *resume, 'run', '--json', f'native-compaction-probe-{label}']
        result = subprocess.run(argv, cwd=root / 'work', env=env, capture_output=True, text=True, timeout=55)
        record(label + '-command.json', {'argv': list(map(str, argv)), 'exit_code': result.returncode})
        (EVIDENCE / (label + '.stderr')).write_text(result.stderr)
        (EVIDENCE / (label + '.stdout')).write_text(result.stdout)
        result.check_returncode()
        output = json.loads(result.stdout)
        assert output['text'] == ACK and output['provider'] == 'OpenAI' and output['model'] == MODEL, output
        assert Responses.requests[start:] == [{
            'path': '/v1/responses', 'model': MODEL, 'probe': True,
            'context_management': [{'type': 'compaction', 'compact_threshold': expected_threshold}],
        }], Responses.requests[start:]
        return output['session_id']

    session_id = run(BASELINE, 'before', 200_000)
    with (EVIDENCE / 'daemon.log').open('w') as log:
        daemon = subprocess.Popen([FIXED, *base, 'serve'], cwd=root / 'work', env=env,
                                  stdout=log, stderr=subprocess.STDOUT)
        try:
            deadline = time.monotonic() + 20
            while not Path(path).is_socket() and daemon.poll() is None and time.monotonic() < deadline:
                time.sleep(0.05)
            assert Path(path).is_socket() and daemon.poll() is None
            before = len(Responses.requests)
            check_meter(path, session_id, 'saved', 200_000)
            with socket_request(path, 'subscribe', working_dir=str(root / 'work'),
                                target_session_id=session_id) as attached:
                await_event(attached, 'done')
                check_meter(path, session_id, 'live', 400_000)
            assert len(Responses.requests) == before, 'Reading or attaching triggered model inference'
        finally:
            if daemon.poll() is None:
                daemon.terminate()
                try:
                    daemon.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    daemon.kill()
                    daemon.wait()
    assert run(FIXED, 'after', 400_000, session_id) == session_id
    sidecar, = (p for p in root.rglob(session_id + '.json') if p.parent.name == 'session-telemetry')
    policy = json.loads(sidecar.read_text())['context_policy']
    record('persisted-policy.json', policy)
    assert policy['compaction_threshold_tokens'] == 400_000, policy


def terminate(_signum, _frame):
    raise KeyboardInterrupt


signal.signal(signal.SIGTERM, terminate)
with tempfile.TemporaryDirectory(prefix='jc-native-', dir=os.environ['TMPDIR']) as tmp:
    with ThreadingHTTPServer(('127.0.0.1', 0), Responses) as server:
        threading.Thread(target=server.serve_forever, daemon=True).start()
        try:
            prove(Path(tmp), server)
        finally:
            server.shutdown()
print('PASS: native wire 200000 -> 400000; saved meter 200000 -> live meter 400000 without inference; 400000 persisted')
