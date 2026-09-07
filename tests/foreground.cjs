// Run after cargo build --release: node tests/foreground.cjs
// Uses an isolated configuration; no Satori connection or external messages.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawn } = require('node:child_process');
const root = path.resolve(__dirname, '..');
const temporary = fs.mkdtempSync(path.join(os.tmpdir(), 'ayjx-foreground-'));
const registry = fs.readFileSync(path.join(root, 'src/plugins/registry.rs'), 'utf8');
const plugins = [...registry.matchAll(/^    ([a-z_]+) \{/gm)].map(match => match[1]);
assert(plugins.includes('ctl') && plugins.includes('help'));
const configPath = path.join(temporary, 'config.toml');
fs.writeFileSync(configPath,
  'command_prefix = ["/"]\n[[bots]]\nenabled = false\nprotocol = "console"\n' +
  plugins.map(name => `[${name}]\nenabled = ${['ctl', 'help'].includes(name)}\n`).join(''));
const child = spawn(path.join(root, 'target/release/ayjx'), ['--console'], {
  cwd: temporary, stdio: ['pipe', 'pipe', 'pipe'],
});
let output = '';
child.stdout.on('data', data => { output += data; });
child.stderr.on('data', data => { output += data; });
const exited = new Promise((resolve, reject) => {
  child.once('error', reject);
  child.once('exit', (code, signal) => resolve({ code, signal }));
});
async function until(predicate, description, milliseconds = 12000) {
  const end = Date.now() + milliseconds;
  while (Date.now() < end) {
    if (predicate()) return;
    if (child.exitCode !== null) throw new Error(`Premature exit while waiting for ${description}\n${output}`);
    await new Promise(resolve => setTimeout(resolve, 40));
  }
  throw new Error(`Timeout: ${description}\n${output}`);
}
async function main() {
  await until(() => output.includes('前台控制台已就绪'), 'console ready');
  child.stdin.write('/ctl status\n');
  await until(() => output.includes('插件状态（全局配置）'), 'status reply');
  assert(output.includes('开 ctl') && output.includes('关 ping'));
  child.stdin.write('/ctl set help image_scale 2\n/ctl set help image_enabled 关\n');
  await until(() => output.includes('已保存 help.image_scale') && output.includes('已保存 help.image_enabled'), 'two immediate writes');
  child.stdin.write('/ctl on ping\n');
  await until(() => output.includes('已全部开启并保存'), 'enable lifecycle plugin');
  child.stdin.write('/ctl list\n');
  await until(() => output.includes('ping（心跳测试） · 待重启'), 'pending initialization status');
  child.stdin.write('/help ctl\n');
  await until(() => output.includes('统一管理全部插件'), 'text help');
  child.stdin.write('/ctl set help image_enabled 开\n');
  await until(() => (output.match(/已保存 help.image_enabled/g) || []).length === 2, 'enable help images');
  const previousReplies = (output.match(/\[Bot Reply\]/g) || []).length;
  child.stdin.write('/help ctl\n');
  await until(() => (output.match(/\[Bot Reply\]/g) || []).length > previousReplies, 'image help or text fallback', 25000);
  const stopping = Date.now();
  child.kill('SIGTERM'); // Leave stdin open to catch blocking-stdin shutdown regressions.
  const result = await Promise.race([
    exited,
    new Promise((_, reject) => {
      const timer = setTimeout(() => reject(new Error('Shutdown hung with stdin open')), 8000);
      timer.unref();
    }),
  ]);
  assert.deepEqual(result, { code: 0, signal: null });
  assert(output.includes('配置已保存') && output.includes('Bye!'));
  const saved = fs.readFileSync(configPath, 'utf8');
  assert(/\[\[bots\]\]\s+enabled = false\s+protocol = "console"/.test(saved), '--console must not persist');
  assert(/\[help\][\s\S]*?image_scale = 2\.0/.test(saved));
  assert(/\[ping\]\s+enabled = true/.test(saved));
  console.log(`Foreground smoke passed: commands, persistence, pending lifecycle, help rendering/fallback, clean shutdown (${Date.now() - stopping} ms).`);
}
main().catch(error => {
  console.error(error);
  process.exitCode = 1;
}).finally(async () => {
  if (child.exitCode === null && child.signalCode === null) child.kill('SIGKILL');
  await exited.catch(() => {});
  fs.rmSync(temporary, { recursive: true, force: true });
});
