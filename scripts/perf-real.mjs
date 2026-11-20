/**
 * 真机（Tauri/WebView2）启动测量：进程启动 → CDP 可用 → 首屏可交互。
 * 需要 exe 已构建（src-tauri/target/release/Ruthenium.exe）。
 * 用法：node scripts/perf-real.mjs [次数]
 * 说明：逐个冷启动测量（只结束**本脚本自己启动**的实例，不碰用户正在用的 Ruthenium），
 * 结束后保留最后一个实例。
 */
import { spawn } from 'node:child_process';
import path from 'node:path';
import fs from 'node:fs';

// 默认相对本仓库推导 exe 路径；不存在时提示用 PERF_EXE 覆盖，避免硬编码某台机器的盘符。
const EXE = process.env.PERF_EXE
  || path.resolve(process.cwd(), 'src-tauri', 'target', 'release', 'Ruthenium.exe');
const CDP = process.env.PERF_CDP || 'http://127.0.0.1:9333';
const RUNS = Number(process.argv[2] || 3);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

if (!fs.existsSync(EXE)) {
  console.error(`perf-real: 找不到 ${EXE}。请先 cargo build --release，或用 PERF_EXE 指定路径。`);
  process.exit(2);
}

// 只结束本脚本自己 spawn 的实例（按 PID），不能 taskkill /IM 无差别强杀
// ——那会连用户正在使用的 Ruthenium（可能有活动传输/正在写配置）一起杀掉。
let myChild = null;
function killMyApp() {
  if (!myChild || myChild.exitCode !== null) return;
  try { myChild.kill(); } catch { /* already gone */ }
}

async function waitCdp(timeoutMs = 20000) {
  const t0 = Date.now();
  while (Date.now() - t0 < timeoutMs) {
    try { const r = await fetch(CDP + '/json/version'); if (r.ok) return Date.now() - t0; } catch { /* retry */ }
    await sleep(60);
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

async function attach() {
  // CDP 端口先于 page target 就绪：轮询直到出现 page 目标
  const t0 = Date.now();
  while (Date.now() - t0 < 10000) {
    const list = await (await fetch(CDP + '/json/list')).json();
    const page = list.find((t) => t.type === 'page');
    if (page) {
      const c = client(page.webSocketDebuggerUrl);
      await c.ready;
      await c.send('Runtime.enable');
      return c;
    }
    await sleep(50);
  }
  throw new Error('no page target');
}

async function evalUntilReady(c) {
  // 页面可能仍在加载：反复 eval，直到 sidebar 存在
  const t0 = Date.now();
  while (Date.now() - t0 < 20000) {
    try {
      const r = await c.send('Runtime.evaluate', {
        expression: `(() => {
          const nav = performance.getEntriesByType('navigation')[0] || {};
          const paints = Object.fromEntries(performance.getEntriesByType('paint').map((p) => [p.name, +p.startTime.toFixed(1)]));
          return {
            ready: !!document.querySelector('.sidebar'),
            appMounted: !!document.querySelector('.app'),
            inPageMs: +performance.now().toFixed(1),
            domContentLoaded: +(nav.domContentLoadedEventEnd || 0).toFixed(1),
            loadEvent: +(nav.loadEventEnd || 0).toFixed(1),
            paints,
            domNodes: document.getElementsByTagName('*').length,
          };
        })()`,
        returnByValue: true,
      });
      const v = r.result?.value;
      if (v && v.ready) return { ...v, attachToReadyMs: Date.now() - t0 };
    } catch { /* context not ready yet */ }
    await sleep(40);
  }
  throw new Error('UI not ready');
}

const results = [];
for (let i = 0; i < RUNS; i++) {
  killMyApp();
  await sleep(600);
  const t0 = Date.now();
  const env = { ...process.env, WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: '--remote-debugging-port=9333' };
  const child = spawn(EXE, [], { env, detached: true, stdio: 'ignore' });
  child.unref();
  myChild = child;
  const cdpMs = await waitCdp();
  const c = await attach();
  const info = await evalUntilReady(c);
  const totalMs = Date.now() - t0;
  results.push({ run: i + 1, processToCdpMs: cdpMs, processToReadyMs: totalMs, ...info });
  console.log(JSON.stringify(results[results.length - 1]));
  c.ws.close();
}

const med = (k) => {
  const a = results.map((r) => r[k]).sort((x, y) => x - y);
  return a[Math.floor(a.length / 2)];
};
const summary = {
  runs: results.length,
  processToCdpMs: med('processToCdpMs'),
  processToReadyMs: med('processToReadyMs'),
  inPageMs: med('inPageMs'),
  domContentLoaded: med('domContentLoaded'),
  loadEvent: med('loadEvent'),
  firstPaint: (() => { const a = results.map((r) => r.paints['first-paint']).filter((x) => x != null).sort((x, y) => x - y); return a[Math.floor(a.length / 2)] ?? null; })(),
};
console.log('===== SUMMARY =====');
console.log(JSON.stringify(summary, null, 2));
if (process.env.PERF_OUT) fs.writeFileSync(process.env.PERF_OUT, JSON.stringify({ results, summary }, null, 2));
