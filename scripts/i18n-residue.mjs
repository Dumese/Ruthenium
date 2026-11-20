/**
 * 英文界面 CJK 残留检查（终端缓冲区除外必须为 0）
 * 排除用户数据（主机名/分组名/标签标题）与终端缓冲区。
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
  const ws = new WebSocket(u); let id = 0; const pending = new Map();
  ws.addEventListener('message', (ev) => {
    const m = JSON.parse(ev.data);
    if (m.id && pending.has(m.id)) { const { resolve, reject } = pending.get(m.id); pending.delete(m.id); m.error ? reject(new Error(JSON.stringify(m.error))) : resolve(m.result); }
  });
  const ready = new Promise((res, rej) => { ws.addEventListener('open', res); ws.addEventListener('error', rej); });
  const send = (method, params = {}, sessionId) => new Promise((resolve, reject) => { const mid = ++id; pending.set(mid, { resolve, reject }); ws.send(JSON.stringify({ id: mid, method, params, sessionId })); });
  return { ws, ready, send };
}

// 在给定根节点内收集 CJK 文本，跳过用户数据与终端
//
// 排除策略说明（漏掉任何一项都会让门禁永久为红）：
//   - 结构性选择器只用于"整块都是用户数据/终端"的容器（主机行、标签页、输入框）；
//   - 语义标记 `[data-user]` / `[data-native]` 由组件显式标注：
//       data-user   → 分组名 / 片段名 / 主机名 / 命令原文（用户数据，非界面文案）
//       data-native → 语言名按母语显示（简体中文 / 繁體中文 / 日本語）
//     这类内容在命令面板副标题、`<option>`、分组行里与界面文案**混排**，
//     用类名一刀切会连带掩盖真实泄漏（例如动作项副标题、标签下拉都是 i18n 文案），
//     所以必须由组件自己声明。
const SCAN = (root) => `(() => {
  const CJK = /[\\u3040-\\u30ff\\u3400-\\u4dbf\\u4e00-\\u9fff\\uac00-\\ud7af\\uff01-\\uff60]/;
  const SKIP = '.termhost, .hname, .hsub, .tab, .hostitem, .menuitem input, .sheet input, .palette input, [data-user], [data-native]';
  const root = document.querySelector(${JSON.stringify(root)});
  if (!root) return { found: false, hits: [] };
  const hits = [];
  const walk = (node) => {
    if (node.nodeType === 3) {
      const t = (node.textContent || '').trim();
      if (t && CJK.test(t)) {
        const el = node.parentElement;
        if (el && el.closest(SKIP)) return;
        hits.push(t.slice(0, 40));
      }
      return;
    }
    if (node.nodeType !== 1) return;
    if (node.matches && node.matches(SKIP)) return;
    for (const child of node.childNodes) walk(child);
  };
  walk(root);
  return { found: true, hits };
})()`;

const main = async () => {
  const c = client(await connect()); await c.ready;
  const { targetInfos } = await c.send('Target.getTargets');
  const page = targetInfos.find((t) => t.type === 'page');
  const { sessionId } = await c.send('Target.attachToTarget', { targetId: page.targetId, flatten: true });
  const S = (m, p = {}) => c.send(m, p, sessionId);
  await S('Page.enable'); await S('Runtime.enable');
  const ev = async (e) => (await S('Runtime.evaluate', { expression: e, returnByValue: true, awaitPromise: true })).result.value;

  await S('Emulation.setDeviceMetricsOverride', { width: 1280, height: 800, deviceScaleFactor: 1.5, mobile: false });
  await S('Page.navigate', { url: URL_APP + '?t=' + Date.now() });
  await sleep(2800);

  // 切到 English
  await ev(`document.dispatchEvent(new KeyboardEvent('keydown',{key:',',ctrlKey:true,bubbles:true,cancelable:true})); true`);
  await sleep(500);
  await ev(`(() => { const el=[...document.querySelectorAll('.sheet .pill')].find(x=>(x.textContent||'').trim().includes('English')); if(el) el.click(); return true; })()`);
  await sleep(600);
  console.log('语言 pill 已点击，当前设置标题:', await ev(`document.querySelector('.sheet .sheethead h2')?.textContent.trim()`));

  const scanTargets = [
    ['标题栏', '.titlebar'],
    ['侧栏', '.sidebar'],
    ['状态栏', '.statusbar'],
    ['设置面板', '.sheet'],
  ];
  let total = 0;
  for (const [name, sel] of scanTargets) {
    const res = await ev(SCAN(sel));
    if (!res.found) { console.log(`${name}: 未找到元素`); continue; }
    if (res.hits.length) { total += res.hits.length; console.log(`${name}: 中文残留 ${res.hits.length} 处 → ${res.hits.join(' | ')}`); }
    else console.log(`${name}: 干净`);
  }

  // 关闭设置，再看覆盖表 / 命令面板 / 主机表单 / 右键菜单
  await ev(`document.dispatchEvent(new KeyboardEvent('keydown',{key:'Escape',bubbles:true,cancelable:true})); true`);
  await sleep(300);

  const flows = [
    ['覆盖表', `(() => { const b=[...document.querySelectorAll('.sidefoot button')][2]; if(b) b.click(); return true; })()`, '.sheet'],
    ['命令面板', `document.dispatchEvent(new KeyboardEvent('keydown',{key:'k',ctrlKey:true,bubbles:true,cancelable:true}))`, '.palette'],
    ['新建主机', `document.dispatchEvent(new KeyboardEvent('keydown',{key:'n',ctrlKey:true,bubbles:true,cancelable:true}))`, '.sheet'],
    ['连接信息', `(() => { const b=[...document.querySelectorAll('.statusbar button')].pop(); if(b) b.click(); return true; })()`, '.sheet'],
    ['编码选择', `(() => { const b=document.querySelector('.encbtn'); if(b) b.click(); return true; })()`, '.encpop'],
  ];
  for (const [name, action, sel] of flows) {
    await ev(action);
    await sleep(650);
    const res = await ev(SCAN(sel));
    if (res.found && res.hits.length) { total += res.hits.length; console.log(`${name}: 中文残留 ${res.hits.length} 处 → ${res.hits.join(' | ')}`); }
    else if (res.found) console.log(`${name}: 干净`);
    else console.log(`${name}: 未打开（跳过）`);
    await ev(`document.dispatchEvent(new KeyboardEvent('keydown',{key:'Escape',bubbles:true,cancelable:true})); true`);
    await sleep(250);
  }

  // 主机右键菜单
  await ev(`(() => { const el=document.querySelector('.hostitem'); if(!el) return false; const r=el.getBoundingClientRect(); el.dispatchEvent(new MouseEvent('contextmenu',{bubbles:true,clientX:r.x+8,clientY:r.y+8})); return true; })()`);
  await sleep(500);
  const menuRes = await ev(SCAN('.menu'));
  if (menuRes.found && menuRes.hits.length) { total += menuRes.hits.length; console.log(`主机右键菜单: 中文残留 ${menuRes.hits.length} 处 → ${menuRes.hits.join(' | ')}`); }
  else console.log('主机右键菜单: 干净（或未打开）');

  console.log(`\n英文界面 CJK 残留总计: ${total}`);
  console.log(total === 0
    ? '结论: PASS（界面文案已全部翻译；用户数据 / 语言母语名按标记排除）'
    : `结论: FAIL（${total} 处疑似未翻译，见上方明细）`);
  c.ws.close();
  process.exit(total === 0 ? 0 : 1);
};
main().catch((e) => { console.error('SCAN_FAILED', e?.stack || e); process.exit(2); });
