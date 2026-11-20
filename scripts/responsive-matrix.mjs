/**
 * 分辨率 × 缩放 适配矩阵检查。
 *
 * 检查项（任何一项失败都说明该组合下布局不可用）：
 *   1. 页面无横向/纵向溢出（不该出现滚动条）
 *   2. 常驻区域（titlebar/sidebar/tabbar/stage/statusbar）都在视口内
 *   3. 关键控件文字不被裁切（scrollWidth > clientWidth 即裁切）
 *   4. 打开的 Sheet / Modal / Palette 完整位于视口内，body 可滚动而不是被压扁
 *   5. SFTP 双栏最小可用宽度
 *
 * 用法：node scripts/responsive-matrix.mjs
 *   先起 preview(4173) 与 headless Edge(9333)
 */
const CDP = process.env.RM_CDP || 'http://127.0.0.1:9333';
const URL_APP = process.env.RM_URL || 'http://localhost:4173/';

const SIZES = [
  [700, 480],
  [820, 520],
  [900, 560],
  [1024, 640],
  [1280, 800],
  [1440, 900],
  [1920, 1080],
  [2560, 1440],
];
const SCALES = [1, 1.25, 1.5, 2];

async function connect() {
  for (let i = 0; i < 100; i++) {
    try {
      const r = await fetch(CDP + '/json/version');
      if (r.ok) return (await r.json()).webSocketDebuggerUrl;
    } catch {
      /* retry */
    }
    await new Promise((r) => setTimeout(r, 200));
  }
  throw new Error('CDP not ready');
}
function client(u) {
  const ws = new WebSocket(u);
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

const CHECK = `(() => {
  const vw = innerWidth, vh = innerHeight;
  const out = { vw, vh, dpr: devicePixelRatio, problems: [] };
  const add = (m) => out.problems.push(m);

  // 1) 页面溢出
  const de = document.documentElement;
  if (de.scrollWidth > vw + 1) add('页面横向溢出 ' + de.scrollWidth + '>' + vw);
  if (de.scrollHeight > vh + 1) add('页面纵向溢出 ' + de.scrollHeight + '>' + vh);

  // 2) 常驻区域在视口内
  for (const sel of ['.titlebar', '.sidebar', '.tabbar', '.stage', '.statusbar', '.workspace']) {
    const el = document.querySelector(sel);
    if (!el) { add('缺少 ' + sel); continue; }
    const r = el.getBoundingClientRect();
    if (r.width <= 0 || r.height <= 0) add(sel + ' 尺寸为 0');
    if (r.right > vw + 1) add(sel + ' 超出右边界 +' + Math.round(r.right - vw));
    if (r.bottom > vh + 1) add(sel + ' 超出下边界 +' + Math.round(r.bottom - vh));
  }

  // 3) 关键控件文字裁切（排除有意 ellipsis 的 .tn/.hname/.hsub）
  const clip = [];
  for (const el of document.querySelectorAll('.titlebar button, .statusbar button, .toolbar button, .seg button, .pill, .tbtn, .winctl button')) {
    if (!el.offsetParent) continue;
    const cs = getComputedStyle(el);
    if (cs.overflow === 'hidden' && cs.textOverflow === 'ellipsis') continue;
    if (el.scrollWidth > el.clientWidth + 1) clip.push((el.textContent || el.title || el.className).trim().slice(0, 18));
  }
  if (clip.length) add('文字被裁切: ' + clip.join(' | '));

  // 3b) 状态栏内容被裁切：flex 子项会被压缩，必须逐个 span 比较
  const sb = document.querySelector('.statusbar');
  if (sb) {
    const clipped = [];
    for (const el of sb.children) {
      if (!(el instanceof HTMLElement)) continue;
      if (el.scrollWidth > el.clientWidth + 1) clipped.push((el.textContent || '').trim().slice(0, 20));
    }
    if (clipped.length) add('状态栏项被裁切: ' + clipped.join(' | '));
  }

  // 3c) 标题栏搜索触发框是否被压得过窄
  const ct = document.querySelector('.cmdtrigger');
  if (ct) {
    const w = ct.getBoundingClientRect().width;
    if (w < 120) add('标题栏搜索框被压缩到 ' + Math.round(w) + 'px');
    if (ct.scrollWidth > ct.clientWidth + 1) add('标题栏搜索框文字被裁切');
  }

  // 3d) 标签栏：标签是否可用（溢出应收起为横向滚动而不是被压扁）
  const tb = document.querySelector('.tabbar');
  if (tb) {
    const cs = getComputedStyle(tb);
    if (tb.scrollWidth > tb.clientWidth + 1 && !['auto', 'scroll'].includes(cs.overflowX)) {
      add('标签栏溢出但不可滚动');
    }
    for (const t of tb.querySelectorAll('.tab')) {
      if (t.getBoundingClientRect().width < 60) { add('标签被压扁 <60px'); break; }
    }
  }

  // 3e) 侧栏分段控件三段文字是否都完整
  for (const b of document.querySelectorAll('.seg button')) {
    if (b.scrollWidth > b.clientWidth + 1) add('分段控件文字裁切: ' + b.textContent.trim());
  }

  // 4) 子元素越出父容器（overflow:hidden 会把溢出裁掉，scrollWidth 不增长，必须逐子项比较）
  const containers = ['.titlebar', '.statusbar', '.toolbar', '.seg', '.sidefoot', '.panehead', '.sheetfoot'];
  for (const sel of containers) {
    const box = document.querySelector(sel);
    if (!box) continue;
    const br = box.getBoundingClientRect();
    const cs = getComputedStyle(box);
    if (cs.overflowX === 'auto' || cs.overflowX === 'scroll') continue; // 可滚动即为设计内行为
    const over = [];
    for (const el of box.children) {
      if (!(el instanceof HTMLElement) || !el.offsetParent) continue;
      const r = el.getBoundingClientRect();
      if (r.right > br.right + 1.5) over.push((el.textContent || el.getAttribute('title') || el.className).trim().slice(0, 16) + '+' + Math.round(r.right - br.right));
      if (r.left < br.left - 1.5) over.push((el.textContent || el.className).trim().slice(0, 16) + '-' + Math.round(br.left - r.left));
    }
    if (over.length) add(sel + ' 子项越界: ' + over.join(' | '));
  }

  // 5) SFTP 双栏
  const panes = [...document.querySelectorAll('.pane')];
  if (panes.length) {
    for (const p of panes) {
      const w = p.getBoundingClientRect().width;
      if (w < 260) add('SFTP 栏过窄 ' + Math.round(w) + 'px');
    }
  }
  return out;
})()`;

const OVERLAY_CHECK = `(() => {
  const vw = innerWidth, vh = innerHeight;
  const problems = [];
  const el = document.querySelector('.sheet, .modal, .palette, .encpop');
  if (!el) return { problems: ['未打开 overlay'], rect: null };
  const r = el.getBoundingClientRect();
  if (r.left < -1 || r.top < -1) problems.push('overlay 超出左上');
  if (r.right > vw + 1) problems.push('overlay 超出右 +' + Math.round(r.right - vw));
  if (r.bottom > vh + 1) problems.push('overlay 超出下 +' + Math.round(r.bottom - vh));
  const body = el.querySelector('.sheetbody');
  if (body && body.scrollHeight > body.clientHeight + 1) {
    // 允许滚动，但内容不能被裁掉（overflow-y 必须是 auto/scroll）
    const oy = getComputedStyle(body).overflowY;
    if (oy !== 'auto' && oy !== 'scroll') problems.push('sheet body 内容溢出且不可滚动');
  }
  return { problems, rect: { w: Math.round(r.width), h: Math.round(r.height) } };
})()`;

const main = async () => {
  const c = client(await connect());
  await c.ready;
  const { targetInfos } = await c.send('Target.getTargets');
  const page = targetInfos.find((t) => t.type === 'page');
  const { sessionId } = await c.send('Target.attachToTarget', { targetId: page.targetId, flatten: true });
  const S = (m, p = {}) => c.send(m, p, sessionId);
  await S('Page.enable');
  await S('Runtime.enable');
  const ev = async (e) => {
    const r = await S('Runtime.evaluate', { expression: e, returnByValue: true, awaitPromise: true });
    if (r.exceptionDetails) throw new Error(JSON.stringify(r.exceptionDetails));
    return r.result.value;
  };

  let total = 0;
  const failures = [];

  for (const [w, h] of SIZES) {
    for (const dsf of SCALES) {
      await S('Emulation.setDeviceMetricsOverride', { width: w, height: h, deviceScaleFactor: dsf, mobile: false });
      await S('Page.navigate', { url: URL_APP });
      await sleep(2200);
      total++;
      const res = await ev(CHECK);
      if (res.problems.length) failures.push({ w, h, dsf, where: '主界面', problems: res.problems });

      // 打开设置（最宽的 Sheet 之一）
      await ev(`document.dispatchEvent(new KeyboardEvent('keydown',{key:',',ctrlKey:true,bubbles:true})); true`);
      await sleep(600);
      const ov = await ev(OVERLAY_CHECK);
      if (ov.problems.length) failures.push({ w, h, dsf, where: '设置 Sheet', problems: ov.problems, rect: ov.rect });
      await ev(`document.dispatchEvent(new KeyboardEvent('keydown',{key:'Escape',bubbles:true})); true`);
      await sleep(250);

      // 打开覆盖表（宽版 560）
      const covBtn = await ev(`(() => { const b=[...document.querySelectorAll('.sidefoot button')][2]; if (b) { b.click(); return true; } return false; })()`);
      await sleep(700);
      if (covBtn) {
        const cov = await ev(OVERLAY_CHECK);
        if (cov.problems.length) failures.push({ w, h, dsf, where: '覆盖表 Sheet', problems: cov.problems, rect: cov.rect });
        await ev(`document.dispatchEvent(new KeyboardEvent('keydown',{key:'Escape',bubbles:true})); true`);
        await sleep(200);
      }
    }
  }

  console.log(`\n组合数: ${total}  失败: ${failures.length}`);
  for (const f of failures) {
    console.log(`  [${f.w}x${f.h} @${f.dsf}x] ${f.where}: ${f.problems.join('; ')}${f.rect ? ' rect=' + JSON.stringify(f.rect) : ''}`);
  }
  c.ws.close();
  process.exit(failures.length ? 1 : 0);
};

main().catch((e) => {
  console.error('MATRIX_FAILED', e?.stack || e);
  process.exit(2);
});
