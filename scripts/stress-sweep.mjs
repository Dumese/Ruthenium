/**
 * 压力/泄漏测试（无人值守）：
 *   A) 连续开关 20 个本地终端标签 → 关闭后 DOM 应回到基线，无异常
 *   B) 连续切换语言 12 次 → 文案正确、无异常、DOM 不增长
 *   C) 反复打开/关闭 8 次设置与控制台 → 覆盖层节点不残留
 *
 * 用法：node scripts/stress-sweep.mjs （preview 4173 + Edge 9333）
 */
const CDP = process.env.SWEEP_CDP || 'http://127.0.0.1:9333';
const URL_APP = process.env.SWEEP_URL || 'http://localhost:4173/';
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function connect() {
  for (let i = 0; i < 100; i++) {
    try { const r = await fetch(CDP + '/json/version'); if (r.ok) return (await r.json()).webSocketDebuggerUrl; }
    catch { /* retry */ }
    await sleep(200);
  }
  throw new Error('CDP not ready');
}
function client(u) {
  const ws = new WebSocket(u); let id = 0; const pending = new Map(); const errors = [];
  ws.addEventListener('message', (ev) => {
    const m = JSON.parse(ev.data);
    if (m.id && pending.has(m.id)) { const { resolve, reject } = pending.get(m.id); pending.delete(m.id); m.error ? reject(new Error(JSON.stringify(m.error))) : resolve(m.result); }
    else if (m.method === 'Runtime.exceptionThrown') errors.push('EXC ' + String(m.params.exceptionDetails.exception?.description || m.params.exceptionDetails.text).slice(0, 140));
    else if (m.method === 'Runtime.consoleAPICalled' && m.params.type === 'error') errors.push('ERR ' + m.params.args.map((a) => a.value ?? a.description ?? '').join(' ').slice(0, 140));
  });
  const ready = new Promise((res, rej) => { ws.addEventListener('open', res); ws.addEventListener('error', rej); });
  const send = (method, params = {}, sessionId) => new Promise((resolve, reject) => { const mid = ++id; pending.set(mid, { resolve, reject }); ws.send(JSON.stringify({ id: mid, method, params, sessionId })); });
  return { ws, ready, send, errors };
}

const main = async () => {
  const c = client(await connect()); await c.ready;
  const { targetInfos } = await c.send('Target.getTargets');
  const page = targetInfos.find((t) => t.type === 'page');
  const { sessionId } = await c.send('Target.attachToTarget', { targetId: page.targetId, flatten: true });
  const S = (m, p = {}) => c.send(m, p, sessionId);
  await S('Page.enable'); await S('Runtime.enable');
  const ev = async (e) => (await S('Runtime.evaluate', { expression: e, returnByValue: true, awaitPromise: true })).result.value;
  const fails = [];

  await S('Emulation.setDeviceMetricsOverride', { width: 1280, height: 800, deviceScaleFactor: 1.5, mobile: false });
  await S('Page.navigate', { url: URL_APP + '?t=' + Date.now() });
  await sleep(2800);

  const snapshot = () => ev(`({
    tabs: document.querySelectorAll('.tabbar .tab').length,
    xterms: document.querySelectorAll('.termhost .xterm').length,
    nodes: document.getElementsByTagName('*').length,
    overlays: document.querySelectorAll('.sheet, .modal, .palette, .menu').length,
  })`);

  const base = await snapshot();
  console.log('基线:', JSON.stringify(base));

  // A) 打开 20 个本地终端
  for (let i = 0; i < 20; i++) {
    await ev(`document.dispatchEvent(new KeyboardEvent('keydown',{key:'t',ctrlKey:true,bubbles:true,cancelable:true})); true`);
    await sleep(120);
  }
  await sleep(1200);
  const opened = await snapshot();
  console.log('开 20 个后:', JSON.stringify(opened), '（期望 tabs=20+基线, xterms>0）');
  if (opened.tabs < 20) fails.push(`只开出 ${opened.tabs} 个标签`);
  if (opened.xterms === 0) fails.push('没有 xterm 实例');

  // 关闭全部（Ctrl+Shift+W 逐个）
  let guard = 0;
  while ((await ev(`document.querySelectorAll('.tabbar .tab').length`)) > 0 && guard++ < 40) {
    await ev(`document.dispatchEvent(new KeyboardEvent('keydown',{key:'W',ctrlKey:true,shiftKey:true,bubbles:true,cancelable:true})); true`);
    await sleep(150);
  }
  await sleep(1200);
  const closed = await snapshot();
  console.log('全部关闭后:', JSON.stringify(closed), '（期望 tabs=0, xterms=0）');
  if (closed.tabs !== 0) fails.push(`关闭后仍剩 ${closed.tabs} 个标签`);
  if (closed.xterms !== 0) fails.push(`关闭后仍残留 ${closed.xterms} 个 xterm 实例（泄漏）`);

  // B) 语言来回切 12 次
  const langs = ['简体中文', 'English', '日本語', '繁體中文'];
  for (let i = 0; i < 12; i++) {
    const lang = langs[i % 4];
    await ev(`document.dispatchEvent(new KeyboardEvent('keydown',{key:',',ctrlKey:true,bubbles:true,cancelable:true})); true`);
    await sleep(380);
    const ok = await ev(`(() => { const el=[...document.querySelectorAll('.sheet .pill')].find(x=>(x.textContent||'').trim().includes(${JSON.stringify(lang)})); if(!el) return false; el.click(); return true; })()`);
    if (!ok) { fails.push(`第 ${i + 1} 次切换未找到 ${lang}`); }
    await sleep(320);
    await ev(`document.dispatchEvent(new KeyboardEvent('keydown',{key:'Escape',bubbles:true,cancelable:true})); true`);
    await sleep(180);
  }
  const afterLang = await snapshot();
  console.log('切语言 12 次后:', JSON.stringify(afterLang));
  if (afterLang.overlays !== 0) fails.push(`切语言后残留 ${afterLang.overlays} 个覆盖层`);

  // C) 反复开关设置 / 命令面板
  for (let i = 0; i < 8; i++) {
    await ev(`document.dispatchEvent(new KeyboardEvent('keydown',{key:',',ctrlKey:true,bubbles:true,cancelable:true})); true`);
    await sleep(260);
    await ev(`document.dispatchEvent(new KeyboardEvent('keydown',{key:'Escape',bubbles:true,cancelable:true})); true`);
    await sleep(180);
    await ev(`document.dispatchEvent(new KeyboardEvent('keydown',{key:'k',ctrlKey:true,bubbles:true,cancelable:true})); true`);
    await sleep(260);
    await ev(`document.dispatchEvent(new KeyboardEvent('keydown',{key:'Escape',bubbles:true,cancelable:true})); true`);
    await sleep(180);
  }
  const afterOverlays = await snapshot();
  console.log('开关覆盖层 16 次后:', JSON.stringify(afterOverlays));
  if (afterOverlays.overlays !== 0) fails.push(`覆盖层残留 ${afterOverlays.overlays} 个`);

  // DOM 节点增长上限（20 个标签开关过，允许一定增长，但不应翻倍）
  const growth = (afterOverlays.nodes - base.nodes) / base.nodes;
  console.log(`DOM 节点增长: ${base.nodes} → ${afterOverlays.nodes} (${(growth * 100).toFixed(1)}%)`);
  if (growth > 0.35) fails.push(`DOM 节点增长 ${(growth * 100).toFixed(0)}% 疑似泄漏`);

  if (c.errors.length) fails.push('运行期异常: ' + c.errors.slice(0, 3).join(' / '));

  console.log(`\n压力测试结论：${fails.length === 0 ? 'PASS' : 'FAIL'}`);
  for (const f of fails) console.log('  ✗ ' + f);
  c.ws.close();
  process.exit(fails.length ? 1 : 0);
};

main().catch((e) => { console.error('STRESS_FAILED', e?.stack || e); process.exit(2); });
