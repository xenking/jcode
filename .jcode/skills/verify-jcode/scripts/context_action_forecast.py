#!/usr/bin/env python3
"""Assert the real daemon's read-only manual action forecast, without a provider call."""
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time


binary = Path(sys.argv[1]).resolve(strict=True)


def read_event(path, request_type, response_type, **fields):
    with socket.socket(socket.AF_UNIX) as client:
        client.settimeout(8)
        client.connect(str(path))
        client.sendall((json.dumps(dict(type=request_type, id=1, **fields)) + '\n').encode())
        with client.makefile('r') as stream:
            for line in stream:
                event = json.loads(line)
                if event.get('id') == 1 and event['type'] in (response_type, 'error'):
                    if event['type'] == 'error':
                        raise RuntimeError(event)
                    return event
    raise RuntimeError(f'No {response_type} after {request_type}')


with tempfile.TemporaryDirectory(prefix='jcf-', dir='/tmp') as scratch:
    root = Path(scratch)
    for folder in ('home', 'data', 'work'):
        (root / folder).mkdir()
    path = root / 'daemon.sock'
    env = {
        'PATH': os.defpath, 'HOME': str(root / 'home'), 'JCODE_HOME': str(root / 'data'),
        'JCODE_RUNTIME_DIR': str(root), 'TMPDIR': str(root), 'JCODE_TEST_SESSION': '1',
        'JCODE_NO_MENUBAR': '1', 'JCODE_NO_TELEMETRY': '1', 'JCODE_NO_AUTO_UPDATE': '1',
        'OPENAI_API_KEY': 'sk-isolated-context-forecast-fixture',
        'JCODE_OPENAI_API_BASE': 'http://127.0.0.1:1/v1',
    }
    (root / 'data/config.toml').write_text('[provider]\nopenai_transport = "https"\n')
    command = [binary, '--no-update', '--no-selfdev', '--provider', 'openai-api',
               '--model', 'gpt-6.1-sol', '--socket', path, '--tool-profile', 'none', 'serve']
    with (root / 'daemon.log').open('w+') as log:
        with subprocess.Popen(command, cwd=root / 'work', env=env, stdout=log,
                              stderr=subprocess.STDOUT) as daemon:
            try:
                deadline = time.monotonic() + 15
                while not path.is_socket() and daemon.poll() is None and time.monotonic() < deadline:
                    time.sleep(0.05)
                assert path.is_socket() and daemon.poll() is None, 'private daemon not ready'
                with socket.socket(socket.AF_UNIX) as attached:
                    attached.settimeout(15)
                    attached.connect(str(path))
                    attached.sendall((json.dumps(dict(type='subscribe', id=1,
                                                       working_dir=str(root / 'work'))) + '\n').encode())
                    with attached.makefile('r') as stream:
                        session_id = None
                        for line in stream:
                            event = json.loads(line)
                            if event['type'] == 'session':
                                session_id = event['session_id']
                            if session_id and event.get('id') == 1 and event['type'] == 'done':
                                break
                        else:
                            raise RuntimeError('Could not open a blank session')
                        breakdown = read_event(path, 'session_context_breakdown',
                                               'context_breakdown', session_id=session_id)
                        assert breakdown['meter']['state'] == 'live', breakdown['meter']
                        forecast = breakdown['action_forecast']
                        print(json.dumps(forecast), flush=True)
                        assert forecast.get('compact_status') == 'no_eligible', forecast
                        assert forecast.get('shake_status') == 'no_eligible', forecast
                        assert forecast['compact'] is None and forecast['shake'] is None, forecast
                        shake = read_event(path, 'session_shake_tools', 'shake_tools',
                                           session_id=session_id, action={'kind': 'preview'})
                        assert shake['result']['kind'] == 'no_change', shake
                        attached.sendall(b'{"type":"compact","id":2}\n')
                        for line in stream:
                            compact = json.loads(line)
                            if compact['type'] == 'compact_result' and not compact['success']:
                                break
                        else:
                            raise RuntimeError('No completion from manual Compact')
                        assert 'Not enough messages to compact' in compact['message'], compact
                        print('PASS: both forecasts match the actions on this private session')
            finally:
                if daemon.poll() is None:
                    daemon.terminate()
                    try:
                        daemon.wait(timeout=8)
                    except subprocess.TimeoutExpired:
                        daemon.kill()
                        daemon.wait()
                if sys.exc_info()[0] is not None:
                    log.seek(0)
                    print(log.read(), file=sys.stderr)
