// Run after cargo build --release: node tests/foreground.cjs
// Uses an isolated configuration; no Satori connection or external messages.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawn, spawnSync } = require('node:child_process');
const root = path.resolve(__dirname, '..');
const temporary = fs.mkdtempSync(path.join(os.tmpdir(), 'ayjx-foreground-'));
const executable = path.join(temporary, 'target/release/ayjx');
fs.mkdirSync(path.dirname(executable), { recursive: true });
fs.copyFileSync(path.join(root, 'target/release/ayjx'), executable);
const launcher = path.join(temporary, 'bot');
fs.copyFileSync(path.join(root, 'bot'), launcher);
const registry = fs.readFileSync(path.join(root, 'src/plugins/registry.rs'), 'utf8');
const plugins = [...registry.matchAll(/^    ([a-z_]+) \{/gm)].map(match => match[1]);
assert(plugins.includes('ctl') && plugins.includes('help'));
const configPath = path.join(temporary, 'config.toml');
fs.writeFileSync(configPath,
  'command_prefix = ["/"]\n[[bots]]\nenabled = false\nprotocol = "console"\n' +
  plugins.map(name => `[${name}]\nenabled = ${['ctl', 'help'].includes(name)}\n` +
    (name === 'ctl' ? 'image_enabled = false\n' : '')).join(''));
const child = spawn(launcher, ['start'], {
  cwd: temporary, stdio: ['pipe', 'pipe', 'pipe'],
  env: { ...process.env, AYJX_WAKE_LOCK: '0' },
});
let output = '';
child.stdout.on('data', data => { output += data; });
child.stderr.on('data', data => { output += data; });
const exited = new Promise((resolve, reject) => {
  child.once('error', reject);
  child.once('exit', (code, signal) => resolve({ code, signal }));
});
// 出错时只显示诊断文字，避免把整张图片的 base64 倾倒进日志。
const readableOutput = () => output.replace(/base64[^"<>\s]+/g, '[image data omitted]');
async function until(predicate, description, milliseconds = 12000) {
  const end = Date.now() + milliseconds;
  while (Date.now() < end) {
    if (predicate()) return;
    if (child.exitCode !== null) throw new Error(`Premature exit while waiting for ${description}\n${readableOutput()}`);
    await new Promise(resolve => setTimeout(resolve, 40));
  }
  throw new Error(`Timeout: ${description}\n${readableOutput()}`);
}
async function main() {
  await until(() => output.includes('前台控制台已就绪'), 'console ready');
  const status = spawnSync(launcher, ['status'], { encoding: 'utf8' });
  assert.equal(status.status, 0);
  assert(status.stdout.includes(String(child.pid)));
  const duplicate = spawnSync(launcher, ['start'], { encoding: 'utf8' });
  assert.equal(duplicate.status, 1);
  assert(duplicate.stderr.includes('已运行'));
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
  await until(() => (output.match(/\[Bot Reply\]/g) || []).length > previousReplies, 'image help or text fallback', 55000);
  // 列表的文本测试显式关闭图片；这里再验证真实 ctl 网页出图。
  child.stdin.write('/ctl set ctl image_enabled 开\n');
  await until(() => output.includes('已保存 ctl.image_enabled'), 'enable control images');
  const imageStart = output.length;
  child.stdin.write('/ctl list\n');
  await until(() => /base64(?:,|:\/\/)iVBOR/.test(output.slice(imageStart)), 'browser control PNG', 55000);
  assert(!output.slice(imageStart).includes('插件状态（全局配置）'), 'control should render a PNG');
  const stopping = Date.now();
  // Leave stdin open to catch blocking-stdin shutdown regressions.
  const stop = spawnSync(launcher, ['stop'], { encoding: 'utf8', timeout: 8000 });
  assert.equal(stop.status, 0, stop.stderr);
  const result = await Promise.race([
    exited,
    new Promise((_, reject) => {
      const timer = setTimeout(() => reject(new Error('Shutdown hung with stdin open')), 8000);
      timer.unref();
    }),
  ]);
  assert.deepEqual(result, { code: 0, signal: null });
  assert.equal(spawnSync(launcher, ['status']).status, 3);
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
