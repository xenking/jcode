#!/usr/bin/env bash
# Actual CLI/daemon group transfer on two isolated homes and repository disks.
set -euo pipefail
if [[ ${1:-} != --isolated ]]; then
  BINARY=$(realpath -e "${JCODE_BIN:?set JCODE_BIN to the binary under test}")
  SCRIPT=$(realpath -e "$0")
  W=$(mktemp -d "${JCODE_SCRATCH_DIR:-${TMPDIR:-/tmp}}/cloud-group-test.XXXXXX")
  mkdir -p "$W/home" "$W/runtime" "$W/tmp"
  exec env -i PATH=/usr/local/bin:/usr/bin:/bin LANG=C.UTF-8 HOME="$W/home" \
    TMPDIR="$W/tmp" XDG_RUNTIME_DIR="$W/runtime" JCODE_RUNTIME_DIR="$W/runtime" \
    JCODE_HOME="$W/home/.jcode" JCODE_NO_AUTO_UPDATE=1 JCODE_NO_TELEMETRY=1 \
    JCODE_NON_INTERACTIVE=1 JCODE_TEST_SESSION=1 W="$W" JCODE_BIN="$BINARY" bash "$SCRIPT" --isolated
fi
RUNTIME=$(mktemp -d /tmp/jcg.XXXXXX)
mkdir "$RUNTIME/local" "$RUNTIME/cloud"
export JCODE_RUNTIME_DIR="$RUNTIME/local" XDG_RUNTIME_DIR="$RUNTIME/local"
PIDS=()
cleanup() {
  local status=$?
  trap - EXIT
  for pid in "${PIDS[@]}"; do kill -TERM -- "-$pid" 2>/dev/null || true; done
  # Bound teardown even if a fixture daemon cannot complete graceful shutdown.
  for pid in "${PIDS[@]}"; do kill -KILL -- "-$pid" 2>/dev/null || true; done
  for pid in "${PIDS[@]}"; do wait "$pid" 2>/dev/null || true; done
  rm -rf -- "$RUNTIME"
  echo "Acceptance artifacts: $W"
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
for prerequisite in bwrap python3 git setsid rg timeout; do command -v "$prerequisite" >/dev/null; done
REPO="$W/local/project"
mkdir -p "$REPO" "$W/cloud/projects" "$W/cloud/home" "$JCODE_HOME/sessions" "$JCODE_HOME/todos" "$JCODE_RUNTIME_DIR/durable-state/swarm"
mkdir -m 700 "$W/cloud/home/.jcode"
chmod 700 "$JCODE_HOME"
printf '[display]\ndebug_socket = false\n' > "$JCODE_HOME/config.toml"
cp "$JCODE_HOME/config.toml" "$W/cloud/home/.jcode/config.toml"
git -C "$REPO" init -q
git -C "$REPO" config user.name 'Group migration fixture'
git -C "$REPO" config user.email 'fixture@example.invalid'
printf 'base\n' > "$REPO/file.txt"
git -C "$REPO" add .
git -C "$REPO" commit -qm base
printf 'local uncommitted\n' > "$REPO/file.txt"
printf 'untracked\n' > "$REPO/untracked.txt"
mkdir -p "$REPO/.jcode"
printf '{"requirements":[{"kind":"env_var","name":"GROUP_RESOURCE_TOKEN"}]}\n' > "$REPO/.jcode/cloud-environment.json"
export GROUP_RESOURCE_TOKEN=synthetic-group-resource
python3 - "$JCODE_HOME" "$REPO" "$JCODE_RUNTIME_DIR" <<'PY'
import datetime,json,pathlib,sys,time
home,repo,runtime=map(pathlib.Path,sys.argv[1:])
now=datetime.datetime.now(datetime.timezone.utc).isoformat()
for sid in ['group-root','group-worker','independent','excluded']:
    session={'id':sid,'title':sid,'created_at':now,'updated_at':now,'working_dir':str(repo),'status':'Active',
             'messages':[{'id':'message-'+sid,'role':'user','timestamp':now,'content':[{'type':'text','text':'Fixture '+sid}]}]}
    (home/'sessions'/f'{sid}.json').write_text(json.dumps(session))
    (home/'todos'/f'{sid}.json').write_text(json.dumps({'items':[],'goal':sid}))
def member(sid,role,owner):
    return {'session_id':sid,'working_dir':str(repo),'swarm_id':'fixture-swarm','swarm_enabled':True,
            'status':'ready','detail':None,'friendly_name':sid,'report_back_to_session_id':owner,
            'role':role,'is_headless':False}
swarm={'swarm_id':'fixture-swarm','coordinator_session_id':'group-root','updated_at_unix_ms':int(time.time()*1000),
       'plan':{'items':[{'id':'work','content':'Continue fixture','status':'pending','priority':'normal','assigned_to':'group-worker'}],
               'version':7,'participants':['group-root','group-worker']},
       'members':[member('group-root','coordinator',None),member('group-worker','agent','group-root')]}
(runtime/'durable-state'/'swarm'/'fixture-swarm.json').write_text(json.dumps(swarm))
PY
{
  printf '#!/usr/bin/env bash\nset -euo pipefail\n'
  printf 'W=%q\nBINARY=%q\nRUNTIME=%q\n' "$W" "$JCODE_BIN" "$RUNTIME"
  cat <<'TRANSPORT'
run_remote() {
  exec bwrap --die-with-parent --ro-bind / / --tmpfs /tmp --proc /proc --dev /dev \
    --bind "$W/cloud/projects" "$W/local" --bind "$W/cloud/home" "$W/home" \
    --bind "$RUNTIME/cloud" "$RUNTIME/cloud" \
    --ro-bind "$BINARY" /tmp/jcode-group-test --clearenv \
    --setenv PATH /usr/local/bin:/usr/bin:/bin --setenv HOME "$W/home" \
    --setenv JCODE_HOME "$W/home/.jcode" --setenv JCODE_NO_AUTO_UPDATE 1 \
    --setenv JCODE_NO_TELEMETRY 1 --setenv JCODE_NON_INTERACTIVE 1 --setenv JCODE_TEST_SESSION 1 \
    --setenv JCODE_RUNTIME_DIR "$RUNTIME/cloud" --setenv XDG_RUNTIME_DIR "$RUNTIME/cloud" \
    --setenv JCODE_SOCKET "$RUNTIME/cloud/cloud.sock" \
    --chdir / bash -c "$1"
}
if [[ "$1" == *env*receive* ]]; then printf 'receive\n' >> "$W/environment-transfers"; fi
if [[ -e "$W/lose-activation-response" && "$1" == *activate* ]]; then
  (run_remote "$1") > "$W/activation-output" 2>> "$W/activation-error"
  exit 255
fi
if [[ "$1" == *group-export* && -e "$W/lose-export-response" ]]; then
  (run_remote "$1") > "$W/export-output" 2>> "$W/export-error"
  exit 255
fi
if [[ "$1" == *group-export* && -e "$W/kill-return-client" ]]; then
  (run_remote "$1") > "$W/replayed-export" 2>> "$W/export-error"
  kill -KILL "$PPID"
  exit 255
fi
run_remote "$1"
TRANSPORT
} > "$W/transport"
chmod +x "$W/transport"
export JCODE_SOCKET="$RUNTIME/local/local.sock"
setsid "$JCODE_BIN" --no-update --no-selfdev serve > "$W/local-daemon.log" 2>&1 &
PIDS+=("$!")
setsid "$W/transport" '/tmp/jcode-group-test --no-update --no-selfdev serve' > "$W/cloud-daemon.log" 2>&1 &
PIDS+=("$!")
python3 - "$RUNTIME/local/local-debug.sock" "$RUNTIME/cloud/cloud-debug.sock" <<'PY'
import pathlib,sys,time
for name in sys.argv[1:]:
    until=time.monotonic()+20
    while not pathlib.Path(name).exists():
        if time.monotonic()>until: raise AssertionError('daemon did not start: '+name)
        time.sleep(.05)
PY
if "$JCODE_BIN" --no-update --no-selfdev debug server:help --socket "$JCODE_SOCKET" > "$W/debug-output" 2> "$W/debug-error"; then
  echo 'FAIL: group migration must not require unrestricted debug control' >&2
  exit 1
fi
grep -q 'Debug control is disabled' "$W/debug-error"
cd "$REPO"
move_group() {
  timeout --kill-after=5s 90s "$JCODE_BIN" --no-update --no-selfdev cloud move --session group-root \
    --host fixture-cloud --remote-binary /tmp/jcode-group-test --transport "$W/transport" \
    --include-session independent --json "$@"
}
move_group --dry-run > "$W/preview.json"
[[ ! -e "$JCODE_HOME/session_leases/group-root.json" ]]
[[ ! -e "$JCODE_HOME/session_leases/group-worker.json" ]]
[[ ! -e "$JCODE_HOME/session_leases/independent.json" ]]
# A successful remote activation with a lost response must never revive local agents.
touch "$W/lose-activation-response"
if move_group > "$W/uncertain.json" 2> "$W/uncertain.err"; then
  echo 'FAIL: simulated lost activation response reported success' >&2
  exit 1
fi
python3 - "$JCODE_HOME" <<'PY'
import json,pathlib,sys
home=pathlib.Path(sys.argv[1])
for sid in ['group-root','group-worker','independent']:
    lease=json.loads((home/'session_leases'/f'{sid}.json').read_text())
    assert lease['away_host']=='fixture-cloud',lease
assert not (home/'session_leases'/'excluded.json').exists()
PY
rm "$W/lose-activation-response"
move_group > "$W/recovered.json"
python3 - "$JCODE_HOME" "$W/cloud/home/.jcode" "$RUNTIME/cloud/durable-state/swarm" <<'PY'
import json,pathlib,sys
local,cloud,state=map(pathlib.Path,sys.argv[1:])
for sid in ['group-root','group-worker','independent']:
    original=json.loads((local/'sessions'/f'{sid}.json').read_text())
    moved=json.loads((cloud/'sessions'/f'{sid}.json').read_text())
    assert moved['id']==sid
    original_message=dict(original['messages'][0])
    moved_message=dict(moved['messages'][0])
    from datetime import datetime
    assert datetime.fromisoformat(original_message.pop('timestamp'))==datetime.fromisoformat(moved_message.pop('timestamp').replace('Z','+00:00'))
    assert moved_message==original_message
    assert moved['migration_epoch']==1,moved
    text='\n'.join(b.get('text','') for m in moved['messages'] for b in m['content'])
    if sid=='group-root':
        assert 'setup once for the workspace' in text,text
    else:
        assert 'do not independently repeat workspace setup' in text,text
    assert (cloud/'todos'/f'{sid}.json').read_bytes()==(local/'todos'/f'{sid}.json').read_bytes()
    assert not json.loads((cloud/'session_leases'/f'{sid}.json').read_text()).get('away_host')
assert not (cloud/'sessions'/'excluded.json').exists()
swarm=json.loads((state/'fixture-swarm.json').read_text())
assert swarm['coordinator_session_id']=='group-root',swarm
assert {'group-root','group-worker'}=={m['session_id'] for m in swarm['members']},swarm
assert swarm['plan']['version']==7,swarm
assert swarm['plan']['items'][0]['assigned_to']=='group-worker',swarm
PY
cmp "$REPO/file.txt" "$W/cloud/projects/project/file.txt"
cmp "$REPO/untracked.txt" "$W/cloud/projects/project/untracked.txt"
python3 - "$W/cloud/home/.jcode/cloud-environment" "$W/environment-transfers" <<'PY'
import json,pathlib,sys
files=list(pathlib.Path(sys.argv[1]).glob('*.json'))
assert len(files)==1,files
assert json.loads(files[0].read_text())['GROUP_RESOURCE_TOKEN']=='synthetic-group-resource'
assert pathlib.Path(sys.argv[2]).read_text().splitlines()==['receive'],'setup must replay once for the whole group'
PY
if rg -q 'synthetic-group-resource' "$W/preview.json" "$W/recovered.json" "$W/uncertain.err"; then
  echo 'FAIL: opaque environment value appeared in command output' >&2
  exit 1
fi
printf 'PASS: real-daemon swarm selection, explicit independent inclusion, preview, shared repository, preserved metadata and uncertain activation recovery\n'
return_group() {
  timeout --kill-after=5s 90s "$JCODE_BIN" --no-update --no-selfdev cloud return --session "$1" \
    --remote-binary /tmp/jcode-group-test --transport "$W/transport" --json "${@:2}"
}
expect_failure() {
  local name=$1; shift
  if "$@" > "$W/$name.out" 2> "$W/$name.err"; then
    echo "FAIL: $name unexpectedly succeeded" >&2
    exit 1
  fi
}
expect_failure wrong-host return_group group-worker --host another-cloud
grep -q 'original host' "$W/wrong-host.err"
expect_failure unselected return_group excluded
grep -q 'was never moved' "$W/unselected.err"
expect_failure single-export "$W/transport" '/tmp/jcode-group-test --no-update --no-selfdev cloud export --session group-worker --epoch 2'
grep -q 'single-session operations are unsafe' "$W/single-export.err"
printf 'PASS: wrong host, excluded session and direct partial export refused\n'

CLOUD_REPO="$W/cloud/projects/project"
printf 'cloud commit\n' > "$CLOUD_REPO/committed.txt"
git -C "$CLOUD_REPO" add committed.txt
git -C "$CLOUD_REPO" commit -qm 'committed on cloud'
printf 'cloud dirty\n' > "$CLOUD_REPO/cloud.txt"
printf 'laptop dirty\n' > "$REPO/laptop.txt"
python3 - "$W/cloud/home/.jcode" <<'PY'
import json,pathlib,sys
home=pathlib.Path(sys.argv[1])
p=home/'sessions'/'group-worker.json'
session=json.loads(p.read_text())
session['messages'].append({'id':'cloud-message','role':'user','timestamp':session['updated_at'],
                            'content':[{'type':'text','text':'Edited on cloud'}]})
p.write_text(json.dumps(session))
(home/'todos'/'group-worker.json').write_text(json.dumps({'items':[],'goal':'cloud todo'}))
PY

assert_leases() {
  python3 - "$JCODE_HOME" "$W/cloud/home/.jcode" "$1" <<'PY'
import json,pathlib,sys
local,cloud=map(pathlib.Path,sys.argv[1:3])
state=sys.argv[3]
for sid in ['group-root','group-worker','independent']:
    here=json.loads((local/'session_leases'/f'{sid}.json').read_text())
    away=json.loads((cloud/'session_leases'/f'{sid}.json').read_text())
    if state=='active':
        assert here.get('away_host')=='fixture-cloud' and not away.get('away_host'),(here,away)
    elif state=='exported':
        assert here.get('away_host') and away.get('away_host')=='return-pending',(here,away)
    else:
        assert not here.get('away_host') and away.get('away_host')=='returned-complete',(here,away)
        assert here['epoch']==away['epoch']==2,(here,away)
assert not (local/'session_leases'/'excluded.json').exists()
PY
}
assert_leases active
touch "$W/lose-export-response"
expect_failure lost-export return_group group-worker
grep -q 'group export unconfirmed' "$W/lost-export.err"
assert_leases exported
rm "$W/lose-export-response"
printf 'changed after export, not part of return\n' > "$CLOUD_REPO/too-late.txt"
printf 'PASS: lost export response fences every member on both machines\n'

touch "$W/kill-return-client"
expect_failure killed-return return_group group-worker
[[ -s "$W/replayed-export" ]]
cmp "$W/export-output" "$W/replayed-export"
assert_leases exported
rm "$W/kill-return-client"
printf 'PASS: SIGKILL after deactivation replays the identical group archive\n'

# Force interruption after Git applied but before it checkpointed. Then stop
# once more after writing some local transcripts, before publishing ownership.
mkdir -p "$JCODE_HOME/cloud-returns/group-root/2/group-merge-applied"
expect_failure interrupted-merge return_group group-root
grep -qx 'cloud dirty' "$REPO/cloud.txt"
assert_leases exported
rmdir "$JCODE_HOME/cloud-returns/group-root/2/group-merge-applied"
rm "$JCODE_HOME/todos/independent.json"
mkdir "$JCODE_HOME/todos/independent.json"
expect_failure interrupted-install return_group group-root
grep -q 'Edited on cloud' "$JCODE_HOME/sessions/group-worker.json"
assert_leases exported
rmdir "$JCODE_HOME/todos/independent.json"

return_group group-worker > "$W/return.json"
assert_leases returned
return_group group-root > "$W/returned-again.json"
cmp "$W/return.json" "$W/returned-again.json"
grep -qx 'laptop dirty' "$REPO/laptop.txt"
grep -qx 'cloud dirty' "$REPO/cloud.txt"
grep -qx 'cloud commit' "$REPO/committed.txt"
[[ $(git -C "$REPO" log -1 --format=%s) == 'committed on cloud' ]]
[[ ! -e "$REPO/too-late.txt" ]]
python3 - "$JCODE_HOME" "$JCODE_RUNTIME_DIR/durable-state/swarm" "$W/return.json" <<'PY'
import json,pathlib,sys
home,state,report=map(pathlib.Path,sys.argv[1:])
result=json.loads(report.read_text())
assert set(result['members'])=={'group-root','group-worker','independent'},result
assert result['merge']['result']=='applied',result
worker=json.loads((home/'sessions/group-worker.json').read_text())
assert sum(m['id']=='cloud-message' for m in worker['messages'])==1,worker
assert sum('Coordinated group returned' in json.dumps(m) for m in worker['messages'])==1,worker
assert json.loads((home/'todos/group-worker.json').read_text())['goal']=='cloud todo'
swarm=json.loads((state/'fixture-swarm.json').read_text())
assert swarm['coordinator_session_id']=='group-root',swarm
assert swarm['plan']['items'][0]['assigned_to']=='group-worker',swarm
PY
printf 'PASS: interrupted Git merge and partial transcript install recover without double merge or half-local group\n'
printf 'PASS: returning a worker returns every migrated member but no excluded session\n'
