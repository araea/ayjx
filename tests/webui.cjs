// cargo build --release && node tests/webui.cjs
// Real HTTP + Chromium, isolated config/database, no Satori or external messages.
// CHROME_BIN selects Chromium; CDP_URL can reuse a running test browser.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const net = require('node:net');
const { spawn } = require('node:child_process');
const root = path.resolve(__dirname, '..');
const temp = fs.mkdtempSync(path.join(os.tmpdir(), 'ayjx-webui-'));
const artifacts = process.env.WEBUI_ARTIFACTS || temp;
fs.mkdirSync(artifacts, { recursive: true });
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
let bot, chrome, ws, pageId, browserURL, sequence = 0;
const pending = new Map(), errors = [];
async function until(fn, label, timeout = 20000) {
  const end = Date.now() + timeout;
  while (Date.now() < end) { if (await fn()) return; await sleep(60); }
  throw new Error('Timeout: ' + label);
}
async function freePort() {
  const server = net.createServer();
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  const port = server.address().port;
  await new Promise(resolve => server.close(resolve));
  return port;
}
function cdp(method, params = {}) {
  return new Promise((resolve, reject) => {
    const id = ++sequence;
    const timer = setTimeout(() => { pending.delete(id); reject(new Error('CDP timeout: ' + method)); }, 20000);
    pending.set(id, { resolve, reject, timer });
    ws.send(JSON.stringify({ id, method, params }));
  });
}
async function run(expression) {
  const result = await cdp('Runtime.evaluate', { expression, returnByValue: true, awaitPromise: true });
  if (result.exceptionDetails) throw new Error(result.exceptionDetails.exception?.description || result.exceptionDetails.text);
  return result.result.value;
}
const j = JSON.stringify;
const inputSelector = path => `.field[data-path="${path}"] input, .field[data-path="${path}"] textarea, .field[data-path="${path}"] select`;
async function edit(path, value) {
  await run(`(() => { const input = document.querySelector(${j(inputSelector(path))}); if (!input) throw Error('missing input'); input.focus(); input.value = ${j(value)}; input.dispatchEvent(new Event('input', {bubbles:true})); })()`);
}
async function select(name) {
  await run(`document.activeElement.blur(); select(${j(name)})`);
}
async function saved() { await until(() => run('busy === 0 && drafts.size === 0'), 'saved drafts'); }
async function main() {
  const port = await freePort(), token = 'isolated-webui-test-key';
  const origin = `http://127.0.0.1:${port}`;
  const registry = fs.readFileSync(path.join(root, 'src/plugins/registry.rs'), 'utf8');
  const plugins = [...registry.matchAll(/^    ([a-z_]+) \{/gm)].map(match => match[1]);
  fs.writeFileSync(path.join(temp, 'config.toml'),
    'command_prefix = ["/"]\n[[bots]]\nenabled = false\nprotocol = "console"\n' +
    plugins.map(name => `[${name}]\nenabled = ${['ctl', 'webui'].includes(name)}\n` +
      (name === 'webui' ? `port = ${port}\ntoken = "${token}"\n` : '')).join(''));
  bot = spawn(path.join(root, 'target/release/ayjx'), ['--console'], { cwd: temp, stdio: ['pipe', 'pipe', 'pipe'] });
  let output = '';
  bot.stdout.on('data', data => { output += data; });
  bot.stderr.on('data', data => { output += data; });
  const api = async (route, body, auth = token) => {
    const response = await fetch(origin + route, { headers: {Authorization: 'Bearer ' + auth, 'Content-Type': 'application/json'},
      ...(body ? {method:'POST', body:JSON.stringify(body)} : {}) });
    return {status:response.status, data:await response.json()};
  };
  await until(async () => { try { return (await api('/api/state')).status === 200; } catch (_) { return false; } }, 'isolated service');
  assert.equal((await api('/api/state', null, 'wrong')).status, 401);
  assert.equal((await api('/api/command', {action:'set', name:'help', path:'image_scale', value:9})).data.ok, false);
  assert.equal((await api('/api/command', {action:'set', name:'help', path:'image_scale', value:2})).data.ok, true);
  assert(fs.readFileSync(path.join(temp, 'config.toml'), 'utf8').includes('image_scale = 2.0'));
  console.log('PASS HTTP authentication, validation and disk persistence');

  browserURL = process.env.CDP_URL;
  if (!browserURL) {
    const debugPort = await freePort();
    browserURL = `http://127.0.0.1:${debugPort}`;
    chrome = spawn(process.env.CHROME_BIN || 'chromium-browser', ['--headless', '--no-sandbox', '--disable-gpu', '--disable-dev-shm-usage',
      `--remote-debugging-port=${debugPort}`, `--user-data-dir=${path.join(temp, 'chrome')}`, 'about:blank'], {stdio:'ignore'});
    chrome.on('error', error => errors.push(error.message));
  }
  await until(async () => { try { return (await fetch(browserURL + '/json/version')).ok; } catch (_) { return false; } }, 'Chromium');
  const page = await (await fetch(browserURL + '/json/new?about:blank', {method:'PUT'})).json();
  pageId = page.id; ws = new WebSocket(page.webSocketDebuggerUrl);
  await new Promise((resolve, reject) => { ws.onopen = resolve; ws.onerror = reject; });
  ws.onmessage = event => {
    const message = JSON.parse(event.data);
    if (message.id) {
      const task = pending.get(message.id); if (!task) return;
      clearTimeout(task.timer); pending.delete(message.id);
      message.error ? task.reject(new Error(message.error.message)) : task.resolve(message.result);
    } else if (message.method === 'Runtime.exceptionThrown') errors.push(message.params.exceptionDetails.exception?.description || message.params.exceptionDetails.text);
  };
  await cdp('Runtime.enable'); await cdp('Page.enable');
  await cdp('Emulation.setDeviceMetricsOverride', {width:1440, height:1000, deviceScaleFactor:1, mobile:false});
  await cdp('Page.navigate', {url:origin + '/?k=' + token});
  await until(() => run('!!document.querySelector("#app") && !document.querySelector("#app").hidden'), 'authenticated UI');
  assert.equal(await run('location.search'), '');
  assert.equal(await run('document.querySelectorAll("button button").length'), 0);
  await select('help');
  await edit('image_scale', '2.5'); await saved();
  assert.equal(await run('currentField("help", "image_scale").value'), 2.5);
  assert.equal(await run('document.activeElement.value'), '2.5');
  console.log('PASS automatic save without losing focused input');

  // Hold a save response while typing into a second field: preserve node and new draft.
  await run(`window.originalFetch = window.fetch; window.activeWrites = 0; window.maxWrites = 0;
    window.fetch = async (...args) => {
      if (args[0] === '/api/command') {
        window.activeWrites++; window.maxWrites = Math.max(window.maxWrites, window.activeWrites);
        try { if (window.failNext) { window.failNext = false; throw Error('模拟断网'); }
          if (window.delayNext) { window.delayNext = false; await new Promise(r => setTimeout(r, 1200)); }
          return await window.originalFetch(...args);
        } finally { window.activeWrites--; }
      }
      return window.originalFetch(...args);
    };`);
  await select('webshot');
  await run('window.delayNext = true');
  await edit('device_scale_factor', '2');
  // Use an existing numeric field from schema rather than assuming plugin-specific names.
  const other = await run('flatten(plugin("webshot").fields).find(f => f.kind === "int").path');
  await run('saveAll()');
  await edit(other, '123');
  await run('window.heldInput = document.activeElement; saveAll()');
  await saved();
  assert.equal(await run('document.activeElement === window.heldInput'), true);
  assert.equal(await run('window.maxWrites'), 1);
  console.log('PASS serialized writes, continuous editing and stable focus');

  await select('help'); await run('window.failNext = true');
  await edit('image_scale', '3');
  await until(() => run('!!drafts.get("help:image_scale")?.error'), 'failed draft retained');
  assert.equal(await run('document.activeElement.value'), '3');
  await run('saveAll()'); await saved();
  assert.equal(await run('currentField("help", "image_scale").value'), 3);
  await edit('image_scale', '9');
  await until(() => run('!!drafts.get("help:image_scale")?.error'), 'validation error');
  assert.equal(await run('document.querySelector(".field.bad input").value'), '9');
  await edit('image_scale', '2'); await saved();
  console.log('PASS network retry and validation errors retain editable drafts');
  await run('window.delayNext = true');
  await edit('image_scale', '2.5'); await run('saveAll()');
  await edit('image_scale', '3.5'); await run('saveAll()'); await saved();
  assert.equal(await run('currentField("help", "image_scale").value'), 3.5);
  await run('document.activeElement.blur(); window.failNext = true'); await sleep(100);
  await run('document.querySelector(".field[data-path=\\"image_enabled\\"] .sw").click()');
  await until(() => run('!!drafts.get("help:image_enabled")?.error'), 'failed switch retained');
  assert.equal(await run('document.querySelector(".field[data-path=\\"image_enabled\\"] .sw").getAttribute("aria-checked")'), 'true');
  await run('document.dispatchEvent(new KeyboardEvent("keydown", {key:"s", ctrlKey:true, bubbles:true}))');
  await saved();
  assert.equal(await run('currentField("help", "image_enabled").value'), false);
  console.log('PASS latest edit wins within an in-flight field; failed switches retry with Ctrl+S');

  await select('repeater');
  await edit('channel.white', '123.9');
  await run(`document.activeElement.dispatchEvent(new KeyboardEvent('keydown', {key:'Enter', bubbles:true}))`);
  assert.equal(await run('drafts.get("repeater:channel.white").invalid'), true);
  assert.deepEqual(await run('currentField("repeater", "channel.white").value'), []);
  await edit('channel.white', '123, 456');
  await run(`document.activeElement.dispatchEvent(new KeyboardEvent('keydown', {key:'Enter', bubbles:true}))`);
  await saved();
  assert.deepEqual(await run('currentField("repeater", "channel.white").value'), [123,456]);
  await edit('interrupt_texts', '一句话 有空格，还有逗号');
  await run(`document.activeElement.dispatchEvent(new KeyboardEvent('keydown', {key:'Enter', bubbles:true}))`);
  await saved();
  assert.equal(await run('currentField("repeater", "interrupt_texts").value.at(-1)'), '一句话 有空格，还有逗号');
  console.log('PASS integer arrays reject fractions; string array entries preserve prose');

  await run('document.activeElement.blur()');
  await sleep(100);
  await run(`const filterInput = document.querySelector('.tools input'); filterInput.value='channel.white'; filterInput.dispatchEvent(new Event('input'))`);
  assert.equal(await run('!!document.querySelector(".field[data-path=\\"channel.white\\"]")'), true);
  assert.equal(await run('!!document.querySelector(".field[data-path=\\"channel.black\\"]")'), false);
  await run(`document.querySelector('.tools input').value='群名单'; document.querySelector('.tools input').dispatchEvent(new Event('input'))`);
  assert.equal(await run('document.querySelectorAll(".field[data-path^=\\"channel.\\"]").length'), 2);
  console.log('PASS nested field filtering and translated group names');

  await select('ai_news');
  const rawText = '["123"]\ncategory = "model"\n';
  await edit('group_preferences', rawText);
  await run('document.querySelectorAll(".tab")[1].click(); document.querySelectorAll(".tab")[0].click()');
  assert.equal(await run(`document.querySelector(${j(inputSelector('group_preferences'))}).value`), rawText);
  await select('help'); await select('ai_news'); await run('refresh(true)');
  assert.equal(await run(`document.querySelector(${j(inputSelector('group_preferences'))}).value`), rawText);
  await run('saveAll()'); await saved();
  assert.equal(await run('currentField("ai_news", "group_preferences").value["123"].category'), 'model');
  console.log('PASS TOML draft survives tabs/navigation/refresh and saves through real validator');

  await select('webui');
  await edit('public_url', '测试');
  await run(`document.activeElement.dispatchEvent(new CompositionEvent('compositionstart', {bubbles:true})); document.activeElement.value='测试输入'; document.activeElement.dispatchEvent(new InputEvent('input', {bubbles:true,isComposing:true}));`);
  await sleep(1000);
  assert.equal(await run('currentField("webui", "public_url").value'), '');
  await run(`document.activeElement.dispatchEvent(new CompositionEvent('compositionend', {bubbles:true}))`);
  await saved();
  assert.equal(await run('currentField("webui", "public_url").value'), '测试输入');
  await edit('public_url', ''); await saved();
  console.log('PASS Chinese composition never submits an unfinished phrase');

  await edit('token', 'rotated-webui-test-key');
  await sleep(1000);
  assert.equal(await run('TOKEN'), token);
  await run('saveAll()'); await saved();
  assert.equal(await run('TOKEN'), 'rotated-webui-test-key');
  assert.equal(await run(`document.querySelector(${j(inputSelector('token'))}).value`), '');
  assert.equal((await api('/api/state', null, 'rotated-webui-test-key')).status, 200);
  assert.equal((await api('/api/state', null, token)).status, 401);
  await run(`document.activeElement.blur(); action({action:'reset', name:'webui', path:''})`);
  assert.equal((await api('/api/state', null, 'rotated-webui-test-key')).status, 200);
  console.log('PASS explicit secret save, credential rotation and reset preserve access');

  await select('repeater');
  await run('document.documentElement.dataset.theme="light"');
  const desktop = await cdp('Page.captureScreenshot', {format:'png'});
  fs.writeFileSync(path.join(artifacts, 'webui-desktop.png'), Buffer.from(desktop.data, 'base64'));
  await cdp('Emulation.setDeviceMetricsOverride', {width:390,height:844,deviceScaleFactor:1,mobile:true});
  await run('document.documentElement.dataset.theme="dark"');
  for (const name of plugins) {
    await select(name);
    assert.equal(await run('document.querySelector("#main").scrollWidth <= document.querySelector("#main").clientWidth'), true, 'mobile overflow: ' + name);
    assert.equal(await run('document.querySelectorAll("button button").length'), 0);
  }
  await select('repeater');
  const mobile = await cdp('Page.captureScreenshot', {format:'png'});
  fs.writeFileSync(path.join(artifacts, 'webui-mobile.png'), Buffer.from(mobile.data, 'base64'));
  assert.equal(await run('getComputedStyle(document.querySelector(".side")).display'), 'none');
  await run('document.querySelector(".back").click()');
  assert.notEqual(await run('getComputedStyle(document.querySelector(".side")).display'), 'none');
  console.log('PASS desktop/mobile themes, all plugin layouts, navigation and semantic controls');
  await cdp('Emulation.setDeviceMetricsOverride', {width:320,height:720,deviceScaleFactor:1,mobile:true});
  await select('oai');
  assert.equal(await run('document.querySelector("#main").scrollWidth <= document.querySelector("#main").clientWidth'), true);
  await select('ai_news'); await edit('group_preferences', '["456"]\ncategory = "model"\n');
  const rotated = await api('/api/command', {action:'set',name:'webui',path:'token',value:'external-key'}, 'rotated-webui-test-key');
  assert.equal(rotated.data.ok, true);
  await run('refresh(true)');
  assert.equal(await run('document.querySelector("#gate").hidden'), false);
  assert.equal(await run('drafts.has("ai_news:group_preferences")'), true);
  await run('document.querySelector("#key").value="external-key"; document.querySelector("#enter").click()');
  await until(() => run('!document.querySelector("#app").hidden'), 'reauthentication');
  await run('document.dispatchEvent(new KeyboardEvent("keydown", {key:"s", ctrlKey:true, bubbles:true}))'); await saved();
  console.log('PASS expired credentials reauthenticate without losing drafts; 320px layout');
  assert.deepEqual(errors, [], 'browser exceptions');
  bot.kill('SIGTERM');
  await until(() => bot.exitCode !== null, 'clean shutdown');
  assert.equal(bot.exitCode, 0);
  assert(output.includes('配置已保存'));
  console.log('All Web UI browser and integration checks passed.');
}
main().catch(error => { console.error(error); process.exitCode = 1; }).finally(async () => {
  if (ws) ws.close();
  for (const task of pending.values()) clearTimeout(task.timer);
  if (pageId) await fetch(browserURL + '/json/close/' + pageId).catch(() => {});
  for (const child of [bot, chrome]) {
    if (child && child.exitCode === null) {
      child.kill('SIGTERM');
      await Promise.race([new Promise(resolve => child.once('exit', resolve)), sleep(3000)]);
      if (child.exitCode === null && child.signalCode === null) child.kill('SIGKILL');
    }
  }
  fs.rmSync(temp, {recursive:true, force:true});
});
