/**
 * 性能诊断探针：定位「启动耗时」与「标签切换卡顿」的具体来源。
 *  1) 启动：导航 → 侧栏主机可见，做 CPU Profile，输出自身耗时 Top 函数与按 URL 汇总
 *  2) 标签切换：逐个切换，记录耗时 + __termPerf 的 fit/resize/mount 增量
 *  3) 挂载路径：切换过程中 Profile，定位耗时函数
 *
 * 前置：npm run build:e2e && npx vite preview --port 4173 --strictPort
 *       msedge --headless=new --remote-debugging-port=9333 about:blank
 */
const CDP = process.env.PERF_CDP || 'http://127.0.0.1:9333';
const URL_APP = process.env.PERF_URL || 'http://localhost:4173/';
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function connect() {
  for (let i = 0; i < 100; i++) {
    try { const r = await fetch(CDP + '/json/version'); if (r.ok) return (await r.json()).webSocketDebuggerUrl; } catch { /* retry */ }
    await sleep(200);
  }
  throw new Error('CDP not reachable');
}
function client(wsUrl) {
  const ws = new WebSocket(wsUrl);
  let id = 0; const pending = new Map();
  ws.addEventListener('message', (ev) => {
    const m = JSON.parse(ev.data);
    if (m.id && pending.has(m.id)) {
      const { resolve, reject } = pending.get(m.id); pending.delete(m.id);
      m.error ? reject(new Error(JSON.stringify(m.error))) : resolve(m.result);
    }
  });
  const ready = new Promise((res, rej) => { ws.addEventListener('open', res); ws.addEventListener('error', rej); });
  const send = (method, params = {}, sessionId) => new Promise((resolve, reject) => {
    const mid = ++id; pending.set(mid, { resolve, reject });
    ws.send(JSON.stringify({ id: mid, method, params, sessionId }));
  });
  return { ws, ready, send };
}

function summarize(profile, topN = 12) {
  const byId = new Map(profile.nodes.map((n) => [n.id, n]));
  const self = new Map(); const byUrl = new Map();
  const dt = profile.timeDeltas || [];
  for (let i = 0; i < (profile.samples || []).length; i++) {
    const d = dt[i] || 0; if (d <= 0) continue;
    const n = byId.get(profile.samples[i]); if (!n) continue;
    const cf = n.callFrame || {};
    const url = String(cf.url || '').split('/').pop() || 'native';
    const key = `${cf.functionName || '(anon)'} @ ${url}:${cf.lineNumber ?? 0}`;
    self.set(key, (self.get(key) || 0) + d);
    byUrl.set(url, (byUrl.get(url) || 0) + d);
  }
  return {
    top: [...self.entries()].sort((a, b) => b[1] - a[1]).slice(0, topN).map(([fn, ms]) => ({ fn, ms: +(ms / 1000).toFixed(1) })),
    byUrl: [...byUrl.entries()].sort((a, b) => b[1] - a[1]).slice(0, 8).map(([u, ms]) => ({ url: u, ms: +(ms / 1000).toFixed(1) })),
  };
}

async function main() {
  const c = client(await connect()); await c.ready;
  const { targetInfos } = await c.send('Target.getTargets');
  const page = targetInfos.find((t) => t.type === 'page');
  const { sessionId } = await c.send('Target.attachToTarget', { targetId: page.targetId, flatten: true });
  const S = (m, p = {}) => c.send(m, p, sessionId);
  await S('Page.enable'); await S('Runtime.enable'); await S('Profiler.enable');
  await S('Page.addScriptToEvaluateOnNewDocument', { source: 'window.__RUTHENIUM_E2E__ = true;' });
  await S('Emulation.setDeviceMetricsOverride', { width: 1440, height: 900, deviceScaleFactor: 1, mobile: false });
  const ev = async (expr) => {
    const r = await S('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true });
    if (r.exceptionDetails) throw new Error(JSON.stringify(r.exceptionDetails).slice(0, 300));
    return r.result.value;
  };

  const out = {};

  // ---------- 1. 启动 Profile ----------
  await S('Profiler.setSamplingInterval', { interval: 200 });
  await S('Profiler.start');
  await S('Page.navigate', { url: URL_APP + '?diag=' + Date.now() });
  const startupInfo = await ev(`(async () => {
    const t = performance.now();
    while (performance.now() - t < 15000) { if (document.querySelector('.hostitem')) break; await new Promise((r) => setTimeout(r, 16)); }
    return { hostsVisibleMs: +performance.now().toFixed(1), domNodes: document.getElementsByTagName('*').length };
  })()`);
  await sleep(200);
  const p1 = await S('Profiler.stop');
  out.startup = { ...startupInfo, ...summarize(p1.profile) };

  // ---------- 2. 造 6 个标签 ----------
  await sleep(400);
  for (let i = 0; i < 5; i++) {
    await ev(`document.dispatchEvent(new KeyboardEvent('keydown',{key:'t',ctrlKey:true,bubbles:true,cancelable:true})); true`);
    await sleep(320);
  }
  await sleep(1500);
  out.tabCount = await ev(`document.querySelectorAll('.tabbar .tab').length`);

  // ---------- 3. 标签切换：逐次记录耗时与计数器增量 ----------
  const switches = [];
  await S('Profiler.start');
  for (let i = 0; i < 14; i++) {
    const idx = i % (await ev(`document.querySelectorAll('.tabbar .tab').length`));
    const before = await ev(`window.__termPerf ? window.__termPerf() : null`);
    const ms = await ev(`(() => new Promise((res) => {
      const t0 = performance.now();
      const ts = [...document.querySelectorAll('.tabbar .tab')];
      const t = ts[${idx}];
      if (t) t.click();
      requestAnimationFrame(() => requestAnimationFrame(() => res(performance.now() - t0)));
    }))()`);
    const after = await ev(`window.__termPerf ? window.__termPerf() : null`);
    switches.push({
      idx, ms: +ms.toFixed(1),
      fit: before && after ? after.fit - before.fit : null,
      resize: before && after ? after.resize - before.resize : null,
      mount: before && after ? after.mount - before.mount : null,
    });
    await sleep(120);
  }
  const p2 = await S('Profiler.stop');
  out.tabSwitch = switches;
  out.tabSwitchSummary = {
    p50: (() => { const a = switches.map((x) => x.ms).sort((x, y) => x - y); return a[Math.floor(a.length / 2)]; })(),
    max: Math.max(...switches.map((x) => x.ms)),
    totalFit: switches.reduce((a, x) => a + (x.fit || 0), 0),
    totalResize: switches.reduce((a, x) => a + (x.resize || 0), 0),
    totalMount: switches.reduce((a, x) => a + (x.mount || 0), 0),
  };
  out.tabSwitchProfile = summarize(p2.profile, 10);

  // ---------- 4. 终端写入路径：单帧 vs 多帧 ----------
  out.writePath = await ev(`(async () => {
    const perf = window.__termPerf ? window.__termPerf() : null;
    return perf;
  })()`);

  console.log(JSON.stringify(out, null, 2));
  if (process.env.PERF_OUT) {
    const fs = await import('node:fs');
    fs.writeFileSync(process.env.PERF_OUT, JSON.stringify(out, null, 2));
  }
  c.ws.close();
}
main().catch((e) => { console.error('PERF_DIAG_FAILED', e?.stack || e); process.exit(1); });
