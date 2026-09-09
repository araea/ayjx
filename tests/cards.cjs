// Generate real-registry fixtures first:
// HELP_CARD_DUMP=/tmp/cards/help CTL_CARD_DUMP=/tmp/cards/ctl cargo test renders_sample_cards_to_png -- --ignored --test-threads=1
// CARD_ARTIFACTS=/tmp/cards node tests/cards.cjs
// Uses only local HTML fixtures and an isolated Chromium profile.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const net = require('node:net');
const {spawn} = require('node:child_process');
const {pathToFileURL} = require('node:url');
const artifacts = process.env.CARD_ARTIFACTS;
assert(artifacts, 'Set CARD_ARTIFACTS to the fixture directory');
let chrome, ws, sequence = 0;
const pending = new Map();
const profile = fs.mkdtempSync(path.join(os.tmpdir(), 'ayjx-cards-'));
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
async function until(fn) {
  const end = Date.now() + 20000;
  while (Date.now() < end) { try { if (await fn()) return; } catch {} await sleep(100); }
  throw Error('Browser did not become ready');
}
function cdp(method, params = {}) {
  return new Promise((resolve, reject) => {
    const id = ++sequence;
    const timer = setTimeout(() => { pending.delete(id); reject(Error('CDP timeout: ' + method)); }, 20000);
    pending.set(id, {resolve, reject, timer});
    ws.send(JSON.stringify({id, method, params}));
  });
}
async function run(expression) {
  const result = await cdp('Runtime.evaluate', {expression, returnByValue:true, awaitPromise:true});
  assert(!result.exceptionDetails, JSON.stringify(result.exceptionDetails));
  return result.result.value;
}
async function main() {
  const server = net.createServer();
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  const port = server.address().port;
  await new Promise(resolve => server.close(resolve));
  const url = `http://127.0.0.1:${port}`;
  chrome = spawn(process.env.CHROME_BIN || 'chromium-browser', ['--headless', '--no-sandbox', '--disable-gpu',
    '--disable-dev-shm-usage', `--remote-debugging-port=${port}`, `--user-data-dir=${profile}`, 'about:blank'], {stdio:'ignore'});
  chrome.on('error', error => { console.error(error.message); });
  await until(async () => (await fetch(url + '/json/version')).ok);
  const page = await (await fetch(url + '/json/new?about:blank', {method:'PUT'})).json();
  ws = new WebSocket(page.webSocketDebuggerUrl);
  await new Promise((resolve, reject) => { ws.onopen = resolve; ws.onerror = reject; });
  ws.onmessage = event => {
    const message = JSON.parse(event.data), task = pending.get(message.id);
    if (!task) return;
    pending.delete(message.id); clearTimeout(task.timer);
    message.error ? task.reject(Error(message.error.message)) : task.resolve(message.result);
  };
  await cdp('Page.enable');
  await cdp('Emulation.setDeviceMetricsOverride', {width:640, height:800, deviceScaleFactor:1, mobile:false});
  const report = [];
  for (const family of ['help','ctl']) {
    const dir = path.join(artifacts, family);
    const files = fs.readdirSync(dir).filter(file => file.endsWith('.html'));
    assert(files.length >= (family === 'help' ? 4 : 5));
    for (const file of files) {
      await cdp('Page.navigate', {url:pathToFileURL(path.resolve(dir, file)).href});
      await until(() => run(`document.readyState === 'complete' && !!document.querySelector('footer')`));
      await run('document.fonts.ready.then(() => true)');
      const metrics = await run(`(() => {
        const shot = document.querySelector('.shot').getBoundingClientRect();
        const overflow = [...document.querySelectorAll('.card *')].filter(el => {
          const rect = el.getBoundingClientRect();
          return rect.width && (rect.left < shot.left || rect.right > shot.right + 1 ||
            (el.clientWidth && el.scrollWidth > el.clientWidth + 1));
        }).map(el => el.className || el.tagName);
        return {width:shot.width, height:shot.height, overflow,
          commands:document.querySelectorAll('.command').length,
          items:document.querySelectorAll('.item').length,
          states:document.querySelectorAll('.status-row').length};
      })()`);
      assert.equal(metrics.width, 640, file);
      assert.deepEqual(metrics.overflow, [], file + ': content overflow');
      assert(metrics.height <= 16000, file + ': excessive height');
      if (file === 'overview.html') assert(metrics.items >= 20);
      if (file === 'detail_widest.html') assert(metrics.commands >= 10);
      // Probe long unbroken values, aliases, escaping and CSS whitespace preservation in the browser.
      if (file === 'config.html') {
        await run(`document.querySelector('.code-line code').textContent = '  key = "' + 'LongValue中文'.repeat(100) + '"'; true`);
        assert.equal(await run(`document.querySelector('.code-line').scrollWidth <= document.querySelector('.code-line').clientWidth + 1`), true);
        assert.equal(await run(`getComputedStyle(document.querySelector('.code-line code')).whiteSpace`), 'pre-wrap');
      }
      report.push({file:family + '/' + file, ...metrics});
      console.log(`PASS ${family}/${file}: ${metrics.width} × ${metrics.height}, no overflow`);
    }
  }
  fs.writeFileSync(path.join(artifacts, 'layout-audit.json'), JSON.stringify(report, null, 2) + '\n');
}
main().catch(error => { console.error(error); process.exitCode = 1; }).finally(async () => {
  for (const task of pending.values()) clearTimeout(task.timer);
  ws?.close();
  if (chrome && chrome.exitCode === null) {
    const exited = new Promise(resolve => chrome.once('exit', resolve));
    chrome.kill('SIGTERM');
    await Promise.race([exited, sleep(3000)]);
    if (chrome.exitCode === null) chrome.kill('SIGKILL');
  }
  fs.rmSync(profile, {recursive:true, force:true});
});
