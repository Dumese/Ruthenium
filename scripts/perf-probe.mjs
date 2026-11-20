/**
 * Frame-budget / layout-thrash probe (CDP).
 *
 * 高刷新率（120/240Hz）下每帧预算只有 8.3ms / 4.2ms，真正的瓶颈不是"帧率上限"，
 * 而是每次交互是否触发样式重算与布局。本探针用 CDP Performance 指标量化：
 *   - LayoutCount / RecalcStyleCount   （每次交互触发的布局与样式重算次数）
 *   - LayoutDuration / RecalcStyleDuration / TaskDuration（主线程耗时）
 * 并测量 requestAnimationFrame 的实际节奏（在无头环境仅作参考）。
 *
 * 用法：
 *   npx vite preview --port 4173 --strictPort &
 *   msedge --headless=new --remote-debugging-port=9333 about:blank &
 *   node scripts/perf-probe.mjs
 */
const CDP = process.env.PERF_CDP || 'http://127.0.0.1:9333';
const URL_APP = process.env.PERF_URL || 'http://localhost:4173/';

async function connect() {
  for (let i = 0; i < 100; i++) {
    try {
      const r = await fetch(`${CDP}/json/version`);
      if (r.ok) return (await r.json()).webSocketDebuggerUrl;
    } catch {
      /* retry */
    }
    await new Promise((r) => setTimeout(r, 200));
  }
  throw new Error('CDP not reachable');
}

function client(wsUrl) {
  const ws = new WebSocket(wsUrl);
  let id = 0;
  const pending = new Map();
  ws.addEventListener('message', (ev) => {
    const m = JSON.parse(ev.data);
    if (m.id && pending.has(m.id)) {
      const { resolve, reject } = pending.get(m.id);
      pending.delete(m.id);
      m.error ? reject(new Error(JSON.stringify(m.error))) : resolve(m.result);
    }
  });
  const ready = new Promise((res, rej) => {
    ws.addEventListener('open', res);
    ws.addEventListener('error', rej);
  });
  const send = (method, params = {}, sessionId) =>
    new Promise((resolve, reject) => {
      const mid = ++id;
      pending.set(mid, { resolve, reject });
      ws.send(JSON.stringify({ id: mid, method, params, sessionId }));
    });
  return { ws, ready, send };
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

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
  // PERF_NO_NAV=1：直接测量已运行的真实应用（WebView2 远程调试），不重新导航
  if (process.env.PERF_NO_NAV !== '1') {
    await S('Emulation.setDeviceMetricsOverride', { width: 1440, height: 900, deviceScaleFactor: 1, mobile: false });
    await S('Page.navigate', { url: URL_APP });
    await sleep(3200);
  } else {
    await sleep(1200);
  }

  const ev = async (expr) => {
    const r = await S('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true });
    if (r.exceptionDetails) throw new Error(JSON.stringify(r.exceptionDetails));
    return r.result.value;
  };
  const metrics = async () => {
    const { metrics: list } = await S('Performance.getMetrics');
    return Object.fromEntries(list.map((m) => [m.name, m.value]));
  };
  const delta = (a, b, keys) => Object.fromEntries(keys.map((k) => [k, +(b[k] - a[k]).toFixed(4)]));

  const KEYS = [
    'LayoutCount',
    'RecalcStyleCount',
    'LayoutDuration',
    'RecalcStyleDuration',
    'ScriptDuration',
    'TaskDuration',
  ];

  const out = {};

  // ---- 0. 刷新节奏（无头环境仅参考）----
  out.raf = await ev(`(async () => {
    const ts = [];
    await new Promise((res) => {
      const step = (t) => { ts.push(t); if (ts.length < 90) requestAnimationFrame(step); else res(); };
      requestAnimationFrame(step);
    });
    const d = ts.slice(1).map((t, i) => t - ts[i]).sort((a, b) => a - b);
    const p = (q) => d[Math.min(d.length - 1, Math.floor(d.length * q))];
    return { frames: d.length, p50: +p(0.5).toFixed(2), p95: +p(0.95).toFixed(2), max: +d[d.length-1].toFixed(2) };
  })()`);

  // ---- 1. 侧栏拖拽：模拟 2s @120Hz 的 mousemove（240 次）----
  {
    const sp = await ev(`(() => { const e = document.querySelector('.splitter'); if (!e) return null; const r = e.getBoundingClientRect(); return { x: Math.round(r.x) + 1, y: 400 }; })()`);
    if (sp) {
      const m0 = await metrics();
      await S('Input.dispatchMouseEvent', { type: 'mousePressed', x: sp.x, y: sp.y, button: 'left', clickCount: 1 });
      for (let i = 0; i < 240; i++) {
        await S('Input.dispatchMouseEvent', { type: 'mouseMoved', x: sp.x + (i % 120), y: sp.y, button: 'left' });
      }
      await S('Input.dispatchMouseEvent', { type: 'mouseReleased', x: sp.x + 60, y: sp.y, button: 'left' });
      await sleep(300);
      const m1 = await metrics();
      out.drag = delta(m0, m1, KEYS);
      out.drag.events = 240;
    }
  }

  // ---- 2. 传输进度：制造一次上传并采样整个过程的指标 ----
  {
    await ev(`(() => { const b=[...document.querySelectorAll('.tabplus')].find(x=>(x.getAttribute('title')||'').includes('SFTP')); if (b) b.click(); return true; })()`);
    await sleep(1200);
    const picked = await ev(`(() => { const r = document.querySelector('.pane .frow'); if (!r) return false; r.click(); return true; })()`);
    await sleep(300);
    const m0 = await metrics();
    if (picked) {
      await ev(`(() => { const b=[...document.querySelectorAll('.toolbar button')].find(x=>x.textContent.includes('上传')); if (b) b.click(); return true; })()`);
      await sleep(3000);
    }
    const m1 = await metrics();
    out.transfer = delta(m0, m1, KEYS);
  }

  // ---- 3. 终端大批量输出：粘贴 ~200KB 让 mock 回显 ----
  {
    await ev(`(() => { const t=[...document.querySelectorAll('.tabbar .tab')].find(x=>!x.classList.contains('data')); if (t) t.click(); return true; })()`);
    await sleep(800);
    const m0 = await metrics();
    await ev(`(() => {
      const ta = document.querySelector('.xterm-helper-textarea');
      if (!ta) return false;
      ta.focus();
      return true;
    })()`);
    const payload = 'x'.repeat(4000);
    for (let i = 0; i < 40; i++) await S('Input.insertText', { text: payload });
    await sleep(2500);
    const m1 = await metrics();
    out.terminal = delta(m0, m1, KEYS);
  }

  console.log(JSON.stringify(out, null, 2));
  c.ws.close();
}

main().catch((e) => {
  console.error('PERF_PROBE_FAILED', e?.stack || e);
  process.exit(1);
});
