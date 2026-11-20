/**
 * 响应速度基线探针（CDP）：量化用户「能感知」的交互延迟，而不是只数帧。
 *
 * 每个动作记录「事件派发 → 下一帧已绘制」的耗时（rAF×2），并汇总 p50/p95；
 * 同时对高开销阶段做 CPU Profile，输出自身耗时最高的函数，便于定位热点。
 *
 * 前置：
 *   npm run build:e2e && npx vite preview --port 4173 --strictPort
 *   msedge --headless=new --remote-debugging-port=9333 about:blank
 * 用法：node scripts/perf-latency.mjs        （设 PERF_LABEL 可标记为基线/优化后）
 */
const CDP = process.env.PERF_CDP || 'http://127.0.0.1:9333';
const URL_APP = process.env.PERF_URL || 'http://localhost:4173/';
const LABEL = process.env.PERF_LABEL || 'run';
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
  let id = 0;
  const pending = new Map();
  const events = [];
  ws.addEventListener('message', (ev) => {
    const m = JSON.parse(ev.data);
    if (m.id && pending.has(m.id)) {
      const { resolve, reject } = pending.get(m.id);
      pending.delete(m.id);
      m.error ? reject(new Error(JSON.stringify(m.error))) : resolve(m.result);
    } else if (m.method) events.push(m);
  });
  const ready = new Promise((res, rej) => { ws.addEventListener('open', res); ws.addEventListener('error', rej); });
  const send = (method, params = {}, sessionId) => new Promise((resolve, reject) => {
    const mid = ++id;
    pending.set(mid, { resolve, reject });
    ws.send(JSON.stringify({ id: mid, method, params, sessionId }));
  });
  return { ws, ready, send, events };
}

const pct = (arr, q) => {
  if (!arr.length) return null;
  const s = [...arr].sort((a, b) => a - b);
  return +s[Math.min(s.length - 1, Math.floor(s.length * q))].toFixed(2);
};
const stat = (arr) => ({ n: arr.length, p50: pct(arr, 0.5), p95: pct(arr, 0.95), max: pct(arr, 1) });

async function main() {
  const c = client(await connect());
  await c.ready;
  const { targetInfos } = await c.send('Target.getTargets');
  const page = targetInfos.find((t) => t.type === 'page');
  const { sessionId } = await c.send('Target.attachToTarget', { targetId: page.targetId, flatten: true });
  const S = (m, p = {}) => c.send(m, p, sessionId);

  await S('Page.enable');
  await S('Runtime.enable');
  await S('Performance.enable');
  await S('Profiler.enable');
  // 开放 E2E 钩子（仅 e2e 构建生效）：__termText / __termCount
  await S('Page.addScriptToEvaluateOnNewDocument', { source: 'window.__RUTHENIUM_E2E__ = true;' });
  await S('Emulation.setDeviceMetricsOverride', { width: 1440, height: 900, deviceScaleFactor: 1, mobile: false });

  const ev = async (expr) => {
    const r = await S('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true });
    if (r.exceptionDetails) throw new Error(JSON.stringify(r.exceptionDetails).slice(0, 400));
    return r.result.value;
  };
  const metrics = async () => {
    const { metrics: list } = await S('Performance.getMetrics');
    return Object.fromEntries(list.map((m) => [m.name, m.value]));
  };
  const delta = (a, b, keys) => Object.fromEntries(keys.map((k) => [k, +(b[k] - a[k]).toFixed(4)]));
  const KEYS = ['LayoutCount', 'RecalcStyleCount', 'LayoutDuration', 'RecalcStyleDuration', 'ScriptDuration', 'TaskDuration'];

  // 单次交互：动作 → 下一帧已绘制
  const latency = (actionExpr) => ev(`(() => new Promise((res) => {
    const t0 = performance.now();
    try { ${actionExpr} } catch (e) { res({ err: String(e) }); return; }
    requestAnimationFrame(() => requestAnimationFrame(() => res(performance.now() - t0)));
  }))()`);

  const out = { label: LABEL, phases: {} };

  // ---------- 0. 启动 ----------
  await S('Page.navigate', { url: URL_APP + '?perf=' + Date.now() });
  // 启动耗时用 MutationObserver 测量：导航前注入观察器，首次出现 .hostitem 时记录时间戳。
  // 不能先 `await sleep(400)` 再取 performance.now()：固定下限 400ms 会把优化全部淹掉。
  const startup = await ev(`(async () => {
    const t = performance.now();
    while (performance.now() - t < 15000) {
      if (document.querySelector('.hostitem')) break;
      await new Promise((r) => setTimeout(r, 8));
    }
    const hostsAt = performance.now();
    const nav = performance.getEntriesByType('navigation')[0] || {};
    const paints = Object.fromEntries(performance.getEntriesByType('paint').map((p) => [p.name, +p.startTime.toFixed(1)]));
    const res = performance.getEntriesByType('resource');
    const sum = (pred) => res.filter(pred).reduce((a, r) => a + (r.decodedBodySize || r.transferSize || 0), 0);
    return {
      hostsVisibleMs: +hostsAt.toFixed(1),
      domContentLoaded: +(nav.domContentLoadedEventEnd || 0).toFixed(1),
      loadEvent: +(nav.loadEventEnd || 0).toFixed(1),
      firstPaint: paints['first-paint'] ?? null,
      firstContentfulPaint: paints['first-contentful-paint'] ?? null,
      domNodes: document.getElementsByTagName('*').length,
      scriptBytes: sum((r) => r.name.includes('/assets/') && r.name.endsWith('.js')),
      fontBytes: sum((r) => r.name.endsWith('.woff2')),
      jsRequests: res.filter((r) => r.name.includes('/assets/') && r.name.endsWith('.js')).length,
    };
  })()`);
  out.phases.startup = startup;
  await sleep(600);

  // ---------- 1. 覆盖层开合 ----------
  {
    const open = [];
    for (let i = 0; i < 8; i++) {
      open.push(await latency(`document.dispatchEvent(new KeyboardEvent('keydown',{key:'k',ctrlKey:true,bubbles:true,cancelable:true}))`));
      await sleep(160);
      await ev(`document.dispatchEvent(new KeyboardEvent('keydown',{key:'Escape',bubbles:true,cancelable:true})); true`);
      await sleep(120);
    }
    const set = [];
    for (let i = 0; i < 6; i++) {
      set.push(await latency(`document.dispatchEvent(new KeyboardEvent('keydown',{key:',',ctrlKey:true,bubbles:true,cancelable:true}))`));
      await sleep(160);
      await ev(`document.dispatchEvent(new KeyboardEvent('keydown',{key:'Escape',bubbles:true,cancelable:true})); true`);
      await sleep(120);
    }
    out.phases.paletteOpen = stat(open.map((x) => x?.err ? 0 : x));
    out.phases.settingsOpen = stat(set.map((x) => x?.err ? 0 : x));
  }

  // ---------- 2. 建终端 + 打字回显延迟 ----------
  {
    const m0 = await metrics();
    await ev(`(() => { const el = document.querySelector('.hostitem'); el && el.dispatchEvent(new MouseEvent('dblclick', { bubbles: true })); return 1; })()`);
    const t0 = Date.now();
    await ev(`(async () => { const t = performance.now(); while (performance.now() - t < 12000) { if (document.querySelector('.termhost .xterm')) return 1; await new Promise((r) => setTimeout(r, 16)); } return 0; })()`);
    const connectMs = Date.now() - t0;
    await sleep(2200); // 等 mock 握手 + 首屏输出

    // 单键 → 缓冲出现字符 的端到端延迟（用 CDP 真实按键事件，xterm 只认 textarea 的真实输入）
    const keys = [];
    for (let i = 0; i < 24; i++) {
      await ev(`(() => { window.__perfPending = {
        before: (window.__termText ? window.__termText() : '').length,
        t0: performance.now(),
      }; return 1; })()`);
      await S('Input.dispatchKeyEvent', { type: 'keyDown', text: 'a', key: 'a', windowsVirtualKeyCode: 65 });
      await S('Input.dispatchKeyEvent', { type: 'keyUp', key: 'a', windowsVirtualKeyCode: 65 });
      const ms2 = await ev(`(() => new Promise((res) => {
        const p = window.__perfPending;
        if (!p) { res(-1); return; }
        const tick = () => {
          const now = (window.__termText ? window.__termText() : '').length;
          if (now > p.before) { requestAnimationFrame(() => res(performance.now() - p.t0)); return; }
          if (performance.now() - p.t0 > 800) { res(-1); return; }
          setTimeout(tick, 4);
        };
        tick();
      }))()`);
      if (ms2 >= 0) keys.push(ms2);
      await sleep(30);
    }
    const m1 = await metrics();
    out.phases.terminalConnectMs = connectMs;
    out.phases.keystrokeEcho = stat(keys);
    out.phases.keystrokeCost = delta(m0, m1, KEYS);
  }

  // ---------- 3. 标签切换 ----------
  {
    for (let i = 0; i < 5; i++) {
      await ev(`document.dispatchEvent(new KeyboardEvent('keydown',{key:'t',ctrlKey:true,bubbles:true,cancelable:true})); true`);
      await sleep(260);
    }
    const tabs = await ev(`document.querySelectorAll('.tabbar .tab').length`);
    const m0 = await metrics();
    const samples = [];
    for (let i = 0; i < 12; i++) {
      samples.push(await latency(`(() => { const ts = [...document.querySelectorAll('.tabbar .tab')]; const t = ts[${i} % ts.length]; if (t) t.click(); return 1; })()`));
      await sleep(90);
    }
    const m1 = await metrics();
    out.phases.tabs = tabs;
    out.phases.tabSwitch = stat(samples.map((x) => x?.err ? 0 : x));
    out.phases.tabSwitchCost = delta(m0, m1, KEYS);
  }

  // ---------- 4. 侧栏搜索（每键过滤） ----------
  {
    const per = [];
    await ev(`(() => { const i = document.querySelector('#sideSearch'); i && i.focus(); return 1; })()`);
    for (const ch of 'web-01staging') {
      per.push(await latency(`(() => { const i = document.querySelector('#sideSearch');
        if (!i) return 0;
        const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
        setter.call(i, (i.value || '') + ${JSON.stringify(ch)});
        i.dispatchEvent(new Event('input', { bubbles: true }));
        return 1; })()`));
      await sleep(40);
    }
    out.phases.sidebarSearch = stat(per.map((x) => x?.err ? 0 : x));
    await ev(`(() => { const i = document.querySelector('#sideSearch');
      if (i) { const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set; setter.call(i, ''); i.dispatchEvent(new Event('input', { bubbles: true })); }
      return 1; })()`);
  }

  // ---------- 5. 终端大批量输出（模拟 cat 大文件）----------
  {
    const m0 = await metrics();
    await ev(`(() => { const ta = document.querySelector('.xterm-helper-textarea'); if (ta) ta.focus(); return 1; })()`);
    const before = await ev(`(window.__termText ? window.__termText() : '').length`);
    const t0 = Date.now();
    const payload = ('RUTHENIUM_PERF_LINE_' + 'x'.repeat(120) + '\r\n');
    for (let i = 0; i < 300; i++) await S('Input.insertText', { text: payload });
    // 等到缓冲区不再增长（稳定 2 帧）或超时
    const drain = await ev(`(() => new Promise((res) => {
      const t = performance.now(); const before = ${before};
      let last = -1, stable = 0, lastLen = -1;
      const tick = () => {
        const len = (window.__termText ? window.__termText() : '').length;
        if (len === lastLen) stable++; else { stable = 0; lastLen = len; }
        if ((len > before && stable >= 2) || performance.now() - t > 12000) {
          requestAnimationFrame(() => requestAnimationFrame(() => res({ ms: performance.now() - t, grown: len - before })));
          return;
        }
        setTimeout(tick, 16);
      };
      tick();
    }))()`);
    const m1 = await metrics();
    out.phases.terminalBurst = { wallMs: Date.now() - t0, ...drain, cost: delta(m0, m1, KEYS) };
  }

  // ---------- 6. 侧栏拖拽（布局抖动）----------
  {
    const sp = await ev(`(() => { const e = document.querySelector('.splitter'); if (!e) return null; const r = e.getBoundingClientRect(); return { x: Math.round(r.x) + 1, y: 400 }; })()`);
    if (sp) {
      const m0 = await metrics();
      await S('Input.dispatchMouseEvent', { type: 'mousePressed', x: sp.x, y: sp.y, button: 'left', clickCount: 1 });
      for (let i = 0; i < 240; i++) await S('Input.dispatchMouseEvent', { type: 'mouseMoved', x: sp.x + (i % 120), y: sp.y, button: 'left' });
      await S('Input.dispatchMouseEvent', { type: 'mouseReleased', x: sp.x + 60, y: sp.y, button: 'left' });
      await sleep(250);
      const m1 = await metrics();
      out.phases.drag = { events: 240, ...delta(m0, m1, KEYS) };
    }
  }

  // ---------- 7. CPU Profile：标签切换 + 打字 ----------
  {
    await S('Profiler.start');
    for (let i = 0; i < 10; i++) {
      await ev(`(() => { const ts = [...document.querySelectorAll('.tabbar .tab')]; const t = ts[${i} % ts.length]; if (t) t.click(); return 1; })()`);
      await sleep(60);
      await ev(`(() => { const ta = document.querySelector('.xterm-helper-textarea'); if (ta) ta.focus();
        for (let k = 0; k < 8; k++) document.dispatchEvent(new KeyboardEvent('keydown', { key: 'b', bubbles: true }));
        return 1; })()`);
      await sleep(80);
    }
    const { profile } = await S('Profiler.stop');
    // 汇总自身耗时（self time）Top 15
    const byId = new Map(profile.nodes.map((n) => [n.id, n]));
    const self = new Map();
    const dt = profile.timeDeltas || [];
    for (let i = 0; i < (profile.samples || []).length; i++) {
      const id = profile.samples[i];
      const d = dt[i] || 0;
      if (d <= 0) continue;
      const n = byId.get(id);
      if (!n) continue;
      const cf = n.callFrame || {};
      const key = `${cf.functionName || '(anonymous)'} @ ${String(cf.url || '').split('/').pop()}:${cf.lineNumber ?? 0}`;
      self.set(key, (self.get(key) || 0) + d);
    }
    out.phases.cpuTop = [...self.entries()].sort((a, b) => b[1] - a[1]).slice(0, 15)
      .map(([k, v]) => ({ fn: k, ms: +(v / 1000).toFixed(1) }));
  }

  // ---------- 8. 长任务统计 ----------
  out.phases.longTasks = await ev(`(() => {
    const e = performance.getEntriesByType('longtask') || [];
    const d = e.map((x) => +x.duration.toFixed(1));
    return { count: d.length, totalMs: +d.reduce((a, b) => a + b, 0).toFixed(1), max: d.length ? Math.max(...d) : 0 };
  })()`);

  const json = JSON.stringify(out, null, 2);
  if (process.env.PERF_OUT) {
    const fs = await import('node:fs');
    fs.writeFileSync(process.env.PERF_OUT, json);
    console.log('written:', process.env.PERF_OUT);
  }
  console.log(json);
  c.ws.close();
}

main().catch((e) => { console.error('PERF_LATENCY_FAILED', e?.stack || e); process.exit(1); });
