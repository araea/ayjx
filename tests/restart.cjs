// Exercise manual and real scheduler-triggered exec restarts in isolation.
// Run after cargo build --release: node tests/restart.cjs
const fs = require('node:fs');
const path = require('node:path');
const os = require('node:os');
const assert = require('node:assert/strict');
const { spawn, spawnSync } = require('node:child_process');
const root = path.resolve(__dirname, '..');
const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'ayjx-restart-'));
const executable = path.join(dir, 'target/release/ayjx');
fs.mkdirSync(path.dirname(executable), { recursive: true });
fs.copyFileSync(path.join(root, 'target/release/ayjx'), executable);
fs.copyFileSync(path.join(root, 'bot'), path.join(dir, 'bot'));
const plugins = [...fs.readFileSync(path.join(root, 'src/plugins/registry.rs'), 'utf8').matchAll(/^    ([a-z_]+) \{/gm)].map(m => m[1]);
const scheduled = new Date(Date.now() + 30000).toISOString().slice(11, 19);
fs.writeFileSync(path.join(dir, 'config.toml'),
  'command_prefix = ["/"]\n[[bots]]\nenabled = false\nprotocol = "console"\n' +
  plugins.map(name => `[${name}]\nenabled = ${['ctl', 'restart'].includes(name)}\n` +
    (name === 'restart' ? `time = "${scheduled}"\nallow_manual_restart = true\nrestart_delay_seconds = 0\n` : '')).join(''));
const child = spawn(path.join(dir, 'bot'), ['start'], {
  cwd: dir, stdio: ['pipe', 'pipe', 'pipe'],
  env: { ...process.env, TZ: 'UTC', AYJX_WAKE_LOCK: '0' },
});
let output = '';
child.stdout.on('data', data => { output += data; });
child.stderr.on('data', data => { output += data; });
const exited = new Promise((resolve, reject) => {
  child.on('error', reject);
  child.on('exit', (code, signal) => resolve({ code, signal }));
});
async function until(predicate, reason, timeout = 15000) {
  const end = Date.now() + timeout;
  while (Date.now() < end) {
    if (predicate()) return;
    if (child.exitCode !== null) throw new Error(`Unexpected exit: ${reason}\n${output}`);
    await new Promise(resolve => setTimeout(resolve, 80));
  }
  throw new Error(`Timeout: ${reason}\n${output}`);
}
const boots = () => (output.match(/前台控制台已就绪/g) || []).length;
async function main() {
  await until(() => boots() === 1, 'initial boot');
  child.stdin.write('/restart\n');
  await until(() => boots() === 2, 'manual restart');
  assert(output.includes('配置已保存，正在原地重启'));
  assert.equal(child.exitCode, null);
  assert(spawnSync(path.join(dir, 'bot'), ['status'], { encoding: 'utf8' }).stdout.includes(String(child.pid)));
  await until(() => boots() === 3, 'daily timer restart', 40000);
  assert(output.includes('每日定时重启触发'));
  const replies = (output.match(/\[Bot Reply\]/g) || []).length;
  child.stdin.write('/ctl list\n');
  await until(() => (output.match(/\[Bot Reply\]/g) || []).length > replies, 'stdin remains usable');
  const lock = spawnSync(path.join(dir, 'bot'), ['start'], { encoding: 'utf8' });
  assert.equal(lock.status, 1);
  child.kill('SIGTERM');
  const result = await exited;
  assert.deepEqual(result, { code: 0, signal: null });
  assert.equal(boots(), 3);
  console.log('Restart smoke passed: manual + scheduled restart, same PID, preserved console and singleton, graceful stop.');
}
main().catch(error => { console.error(error); process.exitCode = 1; }).finally(async () => {
  if (child.exitCode === null && child.signalCode === null) child.kill('SIGKILL');
  await exited.catch(() => {});
  fs.rmSync(dir, { recursive: true, force: true });
});
