/**
 * 真机（Tauri/WebView2）交互延迟探针：直接对着运行中的 Release 应用测，
 * 覆盖「用户能感知」的操作，避免浏览器 mock 掉真实 IPC 成本。
 *
 * 用法：
 *   node scripts/perf-real.mjs 1          # 先确保应用在跑（脚本会拉起并连 CDP）
 *   node scripts/perf-real-ui.mjs         # 测量（读取 9333 端口上的页面）
 *
 * 输出：各交互的 p50/p95（毫秒）+ 长任务统计。
 */
const CDP = process.env.PERF_CDP || 'http://127.0.0.1:9333';
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function attach() {
  const t0 = Date.now();
  while (Date.now() - t0 < 15000) {
    try {
      const list = await (await fetch(CDP + '/json/list')).json();
      const page = list.find((t) => t.type === 'page');
      if (page) {
        const ws = new WebSocket(page.webSocketDebuggerUrl);
        let id = 0; const pending = new Map();
        ws.addEventListener('message', (ev) => {
          const m = JSON.parse(ev.data);
          if (m.id && pending.has(m.id)) {
            const p = pending.get(m.id); pending.delete(m.id);
            m.error ? p.reject(new Error(JSON.stringify(m.error))) : p.resolve(m.result);
          }
        });
        await new Promise((res, rej) => { ws.addEventListener('open', res); ws.addEventListener('error', rej); });
        const rawSend = (method, params = {}) => new Promise((resolve, reject) => {
          const mid = ++id; pending.set(mid, { resolve, reject });
          ws.send(JSON.stringify({ id: mid, method, params }));
        });
        // 每个 CDP 调用最多等 8s，避免某一个挂住拖死整轮测量
        const send = (method, params = {}) => Promise.race([
          rawSend(method, params),
          new Promise((_, rej) => setTimeout(() => rej(new Error('CDP timeout: ' + method)), 8000)),
        ]);
        await send('Runtime.enable');
        await send('Page.enable');
        await send('Performance.enable');
        // 真机窗口若被遮挡/最小化，WebView2 会暂停 rAF 与渲染：先提到前台
        try { await send('Page.bringToFront'); } catch { /* 旧版本 WebView2 */ }
        return { ws, send };
      }
    } catch { /* retry */ }
    await sleep(80);
  }
  throw new Error('CDP page not reachable');
}

const pct = (arr, q) => {
  if (!arr.length) return null;
  const s = [...arr].sort((a, b) => a - b);
  return +s[Math.min(s.length - 1, Math.floor(s.length * q))].toFixed(1);
};
const stat = (arr) => ({ n: arr.length, p50: pct(arr, 0.5), p95: pct(arr, 0.95), max: pct(arr, 1) });

async function main() {
  const { ws, send } = await attach();
  const ev = async (expr) => {
    const r = await send('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true });
    if (r.exceptionDetails) throw new Error(JSON.stringify(r.exceptionDetails).slice(0, 300));
    return r.result.value;
  };
  // 等到 UI 就绪
  await ev(`(async () => { const t = performance.now(); while (performance.now() - t < 15000) { if (document.querySelector('.sidebar')) return 1; await new Promise((r) => setTimeout(r, 30)); } return 0; })()`);
  console.log('visibility:', await ev('document.visibilityState'));
  await sleep(400);

  const out = {};
  const log = (m) => console.log('[phase]', m);

  // 测「事件 → 下一帧已绘制」
  const latency = (expr) => ev(`(() => new Promise((res) => {
    const t0 = performance.now();
    try { ${expr} } catch (e) { res(-1); return; }
    let settled = false;
    const fire = () => { if (!settled) { settled = true; res(performance.now() - t0); } };
    requestAnimationFrame(() => requestAnimationFrame(fire));
    setTimeout(fire, 250);
  }))()`);

  const key = async (k, ctrl, shift) => {
    const base = { key: k, windowsVirtualKeyCode: k.length === 1 ? k.toUpperCase().charCodeAt(0) : 0, modifiers: (ctrl ? 2 : 0) | (shift ? 8 : 0) };
    await send('Input.dispatchKeyEvent', { type: 'keyDown', ...base });
    await send('Input.dispatchKeyEvent', { type: 'keyUp', ...base });
  };

  // ---------- 1. 命令面板开合 ----------
  log("palette");
  const pal = [];
  for (let i = 0; i < 8; i++) {
    pal.push(await latency(`window.dispatchEvent(new KeyboardEvent('keydown',{key:'k',ctrlKey:true,bubbles:true,cancelable:true}))`));
    await sleep(180);
    await ev(`(() => { const el = document.querySelector('.palette input'); if (el) el.blur(); window.dispatchEvent(new KeyboardEvent('keydown',{key:'Escape',bubbles:true,cancelable:true})); return 1; })()`);
    await sleep(140);
  }
  out.paletteOpen = stat(pal.filter((x) => x >= 0));

  // ---------- 2. 新建本地终端（真实 ConPTY）----------
  log("local-term");
  const createStart = Date.now();
  await key('t', true, false);
  const created = await ev(`(async () => { const t = performance.now(); while (performance.now() - t < 15000) { const el = document.querySelector('.termhost .xterm'); if (el) return { ms: performance.now() - t, xterm: true }; await new Promise((r) => setTimeout(r, 16)); } return { ms: -1, xterm: false }; })()`);
  out.localTermCreateMs = Math.round(createStart > 0 ? created.ms : created.ms);
  await sleep(1200);
  out.termCount = await ev(`window.__termCount ? window.__termCount() : null`);
  out.hasRealTerm = await ev(`!!document.querySelector('.termhost .xterm')`);

  // ---------- 3. 单键响应（键盘 → 下一帧）----------
  log("keys");
  const keys = [];
  await ev(`(() => { const ta = document.querySelector('.xterm-helper-textarea'); if (ta) ta.focus(); return 1; })()`);
  for (let i = 0; i < 20; i++) {
    const t0 = Date.now();
    await send('Input.dispatchKeyEvent', { type: 'keyDown', text: 'a', key: 'a', windowsVirtualKeyCode: 65 });
    await send('Input.dispatchKeyEvent', { type: 'keyUp', key: 'a', windowsVirtualKeyCode: 65 });
    await ev(`new Promise((res) => { let done = false; const f = () => { if (!done) { done = true; res(1); } }; requestAnimationFrame(() => requestAnimationFrame(f)); setTimeout(f, 200); })`);
    keys.push(Date.now() - t0);
    await sleep(40);
  }
  out.keyToFrame = stat(keys);

  // ---------- 4. 再建 2 个终端 + 标签切换 ----------
  log("tabs");
  for (let i = 0; i < 2; i++) { await key('t', true, false); await sleep(900); }
  const switchSamples = [];
  for (let i = 0; i < 12; i++) {
    switchSamples.push(await latency(`(() => { const ts = [...document.querySelectorAll('.tabbar .tab')]; const t = ts[${i} % ts.length]; if (t) t.click(); return 1; })()`));
    await sleep(120);
  }
  out.tabSwitch = stat(switchSamples.filter((x) => x >= 0));

  // ---------- 5. 设置面板 ----------
  log("settings");
  const setSamples = [];
  for (let i = 0; i < 6; i++) {
    setSamples.push(await latency(`window.dispatchEvent(new KeyboardEvent('keydown',{key:',',ctrlKey:true,bubbles:true,cancelable:true}))`));
    await sleep(180);
    await ev(`window.dispatchEvent(new KeyboardEvent('keydown',{key:'Escape',bubbles:true,cancelable:true}))`);
    await sleep(140);
  }
  out.settingsOpen = stat(setSamples.filter((x) => x >= 0));

  // ---------- 6. 批量输出往返（真实 IPC：前端 → Rust → ConPTY → Rust → 前端）----------
  // 本地终端的 conhost 会回显输入，因此粘贴大块文本能完整走一遍 term 字节的双向 IPC。
  // 这条路径必须走 base64：同样的 32KB 若按 JSON 数字数组传输会膨胀到 ~120KB。
  log('paste-burst');
  await ev(`(() => { const ta = document.querySelector('.xterm-helper-textarea'); if (ta) ta.focus(); return 1; })()`);
  await sleep(200);
  const burstMetricsBefore = await send('Performance.getMetrics');
  const b0 = Object.fromEntries(burstMetricsBefore.metrics.map((x) => [x.name, x.value]));
  const CHUNK = 'R'.repeat(32 * 1024);
  const pasteStart = Date.now();
  for (let i = 0; i < 5; i++) await send('Input.insertText', { text: CHUNK });
  // 等主线程把回显处理完（连续 3 帧 rAF 且无长任务增长，最多 6s）
  const drain = await ev(`(async () => {
    const t0 = performance.now();
    let stable = 0; let last = -1;
    while (performance.now() - t0 < 6000) {
      await new Promise((r) => new Promise((rr) => { requestAnimationFrame(() => requestAnimationFrame(rr)); setTimeout(rr, 120); }).then(r));
      const now = performance.now();
      if (last >= 0 && now - last < 20) stable += 1; else stable = 0;
      last = now;
      if (stable >= 3) break;
    }
    await new Promise((r) => setTimeout(r, 300));
    return { drainMs: +(performance.now() - t0).toFixed(1) };
  })()`);
  const burstMetricsAfter = await send('Performance.getMetrics');
  const b1 = Object.fromEntries(burstMetricsAfter.metrics.map((x) => [x.name, x.value]));
  out.pasteBurst = {
    bytes: 5 * 32 * 1024,
    wallMs: Date.now() - pasteStart,
    ...drain,
    ScriptDuration: +(b1.ScriptDuration - b0.ScriptDuration).toFixed(3),
    TaskDuration: +(b1.TaskDuration - b0.TaskDuration).toFixed(3),
    LayoutCount: b1.LayoutCount - b0.LayoutCount,
    RecalcStyleCount: b1.RecalcStyleCount - b0.RecalcStyleCount,
  };

  // ---------- 7. 长任务 + 主线程耗时 ----------
  log("summary");
  out.longTasks = await ev(`(() => { const e = performance.getEntriesByType('longtask') || []; const d = e.map((x) => x.duration); return { count: d.length, max: d.length ? +Math.max(...d).toFixed(1) : 0, total: +d.reduce((a, b) => a + b, 0).toFixed(1) }; })()`);
  const m = await send('Performance.getMetrics');
  const map = Object.fromEntries(m.metrics.map((x) => [x.name, x.value]));
  out.mainThread = {
    ScriptDuration: +map.ScriptDuration?.toFixed(2),
    LayoutDuration: +map.LayoutDuration?.toFixed(2),
    RecalcStyleDuration: +map.RecalcStyleDuration?.toFixed(2),
  };

  console.log(JSON.stringify(out, null, 2));
  if (process.env.PERF_OUT) {
    const fs = await import('node:fs');
    fs.writeFileSync(process.env.PERF_OUT, JSON.stringify(out, null, 2));
  }
  ws.close();
}
main().catch((e) => { console.error('PERF_REAL_UI_FAILED', e?.stack || e); process.exit(1); });
