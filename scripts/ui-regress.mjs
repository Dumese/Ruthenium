// UI 回归：锁定窄窗与多语言下的 6 条布局不变式。
// 依赖：preview(4173) + headless Edge(9333)，与 responsive-matrix / ui-sweep 相同。
//   node scripts/ui-regress.mjs
import fs from 'node:fs';
import os from 'node:os';

const CDP = process.env.CDP || 'http://127.0.0.1:9333';
const APP = process.env.APP_URL || 'http://localhost:4173/';
const OUT = process.env.E2E_OUT || os.tmpdir();
const SHOTS = `${OUT}/regress`;
fs.mkdirSync(SHOTS, { recursive: true });

const results = [];
function record(name, ok, detail) {
  results.push({ name, ok, detail });
  console.log(`[${ok ? 'PASS' : 'FAIL'}] ${name}${detail ? ' — ' + detail : ''}`);
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function connect() {
  for (let i = 0; i < 100; i++) {
    try {
      const r = await fetch(CDP + '/json/version');
      if (r.ok) return (await r.json()).webSocketDebuggerUrl;
    } catch { /* retry */ }
    await sleep(200);
  }
  throw new Error('CDP not ready（需要 headless Edge 开 9333）');
}

function client(u) {
  const ws = new WebSocket(u);
  let id = 0;
  const pending = new Map();
  ws.addEventListener('message', (ev) => {
    const m = JSON.parse(ev.data);
    if (m.id && pending.has(m.id)) {
      const p = pending.get(m.id);
      pending.delete(m.id);
      m.error ? p.reject(new Error(JSON.stringify(m.error))) : p.resolve(m.result);
    }
  });
  const ready = new Promise((res, rej) => {
    ws.addEventListener('open', res);
    ws.addEventListener('error', rej);
  });
  const send = (method, params = {}, sessionId) => new Promise((resolve, reject) => {
    const mid = ++id;
    pending.set(mid, { resolve, reject });
    ws.send(JSON.stringify({ id: mid, method, params, sessionId }));
  });
  return { ws, ready, send };
}

const main = async () => {
  const c = client(await connect());
  await c.ready;
  const { targetInfos } = await c.send('Target.getTargets');
  const page = targetInfos.find((t) => t.type === 'page');
  const { sessionId } = await c.send('Target.attachToTarget', { targetId: page.targetId, flatten: true });
  const S = (m, p = {}) => c.send(m, p, sessionId);
  await S('Page.enable');
  await S('Runtime.enable');
  const ev = async (e) => (await S('Runtime.evaluate', { expression: e, returnByValue: true, awaitPromise: true })).result.value;
  const size = (w, h) => S('Emulation.setDeviceMetricsOverride', { width: w, height: h, deviceScaleFactor: 1, mobile: false });
  const shot = async (name) => {
    const { data } = await S('Page.captureScreenshot', { format: 'png' });
    fs.writeFileSync(`${SHOTS}/${name}.png`, Buffer.from(data, 'base64'));
  };
  const openSettings = async () => {
    await ev(`(() => { const e = new KeyboardEvent('keydown', { key: ',', code: 'Comma', ctrlKey: true, bubbles: true, cancelable: true }); document.body.dispatchEvent(e); return !!document.querySelector('.sheet'); })()`);
    await sleep(450);
  };
  const closeSheet = async () => {
    await ev(`(() => { const b = document.querySelector('.sheethead .iconbtn'); b && b.click(); return !!b; })()`);
    await sleep(300);
  };
  const pickLang = async (label) => {
    await ev(`(() => { const p = [...document.querySelectorAll('.sheet .pill')].find(x => x.textContent.trim() === ${JSON.stringify(label)}); p && p.click(); return !!p; })()`);
    await sleep(400);
  };
  const openPeers = async () => {
    await ev(`(() => { const b = document.querySelectorAll('.seg button')[3]; b && b.click(); return !!b; })()`);
    await sleep(400);
  };
  const pairByAddr = async (addr) => {
    await ev(`(() => { const i = document.querySelector('[data-peer-addr]'); if (!i) return false; const set = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set; set.call(i, ${JSON.stringify(addr)}); i.dispatchEvent(new Event('input', { bubbles: true })); return true; })()`);
    await sleep(120);
    await ev(`(() => { const b = document.querySelector('.peers-add button'); b && b.click(); return !!b; })()`);
    await sleep(750);
  };
  const modalFit = () => ev(`(() => {
    const m = document.querySelector('.modal'); if (!m) return null;
    const r = m.getBoundingClientRect();
    return { top: Math.round(r.top), bottom: Math.round(r.bottom), h: Math.round(r.height), vh: innerHeight, fits: r.top >= 0 && r.bottom <= innerHeight + 0.5 };
  })()`);

  await size(560, 400);
  await S('Page.navigate', { url: APP });
  await sleep(900);

  // 1) .kv 标签必须单行（长值不得把「本机指纹」挤成两行）
  await openSettings();
  await ev(`(() => { const k = [...document.querySelectorAll('.sheetbody .kv')].find(x => x.textContent.includes('本机指纹')); if (k) k.scrollIntoView({ block: 'center' }); return !!k; })()`);
  await sleep(250);
  const kv = await ev(`(() => {
    const k = [...document.querySelectorAll('.sheetbody .kv')].find(x => x.textContent.includes('本机指纹'));
    const label = k?.querySelector('span:first-child'); if (!label) return null;
    const rg = document.createRange(); rg.selectNodeContents(label);
    return { text: label.textContent.trim(), lines: rg.getClientRects().length };
  })()`);
  record('.kv 标签不换行', kv?.lines === 1, JSON.stringify(kv));
  await closeSheet();

  // 2) Toast 不拦截点击；弹窗打开时退到弹窗之下（z 55：遮罩 50 / 弹窗 60）
  const toast = await ev(`(() => {
    const d = document.createElement('div'); d.className = 'toast'; d.textContent = 'x';
    document.body.appendChild(d);
    const cs = getComputedStyle(d);
    const baseZ = cs.zIndex;
    const m = document.createElement('div'); m.className = 'modal'; document.body.appendChild(m);
    const modalZ = getComputedStyle(d).zIndex;
    m.remove();
    const out = { pointerEvents: cs.pointerEvents, maxWidth: cs.maxWidth, right: Math.round(d.getBoundingClientRect().right), vw: innerWidth, baseZ, modalZ };
    d.remove();
    return out;
  })()`);
  record('Toast 可穿透点击', toast.pointerEvents === 'none' && toast.right <= toast.vw, JSON.stringify(toast));
  record('弹窗打开时 Toast 不盖住弹窗', toast.baseZ === '90' && toast.modalZ === '55', JSON.stringify(toast));

  // 3) 配对弹窗在最小窗口 + en/ja 完整可见
  for (const [lang, addr] of [['English', '192.168.1.88'], ['日本語', '192.168.1.89']]) {
    await openSettings();
    await pickLang(lang);
    await closeSheet();
    await openPeers();
    await pairByAddr(addr);
    const fit = await modalFit();
    record(`配对弹窗 ${lang} 在 560×400 完整可见`, fit?.fits === true, JSON.stringify(fit));
    await shot(`pair-${lang === 'English' ? 'en' : 'ja'}`);
    await ev(`(() => { const b = document.querySelector('[data-pair-disagree]'); b && b.click(); return !!b; })()`);
    await sleep(300);
  }
  // 回到简体中文
  await openSettings();
  await pickLang('简体中文');
  await closeSheet();

  // 4) 水平导航：设备入口存在；主机库表格只在自身容器内横向滚动
  await openSettings();
  await ev(`(() => { const p = [...document.querySelectorAll('.sheet .pill')].find(x => x.textContent.includes('水平标签')); p && p.click(); return !!p; })()`);
  await sleep(300);
  await closeSheet();
  await sleep(400);
  const tabs = await ev(`[...document.querySelectorAll('.tabbar .tab.data')].map(x => x.textContent.trim())`);
  record('水平导航含「设备」数据标签', tabs.some((t) => t.includes('设备')), tabs.join(','));
  await ev(`(() => { const b = [...document.querySelectorAll('.tabbar .tab.data')].find(x => x.textContent.includes('设备')); b && b.click(); return !!b; })()`);
  await sleep(600);
  const peersPage = await ev(`({ page: !!document.querySelector('.peerspage'), panel: !!document.querySelector('.peerspanel') })`);
  record('水平导航可打开设备页', peersPage.page && peersPage.panel, JSON.stringify(peersPage));
  await shot('xnav-peers-560');
  await ev(`(() => { const b = [...document.querySelectorAll('.tabbar .tab.data')].find(x => x.textContent.includes('主机库')); b && b.click(); return !!b; })()`);
  await sleep(600);
  const table = await ev(`(() => {
    const dp = document.querySelector('.datapage'); const w = document.querySelector('.dptable-wrap'); if (!w) return null;
    dp.scrollLeft = 999; const dpScroll = dp.scrollLeft; dp.scrollLeft = 0;
    w.scrollLeft = 999; const wrapScroll = w.scrollLeft; w.scrollLeft = 0;
    return { dpScroll, wrapScroll, wrapClientW: w.clientWidth, wrapScrollW: w.scrollWidth };
  })()`);
  record('主机库表格在自身容器内横向滚动', table && table.dpScroll === 0 && table.wrapScroll > 0, JSON.stringify(table));

  // 5) SFTP 窄栏：固定列让位，文件行不横向溢出
  await openSettings();
  await ev(`(() => { const p = [...document.querySelectorAll('.sheet .pill')].find(x => x.textContent.includes('经典侧栏')); p && p.click(); return !!p; })()`);
  await sleep(300);
  await closeSheet();
  await sleep(400);
  await ev(`(() => { const b = document.querySelectorAll('.seg button')[0]; b && b.click(); return !!b; })()`);
  await sleep(300);
  await ev(`(() => { const it = [...document.querySelectorAll('.hostitem')].find(x => x.textContent.includes('web-01')); if (!it) return false; it.dispatchEvent(new MouseEvent('dblclick', { bubbles: true })); return true; })()`);
  for (let i = 0; i < 30; i++) { await sleep(350); if (await ev(`!!document.querySelector('.termhost .xterm')`)) break; }
  await ev(`(() => { const b = [...document.querySelectorAll('.tabbar .tabplus')].find(x => (x.getAttribute('title') || '').toUpperCase().includes('SFTP')); b && b.click(); return !!b; })()`);
  for (let i = 0; i < 24; i++) { await sleep(300); if (await ev(`!!document.querySelector('.sftp .pane')`)) break; }
  const sftp = await ev(`(() => {
    const panes = [...document.querySelectorAll('.pane')]; const p = panes[0]; if (!p) return null;
    const list = p.querySelector('.filelist'); const row = p.querySelector('.frow');
    const tb = document.querySelector('.sftp .toolbar');
    return { panes: panes.length, side: p.dataset.side, paneW: Math.round(p.getBoundingClientRect().width), listClientW: list?.clientWidth, listScrollW: list?.scrollWidth, rowClientW: row?.clientWidth, rowScrollW: row?.scrollWidth, toolbarH: tb ? Math.round(tb.getBoundingClientRect().height) : null, hasSwitch: !!document.querySelector('[data-sftp-switch]') };
  })()`);
  record('SFTP 窄窗自动切单栏', !!sftp && sftp.panes === 1 && sftp.hasSwitch && sftp.paneW >= 260, JSON.stringify(sftp));
  record('SFTP 窄栏无横向溢出', !!sftp && sftp.listScrollW <= sftp.listClientW && (!sftp.rowScrollW || sftp.rowScrollW <= sftp.rowClientW), JSON.stringify(sftp));
  record('SFTP 工具栏保持单行（≤48px）', !!sftp && sftp.toolbarH <= 48, `h=${sftp?.toolbarH}`);
  const switched = await ev(`(() => { const bs = [...document.querySelectorAll('[data-sftp-switch] button')]; const other = bs.find(x => !x.classList.contains('on')); if (!other) return false; other.click(); return true; })()`);
  await sleep(450);
  const side2 = await ev(`document.querySelector('.pane')?.dataset.side`);
  record('SFTP 单栏可在本机/远端之间切换', switched && !!side2 && side2 !== sftp?.side, `before=${sftp?.side} after=${side2}`);
  // 拖拽辅助：从远端文件行拖到顶部「本机」停留 500ms，应自动切栏
  await ev(`(() => { const b = document.querySelector('[data-sftp-switch] button[data-side="remote"]'); b && b.click(); return !!b; })()`);
  await sleep(450);
  // 远端 home 只有目录（current/logs/releases），先进入 current 才有可拖的文件行
  await ev(`(() => { const row = document.querySelector('.pane[data-side="remote"] .frow.dir[data-name="current"]') || document.querySelector('.pane[data-side="remote"] .frow.dir'); if (!row) return false; row.dispatchEvent(new MouseEvent('dblclick', { bubbles: true })); return true; })()`);
  await sleep(800);
  const dragStart = await ev(`(() => {
    const row = document.querySelector('.pane[data-side="remote"] .frow:not(.dir)'); if (!row) return null;
    const r = row.getBoundingClientRect(); const x = r.x + 20, y = r.y + 8;
    row.dispatchEvent(new PointerEvent('pointerdown', { bubbles: true, cancelable: true, pointerId: 7, button: 0, clientX: x, clientY: y, buttons: 1 }));
    window.dispatchEvent(new PointerEvent('pointermove', { bubbles: true, pointerId: 7, clientX: x + 30, clientY: y + 30, buttons: 1 }));
    return { x, y };
  })()`);
  const sw = await ev(`(() => { const b = document.querySelector('[data-sftp-switch] button[data-side="local"]'); const r = b.getBoundingClientRect(); return { x: Math.round(r.x + r.width / 2), y: Math.round(r.y + r.height / 2) }; })()`);
  await ev(`window.dispatchEvent(new PointerEvent('pointermove', { bubbles: true, pointerId: 7, clientX: ${sw.x}, clientY: ${sw.y}, buttons: 1 }))`);
  await sleep(750);
  const dragSide = await ev(`document.querySelector('.pane')?.dataset.side`);
  record('单栏拖到切换器停留 500ms 自动切栏', !!dragStart && dragSide === 'local', `after=${dragSide}`);
  await ev(`window.dispatchEvent(new PointerEvent('pointerup', { bubbles: true, pointerId: 7, clientX: ${sw.x}, clientY: ${sw.y}, buttons: 0 }))`);
  await sleep(150);
  const stuck = await ev(`document.body.classList.contains('host-dragging')`);
  record('拖拽结束清理 host-dragging', stuck === false, `stuck=${stuck}`);
  await shot('sftp-560');

  // 6) 多标签下右侧操作常驻可见
  for (const n of ['web-02', 'staging-api', 'dev-mbp', 'localhost', 'db-primary']) {
    const ok = await ev(`(() => { const it = [...document.querySelectorAll('.hostitem')].find(x => x.textContent.includes(${JSON.stringify(n)})); if (!it) return false; it.dispatchEvent(new MouseEvent('dblclick', { bubbles: true })); return true; })()`);
    if (ok) await sleep(1200);
  }
  await size(560, 400);
  await sleep(500);
  const tabbar = await ev(`(() => {
    const tb = document.querySelector('.tabbar'); const acts = [...tb.querySelectorAll('.tabplus')];
    const tr = tb.getBoundingClientRect();
    return { count: tb.querySelectorAll('.tab').length, right: Math.round(tr.right), acts: acts.map(e => ({ title: e.getAttribute('title'), right: Math.round(e.getBoundingClientRect().right) })) };
  })()`);
  record('多标签时右侧操作仍在可视区', tabbar.acts.length >= 1 && tabbar.acts.every((a) => a.right <= tabbar.right), JSON.stringify(tabbar));
  const tabLabels = await ev(`[...document.querySelectorAll('.tabbar .tab:not(.data)')].map(t => {
    const tn = t.querySelector('.tn'); const tx = t.querySelector('.tx');
    return { text: tn.textContent.trim(), clipped: tn.scrollWidth > tn.clientWidth + 1, closeDisplay: tx ? getComputedStyle(tx).display : null };
  })`);
  const plain = tabLabels.find((l) => l.text === 'web-01');
  record('窄窗标签名不再只剩 1–2 字符', !!plain && !plain.clipped && plain.closeDisplay === 'none', JSON.stringify(tabLabels.slice(0, 4)));
  await shot('tabs-560');

  const failed = results.filter((r) => !r.ok);
  console.log(`\nUI 回归: ${results.length - failed.length}/${results.length} 通过`);
  c.ws.close();
  process.exit(failed.length ? 1 : 0);
};

main().catch((e) => { console.error('UI_REGRESS_FAILED', e?.stack || e); process.exit(1); });
