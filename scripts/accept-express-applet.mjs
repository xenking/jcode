// Runs the real applet tool and action receiver without inference or a shared daemon.
// bun scripts/accept-express-applet.mjs /absolute/path/to/jcode
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, readFile, rm } from 'node:fs/promises';
import { createConnection } from 'node:net';
import { resolve, join } from 'node:path';
import { tmpdir } from 'node:os';

const bin = resolve(process.argv[2] ?? 'target/debug/jcode');
const dir = await mkdtemp(join(process.platform === 'darwin' ? '/private/tmp' : tmpdir(), 'jcx-'));
const runtime = join(dir,'r'), home = join(dir,'home'), store = join(dir,'store');
for (const p of [runtime,home,store]) await mkdir(p, {mode:0o700});
const sock = join(runtime,'daemon.sock'), debug = join(runtime,'daemon-debug.sock');
const env = {PATH:process.env.PATH, HOME:home, JCODE_HOME:store, JCODE_TEST_SESSION:'1', JCODE_NO_TELEMETRY:'1', JCODE_RUNTIME_DIR:runtime, JCODE_SOCKET:sock, JCODE_DEBUG_CONTROL:'1', JCODE_TEMP_SERVER:'1', JCODE_SERVER_OWNER_PID:String(process.pid), JCODE_API_SOCKET:join(runtime,'api.sock'), ANTHROPIC_API_KEY:'test-only-no-inference', LANG:'en_US.UTF-8'};
function request(path, message, retain=false) {
  return new Promise((ok, reject) => {
    const socket = createConnection(path), timer = setTimeout(() => socket.destroy(Error('private socket timed out')),30000);
    let buffer='', session;
    socket.on('connect', () => socket.write(JSON.stringify(message)+'\n'));
    socket.on('error', reject).on('close', () => { clearTimeout(timer); reject(Error('private socket closed')); });
    socket.setEncoding('utf8').on('data', chunk => {
      buffer += chunk;
      while (buffer.includes('\n')) {
        const i=buffer.indexOf('\n'), line=buffer.slice(0,i); buffer=buffer.slice(i+1);
        try { const value=JSON.parse(line); if(process.env.EXPRESS_SMOKE_TRACE) console.error(message.type, value.type, value.id, value.session_id); if (retain && ['session','history'].includes(value.type) && value.session_id) session=value.session_id; if(value.id!==message.id || value.type==='ack') continue; if(retain){ if(value.type==='error') throw Error(JSON.stringify(value)); if(value.type!=='done' || !session) continue; clearTimeout(timer); ok({socket,session}); } else { ok(value); socket.destroy(); } return; }
        catch(err){ socket.destroy(err); return; }
      }
    });
  });
}
async function until(label,fn) {
  const deadline=Date.now()+15000; let last;
  while(Date.now()<deadline){ try {const v=await fn(); if(v)return v;} catch(e){last=e;} await Bun.sleep(40); }
  throw Error(`Timed out: ${label}: ${last ?? ''}`);
}
async function command(command,session_id){
  const res=await request(debug,{type:'debug_command',id:1,command,session_id});
  assert.equal(res.ok,true,res.output); return JSON.parse(res.output);
}
const daemon=Bun.spawn([bin,'--no-update','--no-selfdev','--provider','anthropic-api','serve','--socket',sock],{env,cwd:home,stdout:'ignore',stderr:'ignore'});
try {
  await until('private daemon',async()=>(await request(sock,{type:'ping',id:2})).type==='pong');
  const {socket:client,session}=await request(sock,{type:'subscribe',id:10,working_dir:home,supports_applets:true,supports_user_response:false},true);
  daemon.exited.then(()=>client.destroy());
  await until('Desktop applet capability', async () => (await command('tools',session)).includes('applet'));
  const tool = args => command(`tool:applet ${JSON.stringify({intent:'Exercise Express applet in private daemon',...args})}`,session);
  const definitions=await command('tools:full',session);
  const applet=definitions.find(d=>d.name==='applet');
  assert.ok(applet.description.includes('plain text'));
  assert.ok(!applet.description.includes('host.send_prompt'));
  const catalog=await tool({action:'catalog'});
  for(const signature of ['ReviewChange','GreetingButton','host.send_prompt','select{bind']) assert.ok(catalog.output.includes(signature));
  assert.match((await tool({action:'list'})).output,/No applets mounted/);
  const snapshot = async() => JSON.parse(await readFile(join(store,'agent_applets',session+'.json'),'utf8')).instances.find(i=>i.id==='review');
  const express='root = ReviewChange("Review patch", "src/ui.rs", "change42")';
  const mounted=await tool({instance:'review',placement:'end',express});
  assert.ok(!mounted.is_error,mounted.output);
  const card=await snapshot(); assert.equal(card.placement.kind,'inline');
  const nodes = value => value && typeof value==='object' ? [value,...Object.values(value).flatMap(nodes)]:[];
  assert.ok(nodes(card.document.view).some(n=>n.type==='input' && n.bind==='note'));
  const button=nodes(card.document.view).find(n=>n.on_press?.action==='review.approve');
  assert.equal(button.on_press.args.change_id,'change42');
  const pending=tool({action:'update',instance:'review',express,wait:true,timeout_seconds:15});
  await until('waiting revision', async()=>(await snapshot()).document.revision>card.document.revision);
  const ack=await request(sock,{type:'applet_action',id:3,session_id:session,instance:'review',source_key:button.key,action:button.on_press,state:{note:'looks good'}});
  assert.equal(ack.type,'done',JSON.stringify(ack));
  const result=await pending; assert.ok(!result.is_error,result.output);
  for (const s of ['review.approve','change42','looks good']) assert.ok(result.output.includes(s),result.output);
  assert.equal((await snapshot()).document.state.note,'looks good');
  console.log(`PASS private daemon: Express (${Buffer.byteLength(express)} bytes) -> native applet view (${Buffer.byteLength(JSON.stringify(card.document.view))} JSON bytes) -> acknowledged action -> waiting agent tool; note persisted. No model inference.`);
} finally {
  daemon.kill(); await Promise.race([daemon.exited, Bun.sleep(2000)]);
  if (daemon.exitCode===null){daemon.kill(9); await daemon.exited;}
  await rm(dir,{recursive:true,force:true});
}
