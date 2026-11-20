// 窄窗口下终端查找条是否溢出/遮挡（900×560 / 700×480 / 640×480 / 560×400）
// 560×400 是窗口最小尺寸：Windows 11 贴靠布局只在最小尺寸放得进目标分区时才真的贴靠，
// 分区尺寸是可用区的 1/n（见 src-tauri/src/lib.rs 的 WINDOW_MIN_WIDTH 注释）。
const CDP = process.env.SWEEP_CDP || 'http://127.0.0.1:9333';
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
    else if (m.method === 'Runtime.exceptionThrown') errors.push('EXC ' + String(m.params.exceptionDetails.text).slice(0, 120));
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

  for (const [w, h] of [[900, 560], [700, 480], [640, 480], [560, 400]]) {
    await S('Emulation.setDeviceMetricsOverride', { width: w, height: h, deviceScaleFactor: 1.5, mobile: false });
    await S('Page.navigate', { url: 'http://localhost:4173/?t=' + Date.now() });
    await sleep(2600);
    // 点击终端聚焦
    const rect = await ev(`(() => { const r=document.querySelector('.termhost'); if(!r) return null; const b=r.getBoundingClientRect(); return { x: Math.floor(b.x+b.width/2), y: Math.floor(b.y+80) }; })()`);
    if (!rect) { fails.push(`${w}x${h}: 无终端`); continue; }
    await S('Input.dispatchMouseEvent', { type: 'mousePressed', x: rect.x, y: rect.y, button: 'left', clickCount: 1 });
    await S('Input.dispatchMouseEvent', { type: 'mouseReleased', x: rect.x, y: rect.y, button: 'left', clickCount: 1 });
    await sleep(300);
    await S('Input.dispatchKeyEvent', { type: 'rawKeyDown', modifiers: 2, key: 'f', code: 'KeyF', windowsVirtualKeyCode: 70 });
    await S('Input.dispatchKeyEvent', { type: 'keyUp', modifiers: 2, key: 'f', code: 'KeyF', windowsVirtualKeyCode: 70 });
    await sleep(600);
    const info = await ev(`(() => {
      const f = document.querySelector('.termfind');
      if (!f) return { present: false };
      const r = f.getBoundingClientRect();
      const host = document.querySelector('.termhost')?.getBoundingClientRect();
      const inp = f.querySelector('input')?.getBoundingClientRect();
      return {
        present: true,
        rect: { x: Math.round(r.x), y: Math.round(r.y), w: Math.round(r.width), h: Math.round(r.height) },
        inViewport: r.right <= innerWidth + 1 && r.bottom <= innerHeight + 1 && r.left >= -1 && r.top >= -1,
        inputW: inp ? Math.round(inp.width) : null,
        overlapsTabbar: host ? r.top < host.top + 4 : null,
        vw: innerWidth,
      };
    })()`);
    console.log(`${w}x${h}:`, JSON.stringify(info));
    if (!info.present) fails.push(`${w}x${h}: 查找条未打开`);
    else {
      if (!info.inViewport) fails.push(`${w}x${h}: 查找条超出视口`);
      if (info.inputW !== null && info.inputW < 90) fails.push(`${w}x${h}: 输入框过窄 ${info.inputW}px`);
    }
    await S('Input.dispatchKeyEvent', { type: 'rawKeyDown', key: 'Escape', code: 'Escape', windowsVirtualKeyCode: 27 });
    await S('Input.dispatchKeyEvent', { type: 'keyUp', key: 'Escape', code: 'Escape', windowsVirtualKeyCode: 27 });
    await sleep(250);
    if (await ev(`!!document.querySelector('.termfind')`)) fails.push(`${w}x${h}: Esc 未关闭查找条`);
  }

  if (c.errors.length) fails.push('异常: ' + c.errors.slice(0, 2).join(' / '));
  console.log(`\n结论: ${fails.length ? 'FAIL' : 'PASS'}`);
  for (const f of fails) console.log('  ✗ ' + f);
  c.ws.close();
  process.exit(fails.length ? 1 : 0);
};
main().catch((e) => { console.error('FAILED', e?.stack || e); process.exit(2); });
