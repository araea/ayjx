// Uses the installed Pi CLI to verify actual extension discovery, whitelist, schemas and IPC.
// No model request or QQ connection is made. Run: node tests/satori-tools.cjs
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const net = require('node:net');
const {spawn} = require('node:child_process');
(async () => {
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(),'ayjx-pi-tools-'));
  const socket = path.join(tmp,'rpc.sock');
  const output = path.join(tmp,'tools.json');
  const extension = path.resolve(__dirname,'../res/ambient/satori-tools.ts');
  let requests = 0;
  const server = net.createServer(stream => {
    let buf=''; stream.on('data',data=>{
      buf += data;
      if (!buf.includes('\n')) return;
      const request=JSON.parse(buf.trim());
      assert.equal(request.token,'test-only'); requests++;
      stream.end(JSON.stringify({ok:true,result:{message_id:'7837409278651234567'}})+'\n');
    });
  });
  await new Promise(resolve=>server.listen(socket,resolve));
  const checker=path.join(tmp,'check.ts');
  fs.writeFileSync(checker, `
import fs from 'node:fs';
import {rpc} from ${JSON.stringify(extension)};
export default function(pi) {
 pi.on('session_start', async () => {
  const tools = pi.getAllTools().filter(t=>t.name.startsWith('satori_'));
  const receipt = await rpc({id:'test',op:'action',request:{action:'poke',user_id:'42'}});
  fs.writeFileSync(${JSON.stringify(output)},JSON.stringify({active:pi.getActiveTools(),tools,receipt}));
  process.exit(tools.length===3?0:1);
 });
}`);
  try {
    let errors='';
    const child=spawn(process.env.PI_COMMAND || 'pi',['--no-extensions','--no-skills','--no-context-files','--no-session','--tools','read,satori_context,satori_read,satori_action','--extension',extension,'--extension',checker,'-p','--mode','json','startup verification'],{
      env:{...process.env,AYJX_CHAT_SOCKET:socket,AYJX_CHAT_TOKEN:'test-only'},stdio:['ignore','pipe','pipe']});
    child.stdout.resume(); child.stderr.on('data',data=>errors+=data);
    const timer=setTimeout(()=>child.kill('SIGKILL'),20000);
    const code=await new Promise((resolve,reject)=>{child.once('error',reject);child.once('exit',resolve)});
    clearTimeout(timer); assert.equal(code,0,errors);
    const result=JSON.parse(fs.readFileSync(output,'utf8'));
    for (const name of ['satori_context','satori_read','satori_action']) assert(result.active.includes(name),name);
    assert.equal(result.receipt.result.message_id,'7837409278651234567'); assert.equal(requests,1);
    assert(result.tools.find(t=>t.name==='satori_action').parameters.properties.request.anyOf.length>=6);
    console.log('Pi extension: 3 active tools, structured schema and IPC receipt verified; no model or QQ requests.');
  } finally {server.close();fs.rmSync(tmp,{recursive:true,force:true});}
})().catch(error=>{console.error(error);process.exitCode=1});
