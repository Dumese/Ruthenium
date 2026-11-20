/**
 * 低配真机性能探针：对**正在运行的真实 Tauri 应用**做限速测试。
 *
 * 目的：本机（Ryzen 9 / 16GB / RTX）远高于 Windows 10 最低配（2 核 1GHz / 2GB / DX9），
 * 用 CDP 把 CPU 限速 4x、6x、20x（近似单核 1GHz 量级的任务耗时），配合
 * 800×600 视口与软件渲染（应用设置里关掉图形加速），逼近低端机的耗时分布。
 *
 * 用法：
 *   node scripts/perf-lowend.mjs                 # 默认 1x/4x/6x
 *   THROTTLE=6 VIEWPORT=1024x768 node scripts/perf-lowend.mjs
 * 前置：真实应用在跑，且带 --remote-debugging-port=9333
 */
const CDP = process.env.PERF_CDP || 'http://127.0.0.1:9333';
const VIEWPORT = process.env.VIEWPORT || '800x600';
const THROTTLES = (process.env.THROTTLE || '1,4,6').split(',').map(Number);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function attach() {
  for (let i = 0; i < 100; i++) {
    try {
      const list = await (await fetch(CDP + '/json/list')).json();
      const page = list.find((t) => t.type === 'page');
      if (page) {
        const ws = new WebSocket(page.webSocketDebuggerUrl);
        let id = 0; const pending = new Map();
        ws.addEventListener('message', (ev) => {
          const m = JSON.parse(ev.data);
          if (m.id && pending.has(m.id)) { const p = pending.get(m.id); pending.delete(m.id); p.resolve(m.result ?? m.error); }
        });
        await new Promise((res, rej) => { ws.addEventListener('open', res); ws.addEventListener('error', rej); });
        const send = (method, params = {}) => new Promise((resolve, reject) => {
          const mid = ++id; pending.set(mid, { resolve, reject });
          ws.send(JSON.stringify({ id: mid, method, params }));
          setTimeout(() => { if (pending.has(mid)) { pending.delete(mid); reject(new Error('CDP timeout: ' + method)); } }, 45000);
        });
        await send('Runtime.enable'); await send('Page.enable'); await send('Performance.enable');
        return { ws, send };
      }
    } catch { /* retry */ }
    await sleep(200);
  }
  throw new Error('CDP page not reachable');
}

const pct = (a, q) => { if (!a.length) return null; const s = [...a].sort((x, y) => x - y); return +s[Math.min(s.length - 1, Math.floor(s.length * q))].toFixed(1); };
const stat = (a) => ({ n: a.length, p50: pct(a, 0.5), p95: pct(a, 0.95), max: pct(a, 1) });

async function main() {
  const { ws, send } = await attach();
  const [w, h] = VIEWPORT.split('x').map(Number);
  const ev = async (expr, awaitPromise = true) => {
    const r = await send('Runtime.evaluate', { expression: expr, returnByValue: true, awaitPromise });
    if (r?.exceptionDetails) throw new Error(JSON.stringify(r.exceptionDetails).slice(0, 300));
    return r?.result?.value;
  };
  const metrics = async () => Object.fromEntries(((await send('Performance.getMetrics'))?.metrics ?? []).map((m) => [m.name, m.value]));

  // 每次导航前注入探针：记录「导航 → 首屏可交互 / 字体就绪」
  await send('Page.addScriptToEvaluateOnNewDocument', {
    source: `window.__probe = { t0: performance.now() };
      document.addEventListener('DOMContentLoaded', () => { window.__probe.dcl = performance.now(); });
      document.fonts.ready.then(() => { window.__probe.fontsReady = performance.now(); });
      const iv = setInterval(() => {
        if (document.querySelector('.sideitem, .hostitem, .sidelist')) { window.__probe.ui = performance.now(); clearInterval(iv); }
      }, 8);`,
  });
  await send('Emulation.setDeviceMetricsOverride', { width: w, height: h, deviceScaleFactor: 1, mobile: false });

  const out = { viewport: VIEWPORT, runs: [] };
  for (const rate of THROTTLES) {
    await send('Emulation.setCPUThrottlingRate', { rate });
    const run = { throttle: rate };

    // ---- A. 启动（忽略缓存，模拟首次安装）----
    await send('Page.reload', { ignoreCache: true });
    await sleep(400);
    // 等 UI 就绪（低配下可能较慢，最多 20s）
    await ev(`(async () => { const t = performance.now(); while (performance.now() - t < 20000) { if (document.querySelector('.sidelist')) return 1; await new Promise((r) => setTimeout(r, 16)); } return 0; })()`);
    await sleep(300);
    run.startup = await ev(`(() => {
      const p = window.__probe || {};
      const nav = performance.getEntriesByType('navigation')[0] || {};
      const f = performance.getEntriesByType('resource').filter((x) => x.name.endsWith('.woff2'));
      return { dcl: +(p.dcl || 0).toFixed(1), fontsReady: +(p.fontsReady || 0).toFixed(1), ui: +(p.ui || 0).toFixed(1),
        load: +(nav.loadEventEnd || 0).toFixed(1), chunks: f.length,
        fontKB: Math.round(f.reduce((a, x) => a + (x.decodedBodySize || x.transferSize), 0) / 1024) };
    })()`);

    // ---- B. 交互延迟：命令面板 / 通知 ----
    const measureFrame = (expr) => ev(`(() => new Promise((res) => {
      const t0 = performance.now();
      try { ${expr} } catch { res(-1); return; }
      let done = false; const fire = () => { if (!done) { done = true; res(performance.now() - t0); } };
      requestAnimationFrame(() => requestAnimationFrame(fire)); setTimeout(fire, 400);
    }))()`);
    const palette = [];
    for (let i = 0; i < 5; i++) {
      palette.push(await measureFrame(`window.dispatchEvent(new KeyboardEvent('keydown',{key:'k',ctrlKey:true,bubbles:true,cancelable:true}))`));
      await sleep(220);
      await ev(`window.dispatchEvent(new KeyboardEvent('keydown',{key:'Escape',bubbles:true,cancelable:true}))`);
      await sleep(160);
    }
    run.paletteOpen = stat(palette.filter((x) => x >= 0));

    // ---- C. 新建本地终端（真实 ConPTY）----
    const t0 = Date.now();
    await send('Input.dispatchKeyEvent', { type: 'keyDown', key: 't', windowsVirtualKeyCode: 84, modifiers: 2 });
    await send('Input.dispatchKeyEvent', { type: 'keyUp', key: 't', windowsVirtualKeyCode: 84, modifiers: 2 });
    const termMs = await ev(`(async () => { const t = performance.now(); while (performance.now() - t < 25000) { if (document.querySelector('.termhost .xterm')) return performance.now() - t; await new Promise((r) => setTimeout(r, 16)); } return -1; })()`);
    run.terminalOpenMs = Math.round(termMs);
    await sleep(900);

    // ---- D. 打字的「按键 → 下一帧」----
    const keys = [];
    await ev(`(() => { const ta = document.querySelector('.xterm-helper-textarea'); if (ta) ta.focus(); return 1; })()`);
    for (let i = 0; i < 10; i++) {
      const s = Date.now();
      await send('Input.dispatchKeyEvent', { type: 'keyDown', text: 'a', key: 'a', windowsVirtualKeyCode: 65 });
      await send('Input.dispatchKeyEvent', { type: 'keyUp', key: 'a', windowsVirtualKeyCode: 65 });
      await ev(`new Promise((r) => { let d = false; const f = () => { if (!d) { d = true; r(1); } }; requestAnimationFrame(() => requestAnimationFrame(f)); setTimeout(f, 400); })`);
      keys.push(Date.now() - s);
      await sleep(40);
    }
    run.keyToFrame = stat(keys);

    // ---- E. 标签切换（再建 2 个）----
    for (let i = 0; i < 2; i++) { await send('Input.dispatchKeyEvent', { type: 'keyDown', key: 't', windowsVirtualKeyCode: 84, modifiers: 2 }); await send('Input.dispatchKeyEvent', { type: 'keyUp', key: 't', windowsVirtualKeyCode: 84, modifiers: 2 }); await sleep(900); }
    const tabs = [];
    for (let i = 0; i < 6; i++) {
      tabs.push(await measureFrame(`(() => { const ts = [...document.querySelectorAll('.tabbar .tab')]; const t = ts[${i} % ts.length]; if (t) t.click(); return 1; })()`));
      await sleep(140);
    }
    run.tabSwitch = stat(tabs.filter((x) => x >= 0));

    // ---- F. 大量输出（4 × 32KB 粘贴，走真实 IPC 往返）----
    const m0 = await metrics();
    const burstStart = Date.now();
    for (let i = 0; i < 4; i++) await send('Input.insertText', { text: 'R'.repeat(32 * 1024) });
    // 大输出进行中：测「按键 → 下一帧」，衡量是否被输出拖到卡死
    const midBurst = [];
    for (let i = 0; i < 3; i++) {
      const kb = Date.now();
      await send('Input.dispatchKeyEvent', { type: 'keyDown', text: 'b', key: 'b', windowsVirtualKeyCode: 66 });
      await send('Input.dispatchKeyEvent', { type: 'keyUp', key: 'b', windowsVirtualKeyCode: 66 });
      await ev(`new Promise((r) => { let d = false; const f = () => { if (!d) { d = true; r(1); } }; requestAnimationFrame(() => requestAnimationFrame(f)); setTimeout(f, 800); })`);
      midBurst.push(Date.now() - kb);
      await sleep(60);
    }
    await ev(`(async () => { const t = performance.now(); let stable = 0, last = -1;
      while (performance.now() - t < 12000) {
        await new Promise((r) => setTimeout(r, 50));
        const now = performance.now();
        if (last >= 0 && now - last < 90) stable++; else stable = 0;
        last = now; if (stable >= 4) break;
      } return 1; })()`);
    const m1 = await metrics();
    run.burstDuringTyping = stat(midBurst);
    run.burst = {
      bytes: 4 * 32 * 1024,
      wallMs: Date.now() - burstStart,
      scriptMs: +((m1.ScriptDuration - m0.ScriptDuration) * 1000).toFixed(0),
      taskMs: +((m1.TaskDuration - m0.TaskDuration) * 1000).toFixed(0),
      layouts: Math.round(m1.LayoutCount - m0.LayoutCount),
    };

    // ---- G. 内存 / DOM ----
    const mm = await metrics();
    const counters = (await send('Memory.getDOMCounters')) ?? {};
    run.mem = {
      jsHeapMB: +((mm.JSHeapUsedSize || 0) / 1024 / 1024).toFixed(1),
      nodes: counters.nodes, listeners: counters.jsEventListeners, documents: counters.documents,
    };

    out.runs.push(run);
    console.log(JSON.stringify(run));
    // 收尾：关闭全部标签
    await ev(`(() => { [...document.querySelectorAll('.tabbar .tab')].forEach((t) => { const x = t.querySelector('.tx'); if (x) x.click(); }); return 1; })()`);
    await sleep(500);
  }
  await send('Emulation.setCPUThrottlingRate', { rate: 1 });
  console.log('\n===== JSON =====');
  console.log(JSON.stringify(out, null, 2));
  ws.close();
}

main().catch((e) => { console.error('PERF_LOWEND_FAILED', e?.stack || e); process.exit(1); });
