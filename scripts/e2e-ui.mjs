/**
 * Unattended UI end-to-end suite for Ruthenium.
 *
 * Drives a real Chromium/Edge over CDP against the production build (`vite preview`),
 * where the app automatically runs on the mock IPC layer. Exercises every screen,
 * sheet, picker and workflow, then writes a pass/fail matrix + screenshots.
 *
 * Preconditions (started by the caller):
 *   npx vite preview --port 4173 --strictPort
 *   msedge --headless=new --remote-debugging-port=9333 about:blank
 *
 * Run: node scripts/e2e-ui.mjs
 */
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

const CDP = process.env.E2E_CDP || 'http://127.0.0.1:9333';
const URL_APP = process.env.E2E_URL || 'http://localhost:4173/';
// 默认输出到系统临时目录，避免换机后因硬编码路径而 mkdir 失败
const OUT = process.env.E2E_OUT || os.tmpdir();
const SHOTS = `${OUT}/e2e`;
fs.mkdirSync(SHOTS, { recursive: true });

const results = [];
const shots = [];
function record(group, name, ok, detail) {
  results.push({ group, name, ok, detail });
  const mark = ok === true ? 'PASS' : ok === 'skip' ? 'SKIP' : 'FAIL';
  console.log(`[${mark}] ${group} · ${name}${detail ? ' — ' + detail : ''}`);
}

async function connect() {
  for (let i = 0; i < 100; i++) {
    try {
      const r = await fetch(`${CDP}/json/version`);
      if (r.ok) return (await r.json()).webSocketDebuggerUrl;
    } catch { /* retry */ }
    await new Promise((r) => setTimeout(r, 200));
  }
  throw new Error('CDP not reachable');
}

function client(wsUrl) {
  const ws = new WebSocket(wsUrl);
  let id = 0;
  const pending = new Map();
  // 收集 CDP 事件（Runtime.exceptionThrown 等）：稳定性断言必须读真实页面异常，
  // 从 Node 侧读未定义的全局变量只会永远得到空数组（假绿）。
  const events = [];
  ws.addEventListener('message', (ev) => {
    const m = JSON.parse(ev.data);
    if (m.id && pending.has(m.id)) {
      const { resolve, reject } = pending.get(m.id);
      pending.delete(m.id);
      m.error ? reject(new Error(JSON.stringify(m.error))) : resolve(m.result);
    } else if (m.method) {
      events.push(m);
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
  return { ws, ready, send, events };
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// ---------- page helpers (injected into the page) ----------
const HELPERS = `
window.__t = {
  q: (s, root) => (root || document).querySelector(s),
  qa: (s, root) => [...(root || document).querySelectorAll(s)],
  txt: (s, root) => { const e = window.__t.q(s, root); return e ? e.textContent.replace(/\\s+/g, ' ').trim() : null; },
  visible: (s) => { const e = window.__t.q(s); if (!e) return false; const r = e.getBoundingClientRect(); return r.width > 0 && r.height > 0 && getComputedStyle(e).visibility !== 'hidden'; },
  byText: (sel, text, root) => [...(root || document).querySelectorAll(sel)].find(e => (e.textContent || '').replace(/\\s+/g, ' ').trim().includes(text)) || null,
  byTitle: (sel, text, root) => [...(root || document).querySelectorAll(sel)].find(e => (e.getAttribute('title') || '').includes(text)) || null,
  setValue: (el, v) => {
    const proto = el instanceof HTMLTextAreaElement ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
    Object.getOwnPropertyDescriptor(proto, 'value').set.call(el, v);
    el.dispatchEvent(new Event('input', { bubbles: true }));
    el.dispatchEvent(new Event('change', { bubbles: true }));
  },
  key: (k, mods) => {
    // 只派发一次：同一事件对象重复 dispatch 会让"切换类"快捷键自我抵消
    const target = document.activeElement || document.body;
    const ev = new KeyboardEvent('keydown', Object.assign({ key: k, bubbles: true, cancelable: true }, mods || {}));
    target.dispatchEvent(ev);
  },
  rect: (s) => { const e = window.__t.q(s); if (!e) return null; const r = e.getBoundingClientRect(); return { x: Math.round(r.x), y: Math.round(r.y), w: Math.round(r.width), h: Math.round(r.height) }; },
};
true;
`;

async function main() {
  const c = client(await connect());
  await c.ready;
  const { targetInfos } = await c.send('Target.getTargets');
  const page = targetInfos.find((t) => t.type === 'page');
  const { sessionId } = await c.send('Target.attachToTarget', { targetId: page.targetId, flatten: true });
  const S = (m, p = {}) => c.send(m, p, sessionId);
  await S('Page.enable');
  // 启用测试钩子：WebGL 渲染下 .xterm-rows 无文本，改读终端缓冲区
  await S('Page.addScriptToEvaluateOnNewDocument', { source: 'window.__RUTHENIUM_E2E__ = true;' }).catch(() => {});
  await S('Runtime.enable');
  await S('Emulation.setDeviceMetricsOverride', { width: 1440, height: 900, deviceScaleFactor: 1, mobile: false });

  const errors = [];
  await S('Runtime.addBinding', { name: '__e2eError' }).catch(() => {});
  await S('Page.navigate', { url: URL_APP + (URL_APP.includes('?') ? '&' : '?') + 'e2e=' + Date.now() });
  await sleep(3500);

  // 预检：UI E2E 依赖 mock 数据，必须用 `npm run build:e2e` 构建。
  // 生产构建会剔除 mock，若不预检，后续断言会以难懂的方式失败。
  {
    const probe = await S('Runtime.evaluate', {
      expression: `document.querySelectorAll('.hostitem').length`,
      returnByValue: true,
    });
    if (!probe.result?.value) {
      console.error('E2E 预检失败：页面没有 mock 主机数据。请先执行 `npm run build:e2e`，再启动 `npx vite preview --port 4173 --strictPort`。');
      process.exit(3);
    }
  }

  const raw = async (expr) => {
    const r = await S('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true });
    if (r.exceptionDetails) throw new Error(r.exceptionDetails.text + ' ' + JSON.stringify(r.exceptionDetails.exception?.description ?? ''));
    return r.result.value;
  };
  // 每次求值前注入 helpers（页面导航后仍可用）
  let helpersReady = false;
  const ev = async (expr) => {
    if (!helpersReady) { await raw(HELPERS); helpersReady = true; }
    return raw(expr);
  };
  // console 错误收集
  await S('Log.enable').catch(() => {});

  const shot = async (name) => {
    const { data } = await S('Page.captureScreenshot', { format: 'png' });
    fs.writeFileSync(`${SHOTS}/${name}.png`, Buffer.from(data, 'base64'));
    shots.push(name);
  };

  const waitUntil = async (expr, ms = 4000, step = 120) => {
    const end = Date.now() + ms;
    while (Date.now() < end) {
      if (await ev(expr)) return true;
      await sleep(step);
    }
    return false;
  };

  const clickSel = async (sel, idx = 0) => ev(`(() => { const el = __t.qa('${sel}')[${idx}]; if (!el) return false; el.click(); return true; })()`);
  const clickText = async (sel, text) => ev(`(() => { const el = __t.byText('${sel}', ${JSON.stringify(text)}); if (!el) return false; el.click(); return true; })()`);
  const clickTitle = async (sel, text) => ev(`(() => { const el = __t.byTitle('${sel}', ${JSON.stringify(text)}); if (!el) return false; el.click(); return true; })()`);
  const setVal = async (sel, v) => ev(`(() => { const el = __t.q('${sel}'); if (!el) return false; __t.setValue(el, ${JSON.stringify(v)}); return true; })()`);
  /**
   * 真实鼠标点击行内子元素：mouseClickChild('.hostitem', 'web-02', '.hact')。
   * 不能用合成的 element.click()——它跳过 pointerdown / 指针捕获，覆盖不到
   * 「行 pointerdown 捕获指针 → click 被重定向到整行 → ▶ 单击无效」这类 bug。
   */
  const mouseClickChild = async (rowSel, text, childSel) => {
    const p = await ev(`(() => {
      const row = __t.byText('${rowSel}', ${JSON.stringify(text)});
      const el = row && row.querySelector('${childSel}');
      if (!el) return null;
      el.scrollIntoView({ block: 'center' });
      const r = el.getBoundingClientRect();
      return { x: Math.round(r.x + r.width / 2), y: Math.round(r.y + r.height / 2) };
    })()`);
    if (!p) return false;
    await S('Input.dispatchMouseEvent', { type: 'mouseMoved', x: p.x, y: p.y, button: 'none', buttons: 0 });
    await sleep(80);   // 等 hover 把 ▶ 淡入（opacity 过渡 120ms）
    await S('Input.dispatchMouseEvent', { type: 'mousePressed', x: p.x, y: p.y, button: 'left', buttons: 1, clickCount: 1 });
    await sleep(40);
    await S('Input.dispatchMouseEvent', { type: 'mouseReleased', x: p.x, y: p.y, button: 'left', buttons: 0, clickCount: 1 });
    return true;
  };
  const key = async (k, mods = {}) => ev(`(() => { __t.key(${JSON.stringify(k)}, ${JSON.stringify(mods)}); return true; })()`);
  const overlayClosed = () => ev(`!__t.q('.sheet') && !__t.q('.modal') && !__t.q('.palette') && !__t.q('.menu')`);
  const tabCount = () => ev(`__t.qa('.tabbar .tab').length`);
  const activeTab = () => ev(`__t.txt('.tab.on .tn')`);
  const focusXterm = () => ev(`(() => { const ta = __t.q('.xterm-helper-textarea'); if (!ta) return false; ta.focus(); return true; })()`);
  const dblRow = async (side, name) => {
    const ok = await ev(`(() => { const row = __t.qa('.pane[data-side="${side}"] .frow').find(r => r.dataset.name === ${JSON.stringify(name)}); if (!row) return false; row.dispatchEvent(new MouseEvent('dblclick', { bubbles: true })); return true; })()`);
    await sleep(450);
    return ok;
  };

  // ---------- 右键菜单 helpers ----------
  const rightClickSel = async (sel, idx = 0, dx = 12, dy = 12) => ev(`(() => {
    const el = __t.qa('${sel}')[${idx}]; if (!el) return false;
    const r = el.getBoundingClientRect();
    el.dispatchEvent(new MouseEvent('contextmenu', { bubbles: true, cancelable: true, clientX: r.x + ${dx}, clientY: r.y + ${dy} }));
    return true;
  })()`);
  const rightClickText = async (sel, text, dx = 12, dy = 12) => ev(`(() => {
    const el = __t.byText('${sel}', ${JSON.stringify(text)}); if (!el) return false;
    const r = el.getBoundingClientRect();
    el.dispatchEvent(new MouseEvent('contextmenu', { bubbles: true, cancelable: true, clientX: r.x + ${dx}, clientY: r.y + ${dy} }));
    return true;
  })()`);
  const atPoint = async (x, y, selector) => ev(`(() => {
    const el = ${selector ? `__t.q(${JSON.stringify(selector)})` : 'document.body'}; if (!el) return false;
    el.dispatchEvent(new MouseEvent('contextmenu', { bubbles: true, cancelable: true, clientX: ${x}, clientY: ${y} }));
    return true;
  })()`);
  const menuInfo = () => ev(`(() => {
    const m = __t.q('.menu'); if (!m) return { open: false };
    const r = m.getBoundingClientRect();
    const sb = __t.q('.statusbar');
    const items = [...m.querySelectorAll('.menuitem')].map(e => ({
      t: e.textContent.replace(/\\s+/g, ' ').trim(),
      dis: e.getAttribute('aria-disabled') === 'true',
      act: e.getAttribute('data-act'),
      on: e.classList.contains('on'),
      role: e.getAttribute('role'),
    }));
    return {
      open: true, items, dividers: m.querySelectorAll('.menudiv').length,
      role: m.getAttribute('role'), onCount: items.filter(i => i.on).length,
      rect: { x: r.x, y: r.y, w: r.width, h: r.height },
      vw: innerWidth, vh: innerHeight,
      statusTop: sb ? sb.getBoundingClientRect().top : innerHeight,
      itemH: __t.q('.menu .menuitem') ? Math.round(__t.q('.menu .menuitem').getBoundingClientRect().height) : 0,
      minW: getComputedStyle(m).minWidth, maxW: getComputedStyle(m).maxWidth, radius: getComputedStyle(m).borderRadius,
      inView: r.x >= 7 && r.y >= 7 && r.right <= innerWidth - 7 && r.bottom <= (sb ? sb.getBoundingClientRect().top : innerHeight) + 0.5,
    };
  })()`);
  const tabCountOf = () => ev(`__t.qa('.tabbar .tab').length`);
  const mockCalls = () => ev(`(window.__mockCalls || {})`);

  const closeOverlay = async () => {
    if (!(await overlayClosed())) { await key('Escape'); await sleep(250); }
    if (!(await overlayClosed())) { await clickSel('.scrim'); await sleep(250); }
    return overlayClosed();
  };

  // ============ 0. 启动与布局基线 ============
  {
    const g = await ev(`({
      title: document.title,
      appBar: __t.rect('.titlebar'), side: __t.rect('.sidebar'), status: __t.rect('.statusbar'),
      tabs: __t.rect('.tabbar'), term: __t.rect('.termhost'),
      brand: __t.txt('.tbtitle'), caret: !!__t.q('.tbcaret'),
      winCtl: __t.qa('.winctl button').length,
      docScroll: document.documentElement.scrollHeight, viewport: innerHeight,
      hosts: __t.qa('.hostitem').length,
      tabsCount: __t.qa('.tabbar .tab').length,
      tabStates: __t.qa('.tabbar .tab').map(t => t.dataset.status),
      sidebarText: __t.txt('.sidebar') || '',
      statusText: __t.txt('.statusbar') || '',
      bodyText: document.body.innerText || '',
      hacts: __t.qa('.hostitem .hact').length,
    })`);
    record('0 布局', '标题栏 40px / 侧栏 262px', g.appBar.h === 40 && g.side.w === 262, `${g.appBar.h}×${g.side.w}`);
    record('0 布局', '状态栏 24px 且贴底', g.status.h === 24 && g.status.y + 24 === g.viewport, `y=${g.status.y}`);
    record('0 布局', '标签栏 37px', g.tabs.h === 37, `${g.tabs.h}px`);
    record('0 布局', '页面无滚动溢出', g.docScroll === g.viewport, `${g.docScroll}/${g.viewport}`);
    // 分隔条必须不透明，否则侧栏与标签栏之间会露出 3px 白缝
    const seam = await ev(`(() => {
      const s = document.querySelector('.splitter');
      if (!s) return null;
      const cs = getComputedStyle(s, '::before');
      const r = s.getBoundingClientRect();
      return { beforeH: cs.height, beforeBg: cs.backgroundColor, w: Math.round(r.width) };
    })()`);
    record('0 布局', '侧栏与标签栏之间无白缝（分隔条顶部续 chrome）',
      !!seam && seam.beforeH === '37px' && seam.beforeBg !== 'rgba(0, 0, 0, 0)', JSON.stringify(seam));
    record('0 品牌', '词标 Ruthenium + 光标块', g.brand === 'Ruthenium' && g.caret, `${g.brand} caret=${g.caret}`);
    record('0 品牌', '窗口三键 ─ □ ×', g.winCtl === 3, `${g.winCtl} 个`);
    record('0 数据', '侧栏主机与标签已渲染', g.hosts >= 5 && g.tabsCount >= 2, `hosts=${g.hosts} tabs=${g.tabsCount}`);
    record('0 侧栏', '已移除「最近会话」区块', !g.sidebarText.includes('最近会话') && !g.sidebarText.includes('Recent sessions'), '');
    record('0 侧栏', '每台主机都有行内 hover ▶ 连接入口', g.hacts === g.hosts, `hact=${g.hacts}/${g.hosts}`);
    record('0 状态栏', '状态栏不再常驻配置路径', !g.statusText.includes('hosts.toml') && !g.statusText.includes('%APPDATA%'), (g.statusText || '').slice(0, 90));
    record('0 状态栏', '末尾保留编码 chip 与连接信息入口', !!g.statusText.includes('UTF-8') && !!g.statusText.includes('连接信息'), (g.statusText || '').slice(-40));
    record('0 快捷键', '界面不出现 Command 键符号', !/[\u2318]|Command/.test(g.bodyText), '');
    record('0 会话', '启动会话最终进入 connected（非连接中/断开）', (g.tabStates || []).every((s) => s === 'connected'), (g.tabStates || []).join(','));
    await shot('01-boot');
  }

  // ============ 1/2. 主机库 + 分组折叠 + 搜索 ============
  {
    const before = await ev(`__t.qa('.hostitem').length`);
    await setVal('.sidesearch input', 'web');
    await sleep(350);
    const filtered = await ev(`__t.qa('.hostitem').length`);
    const names = await ev(`__t.qa('.hostitem .hname').map(e => e.textContent.trim())`);
    record('2 搜索', '侧栏搜索实时过滤', filtered > 0 && filtered < before && names.every((n) => n.includes('web')), `${before}→${filtered} [${names.join(',')}]`);
    // 清空搜索，折叠分组
    await setVal('.sidesearch input', '');
    await sleep(300);
    const groupsBefore = await ev(`__t.qa('.hostitem').length`);
    await clickSel('.grouprow');
    await sleep(300);
    const groupsAfter = await ev(`__t.qa('.hostitem').length`);
    record('2 分组', '点击分组行折叠', groupsAfter < groupsBefore, `${groupsBefore}→${groupsAfter}`);
    await clickSel('.grouprow');
    await sleep(250);
    record('2 分组', '再次点击展开', (await ev(`__t.qa('.hostitem').length`)) === groupsBefore, '');
    // 分段切换
    await clickText('.seg button', '片段');
    await sleep(350);
    const snips = await ev(`__t.qa('.listitem').length`);
    await clickText('.seg button', '转发');
    await sleep(350);
    const rules = await ev(`__t.qa('.listitem').length`);
    await clickText('.seg button', '主机');
    await sleep(300);
    record('12/16 侧栏', '片段 / 转发 分段切换', snips >= 3 && rules >= 2, `snips=${snips} rules=${rules}`);
    // 主机 tabs 下不应出现「最近会话」的 slabel / 列表
    const recentGone = await ev(`({ text: __t.txt('.sidelist') || '', labels: __t.qa('.sidelist .slabel').length })`);
    record('2 侧栏', '主机列表下无「最近会话」', !recentGone.text.includes('最近会话') && recentGone.labels === 0, `labels=${recentGone.labels}`);
    // 单击只选中：变蓝高亮，不弹详情 Sheet
    await ev(`(() => { const r = __t.byText('.hostitem', 'web-02'); if (r) r.click(); return true; })()`);
    await sleep(300);
    const sel = await ev(`({ on: __t.byText('.hostitem', 'web-02')?.classList.contains('on'), sheet: !!__t.q('.sheet'), menu: !!__t.q('.menu') })`);
    record('3 主机行', '单击主机只选中（.on）且不弹详情', sel.on && !sel.sheet, JSON.stringify(sel));

  // ============ 2b. 侧栏拖拽换分组（pointer 事件自绘，不用 HTML5 DnD） ============
  {
    const pos = async (sel) => ev(`(() => { const el = __t.q('${sel}'); if (!el) return null; el.scrollIntoView({ block: 'center' });
      const r = el.getBoundingClientRect(); return { x: Math.round(r.left + Math.min(60, r.width / 2)), y: Math.round(r.top + r.height / 2) }; })()`);
    const snapshot = () => ev(`({
      order: [...__t.qa('.sidelist [data-group]')].map((e) => (e.classList.contains('grouprow') ? 'G:' : 'H:') + e.dataset.group + (e.dataset.arg ? '/' + (e.querySelector('.hname') || {}).textContent : '')).slice(0, 26),
      groups: [...__t.qa('.grouprow')].map((g) => g.dataset.group + ':' + (g.querySelector('.chip')?.textContent || '').trim() + (g.classList.contains('closed') ? 'x' : '')),
      ghost: !!__t.q('.hostdrag-ghost'), drop: __t.qa('[data-drop]').length, src: !!__t.q('[data-dragsrc]'),
      body: document.body.className, toast: (__t.txt('.toast') || ''), sheet: !!__t.q('.sheet'), hosts: __t.qa('.hostitem').length })`);
    const dragTo = async (fromSel, toSel, opts = {}) => {
      const from = await pos(fromSel); const to = await pos(toSel);
      if (!from || !to) return { missing: true };
      await sleep(140);
      await S('Input.dispatchMouseEvent', { type: 'mousePressed', x: from.x, y: from.y, button: 'left', buttons: 1, clickCount: 1 });
      await sleep(50);
      for (let i = 1; i <= 8; i++) {
        await S('Input.dispatchMouseEvent', { type: 'mouseMoved', x: Math.round(from.x + (to.x - from.x) * i / 8), y: Math.round(from.y + (to.y - from.y) * i / 8), button: 'left', buttons: 1 });
        await sleep(30);
      }
      await sleep(110);
      const mid = await snapshot();
      if (opts.escape) { await ev(`document.dispatchEvent(new KeyboardEvent('keydown',{key:'Escape',bubbles:true,cancelable:true}))`); await sleep(150); }
      else await S('Input.dispatchMouseEvent', { type: 'mouseReleased', x: to.x, y: to.y, button: 'left', buttons: 0, clickCount: 1 });
      await sleep(480);
      return { mid, after: await snapshot(), to };
    };

    // A. 跨组：web-01 生产环境 → 预发布
    let r = await dragTo('.hostitem[data-arg="h1"]', '.grouprow[data-group="预发布"]');
    record('2 拖拽', '拖主机到别的分组行：移动 + 落点高亮 + ghost + 无残留',
      !!r.after && r.after.order.includes('H:预发布/web-01') && r.mid.src && r.mid.drop >= 1
      && !r.after.ghost && r.after.drop === 0 && r.after.body === '' && r.after.toast.includes('预发布'),
      JSON.stringify({ order: r.after?.order?.slice(0, 10), mid: { src: r.mid?.src, drop: r.mid?.drop }, after: { ghost: r.after?.ghost, drop: r.after?.drop, toast: r.after?.toast } }));

    // B. 同组拖放：no-op（不写盘、不弹 toast）
    const beforeB = await snapshot();
    r = await dragTo('.hostitem[data-arg="h1"]', '.grouprow[data-group="预发布"]');
    record('2 拖拽', '同组拖放是 no-op（分组计数不变）',
      JSON.stringify(r.after.groups) === JSON.stringify(beforeB.groups) && r.after.toast === beforeB.toast,
      JSON.stringify({ before: beforeB.groups, after: r.after.groups }));

    // C. 折叠分组作为落点：自动展开并显示移入的主机
    await clickSel('.grouprow[data-group="开发机"]');
    await sleep(280);
    const collapsed = await ev(`({ closed: __t.q('.grouprow[data-group="开发机"]')?.classList.contains('closed'), hosts: __t.qa('.hostitem').length })`);
    r = await dragTo('.hostitem[data-arg="h1"]', '.grouprow[data-group="开发机"]');
    record('2 拖拽', '拖入折叠分组：自动展开并移入',
      collapsed.closed && r.after.order.includes('H:开发机/web-01') && !r.after.groups.some((g) => g.startsWith('开发机:') && g.endsWith('x')),
      JSON.stringify({ collapsed, groups: r.after.groups, order: r.after.order.slice(0, 12) }));

    // D. Esc 取消：不落盘
    const beforeD = await snapshot();
    r = await dragTo('.hostitem[data-arg="h1"]', '.grouprow[data-group="本地"]', { escape: true });
    record('2 拖拽', '拖拽中按 Esc 取消（分组不变、无残留）',
      JSON.stringify(r.after.groups) === JSON.stringify(beforeD.groups) && !r.after.ghost && r.after.body === '',
      JSON.stringify({ before: beforeD.groups, after: r.after.groups }));

    // E. 拖拽尾随的 click 被吞：拖完立刻单击不改变选中
    const from = await pos('.hostitem[data-arg="h1"]'); const to = await pos('.grouprow[data-group="本地"]');
    await S('Input.dispatchMouseEvent', { type: 'mousePressed', x: from.x, y: from.y, button: 'left', buttons: 1, clickCount: 1 });
    await sleep(40);
    for (let i = 1; i <= 8; i++) {
      await S('Input.dispatchMouseEvent', { type: 'mouseMoved', x: Math.round(from.x + (to.x - from.x) * i / 8), y: Math.round(from.y + (to.y - from.y) * i / 8), button: 'left', buttons: 1 });
      await sleep(28);
    }
    await S('Input.dispatchMouseEvent', { type: 'mouseReleased', x: to.x, y: to.y, button: 'left', buttons: 0, clickCount: 1 });
    const p2 = await pos('.hostitem[data-arg="h1"]');
    await sleep(60);
    const selBefore = await ev(`__t.qa('.hostitem.on .hname').map((e) => e.textContent)`);
    await S('Input.dispatchMouseEvent', { type: 'mousePressed', x: p2.x, y: p2.y, button: 'left', buttons: 1, clickCount: 1 });
    await S('Input.dispatchMouseEvent', { type: 'mouseReleased', x: p2.x, y: p2.y, button: 'left', buttons: 0, clickCount: 1 });
    await sleep(260);
    const selAfter = await ev(`__t.qa('.hostitem.on .hname').map((e) => e.textContent)`);
    record('2 拖拽', '拖拽尾随的单击被吞（不误选中/不误连开）',
      JSON.stringify(selBefore) === JSON.stringify(selAfter) && !(await ev(`!!__t.q('.sheet')`)),
      JSON.stringify({ before: selBefore, after: selAfter }));
    const finalHosts = await ev(`__t.qa('.hostitem').length`);
    record('2 拖拽', '拖拽不会增减主机（总数不变）', finalHosts === 8, String(finalHosts));
  }

  }

  // ============ 3. 新建 / 编辑主机 ============
  {
    await key('n', { ctrlKey: true });
    const opened = await waitUntil(`!!__t.q('.sheet')`);
    const keyField = await ev(`(() => { const i = __t.q('.pickrow input'); return i ? { ro: i.readOnly, ph: i.placeholder, hasBtn: !!__t.q('.pickrow button') } : null; })()`);
    record('3 新建主机', 'Ctrl+N 打开表单 Sheet', opened, '');
    record('3 新建主机', '私钥字段为选择器（含「选择…」按钮）', !!keyField && keyField.hasBtn && !!keyField.ph, JSON.stringify(keyField));
    // 填写并保存
    await ev(`(() => { const ins = __t.qa('.sheet .inp'); __t.setValue(ins[0], 'e2e-host'); __t.setValue(ins[1], '10.1.2.3'); return true; })()`);
    await sleep(200);
    await clickText('.sheetfoot button', '保存');
    await sleep(500);
    const created = await ev(`__t.qa('.hostitem .hname').some(e => e.textContent.trim() === 'e2e-host')`);
    record('3 新建主机', '保存后出现在侧栏', created, created ? '' : '未找到 e2e-host');
    await shot('02-hostform-saved');
  }

  // ============ 4. 主机行交互 / 连接去重 / 详情入口 ============
  {
    const tabs0 = await tabCount();
    // 单击只选中
    await clickText('.hostitem', 'e2e-host');
    await sleep(300);
    const single = await ev(`({ on: __t.byText('.hostitem', 'e2e-host')?.classList.contains('on'), sheet: !!__t.q('.sheet') })`);
    record('4 主机行', '单击只选中且不弹详情', single.on && !single.sheet, JSON.stringify(single));
    // 双击才连接：新建主机默认未信任（known=false）→ 先弹指纹确认，接受后才 connecting
    await ev(`(() => { const el = __t.byText('.hostitem', 'e2e-host'); el && el.dispatchEvent(new MouseEvent('dblclick', { bubbles: true })); return true; })()`);
    const tofu = await waitUntil(`!!__t.q('.modal .fingerprint')`, 3000);
    record('4 主机行', '新建主机默认未信任：连接先弹指纹确认（known=false）', tofu,
      tofu ? (await ev(`(__t.q('.fingerprint')?.textContent || '').replace(/\\s+/g, ' ').trim().slice(0, 60)`)) : 'no hostkey modal');
    if (tofu) {
      await clickText('.modalfoot button', '信任并连接');
    }
    await sleep(250);
    const connecting = await ev(`({ count: __t.qa('.tabbar .tab').length, wait: !!__t.q('.connwait'), status: __t.txt('.statusbar') || '', tab: __t.qa('.tabbar .tab').find(t => t.textContent.includes('e2e-host'))?.dataset.status })`);
    record('4 主机行', '双击主机立即出标签并显示连接中', connecting.wait && connecting.tab === 'connecting' && connecting.count === tabs0 + 1, JSON.stringify(connecting));
    await sleep(1200);
    const connected = await ev(`({ wait: !!__t.q('.connwait'), tab: __t.qa('.tabbar .tab').find(t => t.textContent.includes('e2e-host'))?.dataset.status, xterm: __t.qa('.termhost .xterm').length })`);
    record('8 双击连接', '握手完成后标签进入 connected 并挂载 xterm', !connected.wait && connected.tab === 'connected' && connected.xterm === 1, JSON.stringify(connected));
    // 再次双击同一台：切回已有标签，不重复开
    const tabs1 = await tabCount();
    await ev(`(() => { const el = __t.byText('.hostitem', 'e2e-host'); el && el.dispatchEvent(new MouseEvent('dblclick', { bubbles: true })); return true; })()`);
    await sleep(500);
    const tabs2 = await tabCount();
    record('8 双击连接', '同一主机再次连接切回已有标签（hostId 去重）', tabs2 === tabs1, `${tabs1}→${tabs2}`);
    // 详情仍可从右键菜单打开
    await ev(`(() => {
      const item = __t.byText('.hostitem', 'e2e-host'); if (!item) return false;
      const r = item.getBoundingClientRect();
      item.dispatchEvent(new MouseEvent('contextmenu', { bubbles: true, clientX: r.x + 10, clientY: r.y + 10 }));
      return true;
    })()`);
    await waitUntil(`!!__t.q('.menu')`);
    await clickText('.menu .menuitem', '查看详情');
    await waitUntil(`!!__t.q('.sheet')`);
    const detail = await ev(`({ title: __t.txt('.sheet h3'), ssh: __t.txt('.fingerprint'), copy: !!__t.byText('.sheet button', '复制命令') })`);
    record('4 主机详情', '右键菜单可打开详情（含 ssh 命令）', (detail.ssh || '').includes('ssh ') && detail.copy, detail.ssh || '');
    await shot('03-hostdetail');
    await closeOverlay();
  }

  // ============ 5. 删除主机（确认弹窗） ============
  {
    // 通过数据页删除，确保覆盖 #5 的入口
    await ev(`(() => { const tabs = __t.qa('.tabbar .tab'); return true; })()`);
    await key('k', { ctrlKey: true });
    await waitUntil(`!!__t.q('.palette')`);
    await setVal('.palette input', 'e2e-host');
    await sleep(400);
    await key('Escape');
    await sleep(300);
    await closeOverlay();
    // 打开主机库数据页（nav=x 或首标签）→ 用侧栏右键菜单删除更稳定
    const ctxOpened = await ev(`(() => {
      const item = __t.byText('.hostitem', 'e2e-host'); if (!item) return false;
      const r = item.getBoundingClientRect();
      item.dispatchEvent(new MouseEvent('contextmenu', { bubbles: true, clientX: r.x + 10, clientY: r.y + 10 }));
      return true;
    })()`);
    const menuShown = await waitUntil(`!!__t.q('.menu')`);
    const menuItems = await ev(`__t.qa('.menu .menuitem').map(e => e.textContent.replace(/\\s+/g,' ').trim())`);
    record('4 右键菜单', '主机右键 6 项 + 分隔', menuShown && menuItems.length === 6, menuItems.join(' | '));
    await clickText('.menu .menuitem', '删除');
    const confirmShown = await waitUntil(`!!__t.q('.modal')`);
    record('5 删除主机', '删除弹确认对话框', confirmShown, '');
    await clickText('.modalfoot button', '确认');
    await sleep(600);
    const gone = await ev(`!__t.qa('.hostitem .hname').some(e => e.textContent.trim() === 'e2e-host')`);
    record('5 删除主机', '确认后主机消失且弹窗关闭', gone && (await overlayClosed()), gone ? '' : '仍在列表');
  }

  // ============ 8. 双击连接 / 去重 / 连接过程 / 会话级断线 ============
  {
    // 已有 web-01：双击切回，不新开
    const before = await tabCount();
    await ev(`(() => { const el = __t.byText('.hostitem', 'web-01'); el && el.dispatchEvent(new MouseEvent('dblclick', { bubbles: true })); return true; })()`);
    await sleep(500);
    const after = await tabCount();
    record('8 双击连接', '已有会话双击切回、不重复开标签', after === before && (await activeTab()) === 'web-01', `${before}→${after} active=${await activeTab()}`);

    // 新主机：立即出 connecting 标签（spinner + 取消），900ms 后 connected
    const before2 = await tabCount();
    // 真实鼠标单击 ▶（不是 element.click()）：点 ▶ 不应连接，只有双击才行
    await mouseClickChild('.hostitem', 'web-02', '.hact');
    await sleep(180);
    const conn = await ev(`({
      wait: !!__t.q('.connwait'), spin: !!__t.q('.connspin'), cancel: !!__t.byText('.connwait button', '取消'),
      tab: __t.qa('.tabbar .tab').find(t => t.textContent.includes('web-02'))?.dataset.status,
      count: __t.qa('.tabbar .tab').length, status: __t.txt('.statusbar') || '',
    })`);
    record('5 连接过程', '点击 ▶ 立即出标签（spinner + 正在连接 + 取消）', conn.wait && conn.spin && conn.cancel && conn.tab === 'connecting' && conn.count === before2 + 1, JSON.stringify(conn));
    record('5 连接过程', '连接中状态栏显示「连接中」而非「已断开」', conn.status.includes('连接中') && !conn.status.includes('已断开'), (conn.status || '').slice(0, 80));
    await shot('04-connecting');
    await sleep(1200);
    const done = await ev(`({ wait: !!__t.q('.connwait'), tab: __t.qa('.tabbar .tab').find(t => t.textContent.includes('web-02'))?.dataset.status, xterm: __t.qa('.termhost .xterm').length })`);
    record('5 连接过程', '握手完成后切到终端', !done.wait && done.tab === 'connected' && done.xterm === 1, JSON.stringify(done));

    // 取消：连接中关闭标签，后续不得重建后台标签
    const beforeCancel = await tabCount();
    await mouseClickChild('.hostitem', 'db-primary', '.hact');
    await sleep(160);
    const hadWait = await ev(`!!__t.q('.connwait')`);
    await clickText('.connwait button', '取消');
    await sleep(500);
    const afterCancel = await tabCount();
    record('5 连接过程', '取消关闭连接中标签（不残留标签）', hadWait && afterCancel === beforeCancel, `${beforeCancel}→${afterCancel}`);

    // 会话级断线：只影响当前标签，切到别的标签不串状态
    await ev(`(() => { const t = __t.qa('.tabbar .tab').find(x => x.textContent.includes('web-02')); t && t.click(); return true; })()`);
    await sleep(300);
    await clickTitle('.tabplus', '模拟断线');
    await sleep(500);
    const disc = await ev(`({
      banner: !!__t.q('.banner'), status: __t.txt('.statusbar') || '',
      tab: __t.qa('.tabbar .tab').find(t => t.textContent.includes('web-02'))?.dataset.status,
    })`);
    record('5 会话状态', '断线显示内联横幅并标记标签', disc.banner && disc.tab === 'disconnected' && disc.status.includes('已断开'), JSON.stringify(disc));
    await ev(`(() => { const t = __t.qa('.tabbar .tab').find(x => x.textContent.includes('web-01')); t && t.click(); return true; })()`);
    await sleep(350);
    const other = await ev(`({ banner: !!__t.q('.banner'), otherTab: __t.qa('.tabbar .tab').find(t => t.textContent.includes('web-02'))?.dataset.status })`);
    record('5 会话状态', '后台标签断线不影响当前标签', !other.banner && other.otherTab === 'disconnected', JSON.stringify(other));
    await ev(`(() => { const t = __t.qa('.tabbar .tab').find(x => x.textContent.includes('web-02')); t && t.click(); return true; })()`);
    await sleep(350);
    record('5 会话状态', '切回断线标签横幅仍在', (await ev(`!!__t.q('.banner')`)) === true, '');
    await clickText('.banner button', '重新连接');
    await sleep(1400);
    const re = await ev(`({ banner: !!__t.q('.banner'), tab: __t.qa('.tabbar .tab').find(t => t.textContent.includes('web-02'))?.dataset.status })`);
    record('5 会话状态', '重新连接后恢复 connected 且横幅消失', !re.banner && re.tab === 'connected', JSON.stringify(re));
    await shot('04-terminal');
  }

  // ============ 11/13. 终端输入 + 片段插入（只写不执行） ============
  {
    // 直接向 xterm 发送按键（CDP 真实键盘事件）
    await ev(`(() => { const ta = __t.q('.xterm-helper-textarea'); if (ta) ta.focus(); return true; })()`);
    await S('Input.insertText', { text: 'echo e2e-typed' }).catch(() => {});
    await sleep(700);
    const typed = await ev(`(window.__termText ? window.__termText() : '')`);
    record('11 终端输入', '键入内容回显到终端', typed.includes('e2e-typed'), `rows=${(typed || '').slice(0, 60)}`);
    // 片段插入
    await clickText('.seg button', '片段');
    await sleep(400);
    const firstSnip = await ev(`__t.qa('.listitem .lt')[0]?.textContent.trim() || null`);
    await clickSel('.listitem');
    await sleep(700);
    const rows = await ev(`(window.__termText ? window.__termText() : '')`);
    record('13 片段插入', '点击片段写入终端（不自动执行）', !!firstSnip && rows.length > 0, `片段=${firstSnip}`);
    await clickText('.seg button', '主机');
    await sleep(300);
  }

  // ============ 7.5 快捷键（终端聚焦时仍生效） ============
  {
    await ev(`(() => { const t = __t.qa('.tabbar .tab').find(x => x.textContent.includes('web-02')); t && t.click(); return true; })()`);
    await sleep(350);
    await focusXterm();
    const tabs0 = await tabCount();
    // Ctrl+T = 本地终端（不是命令面板）
    await key('t', { ctrlKey: true });
    await sleep(800);
    const local = await ev(`({
      palette: !!__t.q('.palette'), tabs: __t.qa('.tabbar .tab').length,
      title: __t.txt('.tab.on .tn'), tab: __t.qa('.tabbar .tab').find(t => t.classList.contains('on'))?.dataset.status,
    })`);
    record('4 快捷键', 'Ctrl+T 新建本地终端（不弹命令面板）', !local.palette && local.tabs === tabs0 + 1 && local.title === '本地终端' && local.tab === 'connected', JSON.stringify(local));
    record('4 快捷键', '本地终端沿用 app.localTerm 标题与 xterm 挂载', (await ev(`__t.qa('.termhost .xterm').length`)) === 1, '');
    await sleep(400);
    const localRows = await ev(`(window.__termText ? window.__termText() : '')`);
    record('4 快捷键', '本地终端首屏输出未丢失（早到 term://data 回放）', localRows.includes('Ruthenium') || localRows.includes('本地优先'), (localRows || '').slice(0, 60));
    // Ctrl+K 在终端焦点下打开命令面板
    await focusXterm();
    await key('k', { ctrlKey: true });
    await sleep(400);
    record('4 快捷键', '终端聚焦时 Ctrl+K 打开命令面板', await ev(`!!__t.q('.palette')`), '');
    await key('Escape');
    await sleep(250);
    // Ctrl+N 在终端焦点下打开新建主机
    await focusXterm();
    await key('n', { ctrlKey: true });
    await sleep(400);
    record('4 快捷键', '终端聚焦时 Ctrl+N 打开主机表单', await ev(`!!__t.q('.sheet')`), '');
    await closeOverlay();
    // Ctrl+, 在终端焦点下打开设置
    await focusXterm();
    await key(',', { ctrlKey: true });
    await sleep(400);
    record('4 快捷键', '终端聚焦时 Ctrl+, 打开设置', await ev(`!!__t.q('.sheet')`), '');
    await closeOverlay();
    // Ctrl+1..9 跳标签
    await focusXterm();
    await key('1', { ctrlKey: true });
    await sleep(350);
    const one = await activeTab();
    await focusXterm();
    await key('3', { ctrlKey: true });
    await sleep(350);
    const three = await activeTab();
    record('4 快捷键', '终端聚焦时 Ctrl+1..9 跳转会话标签', one === 'web-01' && three === 'web-02', `1→${one} 3→${three}`);
    // Ctrl+Tab / Ctrl+Shift+Tab 循环（首尾相接）
    await focusXterm();
    const cyc0 = await activeTab();
    await key('Tab', { ctrlKey: true });
    await sleep(300);
    const cyc1 = await activeTab();
    await key('Tab', { ctrlKey: true, shiftKey: true });
    await sleep(300);
    const cyc2 = await activeTab();
    record('4 快捷键', 'Ctrl+Tab / Ctrl+Shift+Tab 前后循环', cyc1 !== cyc0 && cyc2 === cyc0, `${cyc0}→${cyc1}→${cyc2}`);
    // 切到最后一个标签再 Ctrl+Tab：回到第一个（首尾相接）
    await ev(`(() => { const ts = __t.qa('.tabbar .tab'); ts[ts.length - 1]?.click(); return true; })()`);
    await sleep(300);
    await focusXterm();
    await key('Tab', { ctrlKey: true });
    await sleep(300);
    const wrap = await activeTab();
    record('4 快捷键', 'Ctrl+Tab 从末尾回到第一个标签', wrap === 'web-01', `→${wrap}`);
    // Ctrl+W 留给 shell（不关标签）；Ctrl+Shift+W 才关标签
    await ev(`(() => { const t = __t.qa('.tabbar .tab').find(x => x.textContent.includes('本地终端')); t && t.click(); return true; })()`);
    await sleep(300);
    await focusXterm();
    const tabsW = await tabCount();
    await key('w', { ctrlKey: true });
    await sleep(350);
    const tabsW2 = await tabCount();
    record('4 快捷键', 'Ctrl+W 不拦截、不关闭标签', tabsW2 === tabsW, `${tabsW}→${tabsW2}`);
    await focusXterm();
    await key('W', { ctrlKey: true, shiftKey: true });
    await sleep(500);
    const tabsW3 = await tabCount();
    const localGone = await ev(`!__t.qa('.tabbar .tab').some(t => t.textContent.includes('本地终端'))`);
    record('4 快捷键', 'Ctrl+Shift+W 关闭当前标签', tabsW3 === tabsW - 1 && localGone, `${tabsW}→${tabsW3}`);
    // 侧栏搜索：Esc 清空；↵ 连接第一个匹配项
    await setVal('.sidesearch input', 'web-01');
    await sleep(350);
    await ev(`__t.q('.sidesearch input').focus()`);
    await key('Enter');
    await sleep(500);
    record('4 快捷键', '侧栏搜索 ↵ 连接第一个匹配主机', (await activeTab()) === 'web-01', await activeTab());
    // 连接切标签会把焦点交给 xterm；Esc 只在搜索框聚焦时生效，重新聚焦后再按
    await ev(`__t.q('.sidesearch input').focus()`);
    await key('Escape');
    await sleep(350);
    const cleared = await ev(`__t.q('.sidesearch input').value === '' && __t.qa('.hostitem').length >= 5`);
    record('4 快捷键', '侧栏搜索 Esc 清空并恢复列表', cleared, '');
  }

  // ============ 9. 断线重连 ============
  {
    await clickTitle('.tabplus', '断线');
    await sleep(700);
    const banner = await ev(`!!__t.q('.banner')`);
    const btn = await ev(`!!__t.byText('.banner button', '重新连接')`);
    record('9 断线重连', '断线内联横幅 + 重新连接', banner && btn, banner ? '' : '无横幅');
    if (banner) {
      await clickText('.banner button', '重新连接');
      await sleep(1400);
      record('9 断线重连', '点击后横幅消失', !(await ev(`!!__t.q('.banner')`)), '');
    }
    await shot('05-banner');
  }

  // ============ 14/15. SFTP 双栏 + 传输 + 原生拖放 ============
  {
    await clickTitle('.tabplus', 'SFTP');
    await sleep(1200);
    const panes = await ev(`__t.qa('.pane').length`);
    const crumbs = await ev(`__t.qa('.crumb').length`);
    record('14 SFTP', '双栏 + 两套面包屑', panes === 2 && crumbs === 2, `panes=${panes} crumbs=${crumbs}`);
    // 左基线对齐：toolbar 标题 / 面包屑行 / 文件行文字 / 队列标题必须在同一条竖线上
    // （10/12/13px 三套内边距会造成两处错位；宽档 12px、窄档 8px 都适用）
    const align = await ev(`(() => {
      const line = (sel) => {
        const e = __t.q(sel);
        if (!e) return null;
        return e.getBoundingClientRect().left + parseFloat(getComputedStyle(e).paddingLeft);
      };
      const base = line('.sftp .toolbar');
      if (base === null) return null;
      const off = (v) => (v === null ? null : Math.round((v - base) * 10) / 10);
      return {
        base: Math.round(base * 10) / 10,
        ph: off(line('.sftp .panehead')),
        fr: off(line('.sftp .frow')),
        q: off(line('.sftp .queue')),
      };
    })()`);
    record('14 SFTP', '左基线对齐（toolbar = 面包屑 = 文件行 = 队列标题）',
      !!align && [align.ph, align.fr, align.q].every((v) => v !== null && Math.abs(v) <= 0.75),
      `base=${align && align.base} panehead=${align && align.ph} frow=${align && align.fr} queue=${align && align.q}`);
    // 进入本地 Windows 目录，得到真实文件行（盘符名与后端一致，形如 "C:"）
    let navOk = true;
    for (const name of ['C:', 'Users', 'Wayne', 'Downloads']) {
      if (!(await dblRow('local', name))) navOk = false;
    }
    const localFiles = await ev(`__t.qa('.pane[data-side="local"] .frow:not(.dir)').map(r => r.dataset.name)`);
    record('14 SFTP', '本地栏按 Windows 路径进入子目录', navOk && localFiles.length > 0, `${localFiles.join(',')}`);
    // 选中文件 → 上传按钮可用 → 进度
    const picked = await ev(`(() => { const row = __t.q('.pane[data-side="local"] .frow:not(.dir)'); if (!row) return false; row.click(); return true; })()`);
    await sleep(350);
    const upEnabled = await ev(`(() => { const b = __t.byText('.toolbar button', '上传'); return b ? !b.disabled : false; })()`);
    record('14 SFTP', '选中文件后「上传」可用', picked && upEnabled, `picked=${picked} enabled=${upEnabled}`);
    if (upEnabled) {
      await clickText('.toolbar button', '上传');
      const seq = [];
      for (let i = 0; i < 24; i++) {
        await sleep(300);
        const p = await ev(`[...__t.qa('.qpct')].map(e => e.textContent.trim())`);
        seq.push(p.join('/'));
        if (seq[seq.length - 1].includes('100')) break;
      }
      const queued = await ev(`__t.qa('.qrow').length`);
      record('15 传输', '队列出现任务并推进进度', queued > 0 && seq.some((x) => /[1-9]/.test(x)), `rows=${queued} seq=${seq.slice(0, 8).join(' ')}`);
      record('15 传输', '进度推进到 100%', seq.some((x) => x.includes('100')), seq.join(' '));
    } else {
      record('15 传输', '上传进度', 'skip', '未选中本地文件');
    }

    // 拖放上传：本地文件 → 对侧远程面板（Pointer 自绘拖拽：CDP 真实鼠标事件；
    // 合成 DragEvent 无效：Tauri dragDropEnabled 屏蔽 HTML5 DnD）
    const fpos = async (sel) => ev(`(() => { const el = __t.q('${sel}'); if (!el) return null;
      el.scrollIntoView({ block: 'center' });
      const r = el.getBoundingClientRect(); return { x: Math.round(r.left + Math.min(120, r.width / 2)), y: Math.round(r.top + r.height / 2) }; })()`);
    const dragFileTo = async (fromSel, toSel) => {
      const from = await fpos(fromSel); const to = await fpos(toSel);
      if (!from || !to) return { missing: true };
      await sleep(140);
      await S('Input.dispatchMouseEvent', { type: 'mousePressed', x: from.x, y: from.y, button: 'left', buttons: 1, clickCount: 1 });
      await sleep(50);
      for (let i = 1; i <= 8; i++) {
        await S('Input.dispatchMouseEvent', { type: 'mouseMoved', x: Math.round(from.x + (to.x - from.x) * i / 8), y: Math.round(from.y + (to.y - from.y) * i / 8), button: 'left', buttons: 1 });
        await sleep(30);
      }
      await sleep(110);
      const mid = await ev(`({
        dragging: __t.q('.frow.dragging')?.dataset.name || null,
        zone: !!__t.q('.dropzone.on'), tip: __t.txt('.droptip') || '',
        dirOver: __t.q('.frow.dir-over')?.dataset.name || null,
      })`);
      await S('Input.dispatchMouseEvent', { type: 'mouseReleased', x: to.x, y: to.y, button: 'left', buttons: 0, clickCount: 1 });
      await sleep(500);
      return { mid, to };
    };

    const q0 = await ev(`__t.qa('.qrow').length`);
    const r = await dragFileTo('.pane[data-side="local"] .frow:not(.dir)', '.pane[data-side="remote"]');
    record('15 拖放', '按住文件行拖到对侧：dragging 态 + 上传落区 + 进入队列',
      !!r.mid && r.mid.dragging && r.mid.zone && r.mid.tip.includes('上传'),
      JSON.stringify({ mid: r.mid, q0 }));
    await sleep(600);
    const q1 = await ev(`__t.qa('.qrow').length`);
    record('15 拖放', '松开后上传进入现有传输队列', q1 > q0, `${q0}→${q1}`);

    // 拖放到目录行：落到该子目录（实线落区）
    const q2 = await ev(`__t.qa('.qrow').length`);
    const r2 = await dragFileTo('.pane[data-side="local"] .frow:not(.dir)', '.pane[data-side="remote"] .frow.dir');
    record('15 拖放', '拖到对侧目录行高亮该目录（row 落区）',
      !!r2.mid && r2.mid.zone === true && !!r2.mid.dirOver,
      JSON.stringify(r2.mid));
    await sleep(600);
    const q3 = await ev(`__t.qa('.qrow').length`);
    record('15 拖放', '拖到目录行按目标子目录进入队列', q3 > q2, `${q2}→${q3}`);

    // 下载方向：远程文件 → 本地面板
    await dblRow('remote', 'current');
    const q4 = await ev(`__t.qa('.qrow').length`);
    const downName = await ev(`__t.q('.pane[data-side="remote"] .frow:not(.dir)')?.dataset.name || null`);
    const r3 = await dragFileTo('.pane[data-side="remote"] .frow:not(.dir)', '.pane[data-side="local"]');
    record('15 拖放', '反向拖入本地面板提示下载',
      !!r3.mid && r3.mid.zone && r3.mid.tip.includes('下载'), JSON.stringify(r3.mid));
    await sleep(600);
    const q5 = await ev(`__t.qa('.qrow').length`);
    const downRow = await ev(`[...__t.qa('.qrow')].slice(-1).map(r => r.textContent.replace(/\\s+/g,' ').trim())[0] || ''`);
    record('15 拖放', '下载进入队列并标记方向', q5 > q4 && downRow.includes(downName || 'package.json') && downRow.includes('下载'), `${q4}→${q5} ${downRow}`);
    await shot('06-sftp');
  }

  // ============ 16/17. 转发规则启停 ============
  {
    // 状态栏显示「运行态」监听数（tunnel://state）。先连接 db-primary，
    // r1（host=h3，on=true）会随会话拉起 → 状态栏出现「转发 1」。
    await clickText('.seg button', '主机');
    await sleep(300);
    await ev(`(() => { const el = __t.byText('.hostitem', 'db-primary'); el && el.dispatchEvent(new MouseEvent('dblclick', { bubbles: true })); return true; })()`);
    await waitUntil(`__t.qa('.tabbar .tab').some(t => t.textContent.includes('db-primary') && t.dataset.status === 'connected')`, 6000);
    await clickText('.seg button', '转发');
    await sleep(500);
    const before = await ev(`__t.qa('.listitem .dot.on').length`);
    const statusBefore = await ev(`__t.txt('.statusbar')`);
    record('17 转发启停', '会话连接后已启用规则进入「监听中」并显示在状态栏',
      before >= 1 && (statusBefore || '').includes('转发'), `dots=${before} status="${(statusBefore || '').slice(0, 80)}"`);
    await clickSel('.listitem');
    await sleep(600);
    const after = await ev(`__t.qa('.listitem .dot.on').length`);
    const statusAfter = await ev(`__t.txt('.statusbar')`);
    record('17 转发启停', '点击规则切换启用状态', before !== after, `${before}→${after}`);
    record('17 转发启停', '状态栏转发计数同步（关闭后不再显示监听数）',
      statusBefore !== statusAfter && !(statusAfter || '').includes('转发'),
      `before="${(statusBefore || '').slice(0, 60)}" after="${(statusAfter || '').slice(0, 60)}"`);
    // 还原：把规则重新打开，供后续用例使用
    await clickSel('.listitem');
    await sleep(400);
    await clickText('.seg button', '主机');
    await sleep(300);
  }

  // ============ 18. 命令面板 ============
  {
    await key('k', { ctrlKey: true });
    const open = await waitUntil(`!!__t.q('.palette')`);
    await setVal('.palette input', 'web');
    await sleep(500);
    const rows = await ev(`__t.qa('.palrow').length`);
    await key('ArrowDown');
    await sleep(200);
    const sel = await ev(`__t.qa('.palrow.on').length`);
    await key('Escape');
    await sleep(300);
    record('18 命令面板', 'Ctrl+K 打开并模糊匹配', open && rows > 0, `rows=${rows}`);
    record('18 命令面板', '↑↓ 高亮 + Esc 关闭', sel === 1 && (await overlayClosed()), `sel=${sel}`);
  }

  // ============ 19/20. 设置 + 主题 ============
  {
    // 先切回终端标签：稍后断言关闭图形加速后终端回退 DOM 渲染
    await ev(`(() => { const t = __t.qa('.tabbar .tab').find(x => x.textContent.includes('web-01')); t && t.click(); return true; })()`);
    await waitUntil(`!!__t.q('.termhost .xterm')`, 3000);
    await sleep(250);
    await key(',', { ctrlKey: true });
    await waitUntil(`!!__t.q('.sheet')`);
    // 关于页版本号必须与 tauri.conf.json 一致（版本的唯一权威源）；界面不得
    // 硬编码版本号——构建期注入 __APP_VERSION__，这条断言守住这一点
    const confVersion = JSON.parse(fs.readFileSync(new URL('../src-tauri/tauri.conf.json', import.meta.url), 'utf8')).version;
    const aboutVer = await ev(`__t.txt('.sheet .bt')`);
    record('19 设置', '关于版本号与 tauri.conf.json 一致',
      typeof aboutVer === 'string' && aboutVer.includes(confVersion),
      `sheet='${aboutVer}' conf=${confVersion}`);
    const s = await ev(`({
      labels: __t.qa('.sheet .field label').map(e => e.textContent.trim()),
      rows: __t.qa('.sheet .setrow .srt').map(e => e.textContent.trim()),
      subtitles: __t.qa('.sheet .sheethead p').length,
      smallText: __t.qa('.sheet .srs, .sheet .hint, .sheet .sidehint').filter(e => (e.closest('.setrow')?.dataset.setting || '') !== 'gpu').length,
      pills: __t.qa('.sheet .pill').length,
      switches: __t.qa('.sheet .switch').length,
      ranges: __t.qa('.sheet input[type=range]').length,
      foot: __t.txt('.sheet .sheetfoot'),
    })`);
    record('19 设置', '设置项齐全（语言/主题/字号/行高/字体/光标/行号/导航 + 开关）', s.ranges >= 2 && s.switches >= 4 && s.labels.length >= 6, `labels=${s.labels.length} ranges=${s.ranges} sw=${s.switches}`);
    // 启动门禁（Windows Hello，默认关闭）：开关存在、默认关，可开启并落入偏好
    const helloSwitch = () => ev(`(() => {
      const row = __t.q('.sheet .setrow[data-setting="requireWindowsHello"]');
      const sw = row && row.querySelector('.switch');
      return { row: !!row, on: sw ? sw.getAttribute('aria-checked') : null, text: (row && row.textContent || '').trim() };
    })()`);
    const helloOff = await helloSwitch();
    record('19 设置', 'Windows Hello 启动验证开关存在且默认关闭',
      helloOff.row && helloOff.on === 'false', JSON.stringify(helloOff));
    await ev(`(() => { const r = __t.q('.sheet .setrow[data-setting="requireWindowsHello"] .switch'); r && r.click(); return true; })()`);
    await sleep(300);
    const helloOn = await helloSwitch();
    record('19 设置', 'Windows Hello 启动验证开关可开启',
      helloOn.on === 'true', `aria-checked=${helloOn.on}`);
    // 局域网对等层（默认关）：开关存在且默认关，开启后状态行显示监听端口，关闭后回到未监听
    const lanSwitch = () => ev(`(() => {
      const row = __t.q('.sheet .setrow[data-setting="lanVisible"]');
      const sw = row && row.querySelector('.switch');
      return { row: !!row, on: sw ? sw.getAttribute('aria-checked') : null, status: (__t.q('.sheet [data-lan-status]')?.textContent || '').trim() };
    })()`);
    const lanOff = await lanSwitch();
    record('19 设置', '局域网可见开关存在且默认关闭', lanOff.row && lanOff.on === 'false', JSON.stringify(lanOff));
    await ev(`(() => { const r = __t.q('.sheet .setrow[data-setting="lanVisible"] .switch'); r && r.click(); return true; })()`);
    await sleep(700);
    const lanOn = await lanSwitch();
    record('19 设置', '局域网可见开启后状态显示监听端口', lanOn.on === 'true' && lanOn.status.includes(':27410'), `on=${lanOn.on} status=${lanOn.status}`);
    await ev(`(() => { const r = __t.q('.sheet .setrow[data-setting="lanVisible"] .switch'); r && r.click(); return true; })()`);
    await sleep(700);
    const lanBack = await lanSwitch();
    record('19 设置', '局域网可见关闭后回到未监听', lanBack.on === 'false' && lanBack.status.length > 0, `on=${lanBack.on} status=${lanBack.status}`);
    // 共享目录：原生目录选择器（mock 返回固定路径），并提供清除
    const shareInput = () => ev(`__t.q('[data-lan-share-root]')?.value ?? null`);
    await ev(`(() => { const b = __t.q('[data-lan-share-browse]')?.closest('button'); b && b.click(); return !!b; })()`);
    await sleep(400);
    const sharePicked = await shareInput();
    record('19 设置', '共享目录可调用系统目录选择器', String(sharePicked || '').includes('RutheniumShared'), `path=${sharePicked}`);
    await ev(`(() => { const b = [...__t.qa('.pathpick .tbtn')].find((x) => x.textContent.includes('清除')); b && b.click(); return !!b; })()`);
    await sleep(300);
    const shareCleared = await shareInput();
    record('19 设置', '共享目录可清除（空 = 全盘）', shareCleared === '', `path=${shareCleared}`);
        // 侧栏宽度已移出设置（拖拽是手势而不是设置项）
    record('19 设置', '已移除「侧栏宽度」控件', s.ranges === 2 && !(s.rows || []).some((x) => x.includes('宽度')), `ranges=${s.ranges} rows=${(s.rows || []).join(',')}`);
    record('19 设置', '包含「显示行号」开关', (s.rows || []).some((x) => x.includes('行号')), (s.rows || []).join(','));
    // 行高滑杆：存在、范围 1.0–2.0（默认 1.3）；拖动后标签数值同步（真实行距变化由真实应用探针验证）
    const lh = await ev(`(async () => {
      const slider = __t.qa('.sheet input[type=range]').find((i) => Number(i.getAttribute('max')) === 2);
      if (!slider) return { found: false };
      const labelOf = () => [...__t.qa('.sheet .field label')].map((e) => e.textContent.trim()).find((t) => t.includes('行高')) || '';
      const before = labelOf();
      const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set;
      setter.call(slider, '2');
      slider.dispatchEvent(new Event('input', { bubbles: true }));
      await new Promise((r) => setTimeout(r, 500));
      const after = labelOf();
      setter.call(slider, '1.3');
      slider.dispatchEvent(new Event('input', { bubbles: true }));
      await new Promise((r) => setTimeout(r, 400));
      return { found: true, min: slider.min, max: slider.max, before, after };
    })()`);
    record('19 设置', '行高滑杆存在（1.0–2.0）且数值随拖动更新',
      lh.found && lh.min === '1' && lh.max === '2'
        && lh.before.includes('1.30') && lh.after.includes('2.00'),
      JSON.stringify(lh));
    // 行号语义：只给「有内容的行」编号，
    // 空行与正在输入的提示符行不编号——否则清屏后会把下方空白号成 3、4、5…，看起来像整体串位。
    {
      const toggled = await ev(`(async () => {
        const row = [...document.querySelectorAll('.sheet .setrow')].find((r) => (r.textContent || '').includes('行号'));
        if (!row) return false;
        const sw = row.querySelector('.switch');
        const on = sw && sw.getAttribute('aria-checked') === 'true';
        if (!on) sw.click();
        await new Promise((r) => setTimeout(r, 500));
        return true;
      })()`);
      const gutter = await ev(`(() => {
        const items = [...document.querySelectorAll('.termgutter .tln')].map((l) => ({
          n: Number(l.textContent), top: +l.getBoundingClientRect().top.toFixed(2), h: +l.getBoundingClientRect().height.toFixed(2),
        })).filter((x) => Number.isFinite(x.n));
        // 1) 编号必须连续（空行也占号，不能跳号）
        let consecutive = true;
        for (let i = 1; i < items.length; i++) if (items[i].n !== items[i - 1].n + 1) consecutive = false;
        // 2) 行号落点必须严格跟随单元格高度（改行高后不能漂移）
        const cellH = items[0] ? items[0].h : 0;
        let aligned = true;
        for (let i = 1; i < items.length; i++) {
          const want = items[0].top + (items[i].n - items[0].n) * cellH;
          if (Math.abs(items[i].top - want) > 1) aligned = false;
        }
        return { count: items.length, first: items[0] && items[0].n, last: items[items.length - 1] && items[items.length - 1].n, consecutive, aligned, cellH };
      })()`);
      record('20 行号', '空行占号且编号连续、落点跟随单元格高度',
        toggled && gutter.count > 0 && gutter.consecutive && gutter.aligned,
        JSON.stringify(gutter));
      // 还原开关，避免影响后续用例
      await ev(`(async () => {
        const row = [...document.querySelectorAll('.sheet .setrow')].find((r) => (r.textContent || '').includes('行号'));
        const sw = row && row.querySelector('.switch');
        if (sw && sw.getAttribute('aria-checked') === 'true') sw.click();
        await new Promise((r) => setTimeout(r, 300));
        return true;
      })()`);
    }
    record('19 设置', '无副标题；仅允许 1 条语言说明', s.subtitles === 0 && s.smallText <= 1, `sub=${s.subtitles} hint=${s.smallText}`);
    record('19 设置', '页脚仅「完成」', (s.foot || '').trim() === '完成', s.foot || '');

    // 图形加速：单个开关（开=system / 关=off），不是三态 pill
    const gpu = await ev(`(() => {
      const row = __t.q('.sheet .setrow[data-setting="gpu"]');
      if (!row) return null;
      const sw = row.querySelector('.switch');
      return {
        text: (row.querySelector('.srt')?.textContent || '').trim(),
        sub: (row.querySelector('.srs')?.textContent || '').trim(),
        sw: !!sw,
        checked: sw ? sw.getAttribute('aria-checked') : null,
        disabled: sw ? sw.getAttribute('aria-disabled') : null,
        act: sw ? sw.getAttribute('data-act') : null,
        switchCount: row.querySelectorAll('.switch').length,
        pills: row.querySelectorAll('.pill').length,
      };
    })()`);
    record('19 设置', '包含「使用图形加速」单开关（非三态 pill）',
      !!gpu && gpu.text.includes('图形加速') && gpu.sw && gpu.switchCount === 1 && gpu.pills === 0, JSON.stringify(gpu));
    if (gpu && gpu.disabled !== 'true') {
      await ev(`(() => { const row = __t.q('.sheet .setrow[data-setting="gpu"]'); row?.querySelector('.switch')?.click(); return true; })()`);
      await sleep(700);
      const gpuOff = await ev(`(() => {
        const row = __t.q('.sheet .setrow[data-setting="gpu"]');
        const sw = row?.querySelector('.switch');
        return {
          checked: sw?.getAttribute('aria-checked'),
          sub: (row?.querySelector('.srs')?.textContent || '').trim(),
          canvases: __t.qa('.termhost canvas').length,
          rows: (__t.q('.termhost .xterm-rows')?.innerText || '').trim().slice(0, 90),
        };
      })()`);
      record('19 图形加速', '关闭开关 → gpuMode=off（设置界面状态）且提示重启生效',
        gpuOff.checked === 'false' && (gpuOff.sub || '').includes('重启后完全生效'), JSON.stringify(gpuOff));
      record('19 图形加速', '关闭后终端回退 DOM 渲染（无 WebGL canvas，.xterm-rows 有文本）',
        gpuOff.canvases === 0 && (gpuOff.rows || '').length > 0, `canvas=${gpuOff.canvases} rows=${(gpuOff.rows || '').slice(0, 40)}`);
      // 还原为 system：已打开终端不重建（重启后统一生效），新建终端才重新启用 WebGL
      await ev(`(() => { const row = __t.q('.sheet .setrow[data-setting="gpu"]'); row?.querySelector('.switch')?.click(); return true; })()`);
      await sleep(600);
      const gpuOn = await ev(`(() => {
        const row = __t.q('.sheet .setrow[data-setting="gpu"]');
        const sw = row?.querySelector('.switch');
        return { checked: sw?.getAttribute('aria-checked'), sub: (row?.querySelector('.srs')?.textContent || '').trim() };
      })()`);
      record('19 图形加速', '重新开启 → 恢复 system 且提示消失', gpuOn.checked === 'true' && gpuOn.sub === '', JSON.stringify(gpuOn));
    } else {
      record('19 设置', 'WebGL 不可用：开关禁用、aria-disabled 且不挂 data-act',
        !!gpu && gpu.disabled === 'true' && gpu.act === null && (gpu.sub || '').length > 0, JSON.stringify(gpu));
      record('19 图形加速', 'WebGL 不可用：跳过 DOM 回退断言', 'skip', '');
    }
    await shot('07-settings');

    // 字体切换 → 终端字体变量变化
    const fontChanged = await ev(`(() => {
      const before = getComputedStyle(document.documentElement).getPropertyValue('--termfont').trim();
      const b = __t.byText('.sheet .pill', 'IBM Plex Mono'); if (!b) return { ok: false, before };
      b.click();
      return { ok: true, before };
    })()`);
    await sleep(500);
    const fontAfter = await ev(`getComputedStyle(document.documentElement).getPropertyValue('--termfont').trim()`);
    record('19 设置', '终端字体切换即时生效', fontChanged.ok && fontAfter.includes('Rut Mono Plex'), `${fontChanged.before} → ${fontAfter}`);

    // 主题
    const darkBefore = await ev(`document.body.classList.contains('dark')`);
    await clickText('.sheet .pill', '深色');
    await sleep(500);
    const darkAfter = await ev(`document.body.classList.contains('dark')`);
    record('20 主题', '深色主题切换生效', darkAfter !== darkBefore, `${darkBefore}→${darkAfter}`);
    await shot('08-settings-dark');
    await clickText('.sheet .pill', '浅色');
    await sleep(400);
  }

  // ============ 24. Termius X 导航 ============
  {
    await clickText('.sheet .pill', '水平标签');
    await sleep(900);
    const x = await ev(`({ dataTabs: __t.qa('.tabbar .tab.data').length, sideHidden: !__t.q('.sidebar') || getComputedStyle(__t.q('.sidebar')).display === 'none' })`);
    record('24 Termius X', '水平数据标签 + 侧栏收起', x.dataTabs >= 4 && x.sideHidden, `dataTabs=${x.dataTabs} hidden=${x.sideHidden}`);
    await shot('09-xnav');
    // 主机库「凭据」列：只显示文件名，完整路径进 title，列宽不撑开
    await clickText('.tabbar .tab.data', '主机库');
    await sleep(600);
    const creds = await ev(`__t.qa('.dptable td.cred').map(td => ({
      text: td.textContent.trim(), title: td.title,
      width: td.getBoundingClientRect().width,
      ws: getComputedStyle(td).whiteSpace, overflow: getComputedStyle(td).overflow,
      ellipsis: getComputedStyle(td).textOverflow, max: getComputedStyle(td).maxWidth,
    }))`);
    const credOk = creds.length >= 5 && creds.every((c) => c.text === 'id_ed25519' || c.text === '—')
      && creds.every((c) => !c.text.includes('\\') && !c.text.includes('/'))
      && creds[0].title.includes('id_ed25519') && creds[0].title.length > creds[0].text.length
      && creds.every((c) => c.width <= 190) && creds[0].max === '160px'
      && creds[0].ws === 'nowrap' && creds[0].overflow === 'hidden' && creds[0].ellipsis === 'ellipsis';
    record('6 凭据列', '只显示文件名、完整路径在 title、列宽不撑开', credOk, JSON.stringify(creds[0] || null));
    // 切回经典
    await key(',', { ctrlKey: true });
    await waitUntil(`!!__t.q('.sheet')`);
    await clickText('.sheet .pill', '经典侧栏');
    await sleep(800);
    const back = await ev(`__t.qa('.tabbar .tab.data').length === 0 && !!__t.q('.sidebar')`);
    record('24 Termius X', '可切回经典侧栏', back, '');
    await closeOverlay();
  }

  // ============ 22. 专注模式 ============
  {
    await key('F', { ctrlKey: true, shiftKey: true });
    await sleep(600);
    const focusOn = await ev(`({
      focusClass: __t.q('.app').classList.contains('focus'),
      titleHidden: getComputedStyle(__t.q('.titlebar')).display === 'none',
      exitBtn: !!__t.q('.focusexit'),
    })`);
    record('22 专注模式', 'Ctrl+Shift+F 隐藏 chrome 并保留退出按钮', focusOn.focusClass && focusOn.titleHidden && focusOn.exitBtn, JSON.stringify(focusOn));
    await shot('10-focus');
    if (focusOn.exitBtn) await clickSel('.focusexit');
    await sleep(500);
    record('22 专注模式', '退出后 chrome 恢复', await ev(`getComputedStyle(__t.q('.titlebar')).display !== 'none'`), '');
  }

  // ============ 23. 侧栏折叠 + 拖拽 ============
  {
    const w0 = await ev(`__t.rect('.sidebar').w`);
    await key('b', { ctrlKey: true });
    await sleep(500);
    const collapsed = await ev(`!__t.q('.sidebar') || __t.rect('.sidebar').w === 0`);
    await key('b', { ctrlKey: true });
    await sleep(500);
    const w1 = await ev(`__t.rect('.sidebar').w`);
    record('23 侧栏', 'Ctrl+B 折叠/展开', collapsed && w1 === w0, `${w0}→${collapsed?'0':'?'}→${w1}`);

    // 拖拽 splitter（真实鼠标事件）
    const sp = await ev(`__t.rect('.splitter')`);
    if (sp) {
      const startX = sp.x + 1, y = 400;
      await S('Input.dispatchMouseEvent', { type: 'mousePressed', x: startX, y, button: 'left', clickCount: 1 });
      for (const dx of [40, 80, 120]) {
        await S('Input.dispatchMouseEvent', { type: 'mouseMoved', x: startX + dx, y, button: 'left' });
        await sleep(80);
      }
      await S('Input.dispatchMouseEvent', { type: 'mouseReleased', x: startX + 120, y, button: 'left' });
      await sleep(400);
      const w2 = await ev(`__t.rect('.sidebar').w`);
      record('23 侧栏', '拖拽分隔条改变宽度（210–360）', w2 > w1 && w2 <= 360, `${w1}→${w2}`);
    } else {
      record('23 侧栏', '拖拽分隔条', 'skip', 'splitter 不可见');
    }
  }

  // ============ 25/26. 状态栏 + 连接信息 ============
  {
    // 先切到一个已连接的终端标签
    await ev(`(() => { const t = __t.qa('.tabbar .tab').find(x => !x.classList.contains('data') && x.dataset.status === 'connected'); if (t) t.click(); return true; })()`);
    await sleep(500);
    const st = await ev(`({
      text: __t.txt('.statusbar'),
      enc: !!__t.q('.encbtn'),
      info: __t.qa('.statusbar button').some(b => b.textContent.includes('连接信息')),
      titles: __t.qa('.statusbar button').map(b => b.getAttribute('title')),
      mono: __t.qa('.statusbar .mono').length,
      lastTwo: [...__t.qa('.statusbar > *')].slice(-2).map(e => ({ cls: e.className, txt: e.textContent.trim() })),
    })`);
    const trimOk = !(st.text || '').includes('hosts.toml') && !(st.text || '').includes('%APPDATA%');
    const tailOk = st.lastTwo.length === 2 && String(st.lastTwo[1].txt).includes('连接信息') && String(st.lastTwo[0].txt).includes('UTF-8');
    record('25 状态栏', '已去除常驻配置路径', trimOk && st.mono === 0, (st.text || '').slice(0, 80));
    record('25 状态栏', '末尾只保留编码 chip 与连接信息入口', tailOk, JSON.stringify(st.lastTwo));
    record('25 状态栏', '编码与连接信息入口可用', st.enc && st.info, `enc=${st.enc} info=${st.info}`);
    const opened = await ev(`(() => { const b = [...__t.qa('.statusbar button')].find(x => x.textContent.includes('连接信息')); if (!b) return false; b.click(); return true; })()`);
    await sleep(2600);
    const ci = await ev(`({ title: __t.txt('.sheet h3'), cards: __t.qa('.mcard').length, bars: __t.qa('.mbar').length, pause: !!__t.byText('.sheet button', '暂停'), refresh: !!__t.byText('.sheet button', '刷新') })`);
    record('26 连接信息', '打开面板并显示指标卡', opened && ci.cards >= 3, `cards=${ci.cards} bars=${ci.bars}`);
    record('26 连接信息', '暂停/刷新控件存在', ci.pause && ci.refresh, `pause=${ci.pause} refresh=${ci.refresh}`);
    if (ci.pause) {
      await clickText('.sheet button', '暂停');
      await sleep(500);
      const resumed = await ev(`!!__t.byText('.sheet button', '继续')`);
      record('26 连接信息', '暂停后可继续', resumed, '');
      if (resumed) await clickText('.sheet button', '继续');
      await sleep(400);
    }
    await shot('11-conninfo');
    await closeOverlay();
  }

  // ============ 28. 编码选择器 ============
  {
    // 先切到一个终端会话：编码提示行写进终端缓冲区
    await ev(`(() => { const t = __t.qa('.tabbar .tab').find(x => !x.classList.contains('data')); if (t) t.click(); return true; })()`);
    await sleep(700);
    const chipText = await ev(`__t.txt('.encbtn')`);
    await clickSel('.encbtn');
    await waitUntil(`!!__t.q('.encpop')`);
    const picker = await ev(`({ groups: __t.qa('.encgroup').length, items: __t.qa('.enclist [role=option], .enclist button, .enclist div[class*=erow]').length })`);
    record('28 编码', '状态栏编码 chip 打开选择器', !!chipText && (await ev(`!!__t.q('.encpop')`)), `chip=${chipText}`);
    record('28 编码', '分组列表存在', picker.groups >= 4, `groups=${picker.groups}`);
    const picked = await ev(`(() => { const el = __t.byText('.encrow', 'GB18030'); if (!el) return false; el.click(); return true; })()`);
    await sleep(800);
    const encDiag = await ev(`({ open: !!__t.q('.encpop'), chip: __t.txt('.encbtn'), rows: __t.qa('.encrow').length, first: __t.qa('.encrow .ec').slice(0,3).map(e=>e.textContent) })`);
    const chipAfter = encDiag.chip;
    record('28 编码', '切换为 GB18030 并回写 chip', picked && (chipAfter || '').includes('GB18030'), `picked=${picked} open=${encDiag.open} rows=${encDiag.rows} ${chipText} → ${chipAfter}`);
    const hint = await ev(`(window.__termText ? window.__termText() : '')`);
    record('28 编码', '终端插入本地编码提示行', hint.includes('[编码]') || hint.includes('GB18030'), '');
    await shot('12-encoding');
  }

  // ============ 12/21. 片段/转发 数据页 + 备份 ============
  {
    // 备份 Sheet：导出 / 导入（不含「重置演示数据」）
    await clickTitle('.sidefoot button', '备份');
    await waitUntil(`!!__t.q('.sheet')`);
    const bk = await ev(`({ title: __t.txt('.sheet h3'), has: !!__t.byText('.sheet button', '导出'), imp: !!__t.byText('.sheet button', '选择备份文件'), reset: !!__t.byText('.sheet button', '重置') })`);
    record('21 备份', '备份 Sheet 仅导出 / 导入两入口（无重置）', bk.has && bk.imp && !bk.reset, bk.title || '');
    if (bk.has) {
      await clickText('.sheet button', '导出');
      await sleep(700);
      const toast = await ev(`__t.txt('.toast')`);
      record('21 备份', '导出触发提示', !!toast, toast || '');
    }
    await shot('13-backup');
    await closeOverlay();
  }

  // ============ 6. 导入 ~/.ssh/config ============
  {
    await clickTitle('.sidefoot button', '导入');
    await waitUntil(`!!__t.q('.sheet')`);
    const ta = await ev(`!!__t.q('.sheet textarea')`);
    if (ta) {
      await ev(`(() => { const t = __t.q('.sheet textarea'); __t.setValue(t, 'Host e2e-imported\\n  HostName 203.0.113.77\\n  User ops\\n  Port 2222\\n'); return true; })()`);
      await sleep(300);
      await clickText('.sheetfoot button', '解析');
      await sleep(600);
      const rows = await ev(`__t.qa('.checkrow').length`);
      record('6 导入', '解析出主机预览行', rows > 0, `rows=${rows}`);
      if (rows > 0) {
        // 确保该行处于选中态（默认可能已选中，盲目点击会取消选择）
        await ev(`(() => { const r = __t.q('.checkrow'); if (r && !r.classList.contains('on')) r.click(); return true; })()`);
        await sleep(300);
        const selected = await ev(`__t.qa('.checkrow.on').length`);
        await clickText('.sheetfoot button', '导入');
        await sleep(900);
        const added = await ev(`__t.qa('.hostitem .hname').map(e => e.textContent.trim())`);
        record('6 导入', '导入后进入主机列表', (added || []).some((n) => n.includes('e2e-imported')), `selected=${selected} hosts=${(added || []).length}`);
      }
      await shot('14-import');
    } else {
      record('6 导入', '导入向导', false, '未找到文本框');
    }
    await closeOverlay();
  }

  // ============ 27. 国际化 ============
  {
    await key(',', { ctrlKey: true });
    await waitUntil(`!!__t.q('.sheet')`);
    const zh = await ev(`({ lang: document.documentElement.lang, title: __t.txt('.sheet h3'), label: __t.qa('.sheet .field label')[0]?.textContent.trim() })`);
    await clickText('.sheet .pill', 'English');
    await sleep(700);
    const en = await ev(`({ lang: document.documentElement.lang, title: __t.txt('.sheet h3'), label: __t.qa('.sheet .field label')[0]?.textContent.trim() })`);
    record('27 i18n', '切换到 English 生效', zh.lang !== en.lang && en.lang === 'en' && en.title !== zh.title, `${zh.lang}/${zh.title} → ${en.lang}/${en.title}`);
    await shot('15-i18n-en');
    await clickText('.sheet .pill', '日本語');
    await sleep(700);
    const ja = await ev(`({ lang: document.documentElement.lang, title: __t.txt('.sheet h3') })`);
    record('27 i18n', '切换到 日本語 生效', ja.lang === 'ja', `${ja.lang}/${ja.title}`);
    await shot('16-i18n-ja');
    await clickText('.sheet .pill', '简体中文');
    await sleep(600);
    record('27 i18n', '切回简体中文', (await ev(`document.documentElement.lang`)) === 'zh-CN', '');
    await closeOverlay();
  }

  // ============ 29. 认证方式（表单三选一 / 密码状态行 / 清除暂存） ============
  {
    await clickText('.seg button', '主机');
    await sleep(300);
    await key('n', { ctrlKey: true });
    await waitUntil(`!!__t.q('.sheet')`);
    const auth0 = await ev(`({
      pills: __t.qa('#authField .pill').map(p => ({ t: p.textContent.trim(), on: p.classList.contains('on') })),
      keyRow: !!__t.q('.sheet .pickrow input'),
      pwRow: !!__t.q('.sheet input[type=password]'),
      label: __t.txt('#authField label'),
    })`);
    record('29 认证', '表单有认证三选一且默认私钥（显示私钥路径字段）',
      auth0.pills.length === 3 && auth0.pills[0].on && auth0.keyRow && !auth0.pwRow && auth0.label === '认证方式',
      JSON.stringify(auth0));

    // 选密码：掩码输入 + 状态行「未保存密码」；未保存时不出现「清除」
    await ev(`(() => { const b = __t.qa('#authField .pill')[1]; b && b.click(); return true; })()`);
    await sleep(250);
    const pwForm = await ev(`({
      pw: !!__t.q('.sheet input[type=password]'),
      type: __t.q('.sheet input[type=password]')?.type,
      keyRow: !!__t.q('.sheet .pickrow input'),
      row: __t.txt('.authrow'),
      clear: !!__t.byText('.authrow .tbtn', '清除'),
      dot: __t.q('.authrow .dot')?.className,
      ph: __t.q('.sheet input[type=password]')?.placeholder,
    })`);
    record('29 认证', '选密码显示掩码输入 + 「未保存密码」状态行，且不显示私钥字段',
      pwForm.pw && pwForm.type === 'password' && !pwForm.keyRow && (pwForm.row || '').includes('未保存密码') && !pwForm.clear,
      JSON.stringify(pwForm));

    // 选 Agent：密钥与密码字段都不显示
    await ev(`(() => { const b = __t.qa('#authField .pill')[2]; b && b.click(); return true; })()`);
    await sleep(250);
    const agentForm = await ev(`({
      pw: !!__t.q('.sheet input[type=password]'), keyRow: !!__t.q('.sheet .pickrow input'),
      on: __t.qa('#authField .pill')[2]?.classList.contains('on'),
    })`);
    record('29 认证', '选 SSH Agent 不显示私钥与密码字段', agentForm.on && !agentForm.pw && !agentForm.keyRow, JSON.stringify(agentForm));

    // 切回密码，填一台新主机并保存（先落盘生成 id，再写 keyring）
    await ev(`(() => { const b = __t.qa('#authField .pill')[1]; b && b.click(); return true; })()`);
    await sleep(200);
    await ev(`(() => {
      const ins = __t.qa('.sheet .grid2 .inp');
      __t.setValue(ins[0], 'e2e-pw'); __t.setValue(ins[1], '10.9.9.9');
      return true;
    })()`);
    await sleep(150);
    await ev(`(() => { const i = __t.q('.sheet input[type=password]'); __t.setValue(i, 'form-secret'); return true; })()`);
    await sleep(150);
    const callsBefore = await mockCalls();
    await clickText('.sheetfoot button', '保存');
    await sleep(700);
    const callsAfter = await mockCalls();
    const savedHost = await ev(`__t.qa('.hostitem .hname').some(e => e.textContent.trim() === 'e2e-pw')`);
    record('29 认证', '密码认证主机保存后写 keyring（set_password 被调用）',
      savedHost && (callsAfter.set_password || 0) > (callsBefore.set_password || 0),
      `host=${savedHost} set_password=${callsAfter.set_password || 0}`);

    // 重新打开编辑：显示「已保存密码」+ 清除按钮；不回显密码
    await rightClickText('.hostitem', 'e2e-pw');
    await waitUntil(`!!__t.q('.menu')`);
    await clickText('.menu .menuitem', '编辑配置');
    await waitUntil(`!!__t.q('.sheet')`);
    await sleep(400);
    const edit = await ev(`({
      row: __t.txt('.authrow'), clear: !!__t.byText('.authrow .tbtn', '清除'),
      value: __t.q('.sheet input[type=password]')?.value || '',
      ph: __t.q('.sheet input[type=password]')?.placeholder || '',
    })`);
    record('29 认证', '编辑时显示「已保存密码」且不回显密码，占位「已保存，留空则不修改」',
      (edit.row || '').includes('已保存密码') && edit.clear && edit.value === '' && (edit.ph || '').includes('留空'),
      JSON.stringify(edit));

    // 「保存密码」按钮：空输入禁用；输入新值后立即写 keyring、状态切已保存、清空输入
    const emptySave = await ev(`({
      disabled: __t.q('.sheet .pwsave button')?.disabled,
      text: __t.txt('.sheet .pwsave button'),
    })`);
    record('29 认证', '密码输入右下角提供「保存密码」按钮，空输入时禁用',
      !!emptySave && emptySave.disabled === true && (emptySave.text || '').includes('保存密码'),
      JSON.stringify(emptySave));
    const setBeforeBtn = (await mockCalls()).set_password || 0;
    await ev(`(() => { __t.setValue(__t.q('.sheet input[type=password]'), 'btn-secret'); return true; })()`);
    await sleep(150);
    const btnEnabled = await ev(`!__t.q('.sheet .pwsave button')?.disabled`);
    await ev(`(() => { __t.q('.sheet .pwsave button')?.click(); return true; })()`);
    await sleep(600);
    const afterBtn = await ev(`({
      row: __t.txt('.authrow'), clear: !!__t.byText('.authrow .tbtn', '清除'),
      value: __t.q('.sheet input[type=password]')?.value || '',
      disabled: __t.q('.sheet .pwsave button')?.disabled,
    })`);
    const setAfterBtn = (await mockCalls()).set_password || 0;
    record('29 认证', '点「保存密码」立即写 keyring，状态变已保存密码且清空输入',
      btnEnabled && (afterBtn.row || '').includes('已保存密码') && afterBtn.clear
        && afterBtn.value === '' && afterBtn.disabled === true && setAfterBtn === setBeforeBtn + 1,
      JSON.stringify(afterBtn) + ` set_password=${setBeforeBtn}→${setAfterBtn}`);

    // 「清除」只在表单内暂存：状态变未保存、输入框清空；点取消不得删除任何凭据
    const delBefore = (await mockCalls()).delete_password || 0;
    await ev(`(() => { const b = __t.byText('.authrow .tbtn', '清除'); b && b.click(); return true; })()`);
    await sleep(300);
    const cleared = await ev(`({ row: __t.txt('.authrow'), clear: !!__t.byText('.authrow .tbtn', '清除'), value: __t.q('.sheet input[type=password]')?.value || '' })`);
    record('29 认证', '点「清除」表单内状态变未保存（暂存，未删 keyring）',
      (cleared.row || '').includes('未保存密码') && !cleared.clear && cleared.value === '' && ((await mockCalls()).delete_password || 0) === delBefore,
      JSON.stringify(cleared));
    await clickText('.sheetfoot button', '取消');
    await sleep(400);
    const delAfterCancel = (await mockCalls()).delete_password || 0;
    record('29 认证', '取消表单不删除任何凭据', delAfterCancel === delBefore, `delete_password=${delAfterCancel}`);

    // 再进表单：仍是「已保存密码」（取消没有撤销掉凭据）
    await rightClickText('.hostitem', 'e2e-pw');
    await waitUntil(`!!__t.q('.menu')`);
    await clickText('.menu .menuitem', '编辑配置');
    await waitUntil(`!!__t.q('.sheet')`);
    await sleep(400);
    record('29 认证', '取消后凭据仍在（仍是已保存密码）', ((await ev(`__t.txt('.authrow')`)) || '').includes('已保存密码'), '');

    // 清除 + 保存 → 才真正 delete_password
    await ev(`(() => { const b = __t.byText('.authrow .tbtn', '清除'); b && b.click(); return true; })()`);
    await sleep(250);
    await clickText('.sheetfoot button', '保存');
    await sleep(700);
    const delAfterSave = (await mockCalls()).delete_password || 0;
    record('29 认证', '点保存主机后才真正 delete_password', delAfterSave === delBefore + 1, `${delBefore}→${delAfterSave}`);
  }

  // ============ 29. 连接时密码弹窗（不保存 / 取消 / SFTP 复用 / 过期重试） ============
  {
    // e2e-pw 现在没有凭据：点连接应先弹密码框且不建会话
    const tabs0 = await tabCountOf();
    await ev(`(() => { const el = __t.byText('.hostitem', 'e2e-pw'); el && el.dispatchEvent(new MouseEvent('dblclick', { bubbles: true })); return true; })()`);
    const opened = await waitUntil(`!!__t.q('.modal #pwInput')`, 3000);
    const modal = await ev(`({
      type: __t.q('#pwInput')?.type, ph: __t.q('#pwInput')?.placeholder,
      tabs: __t.qa('.tabbar .tab').length,
      title: __t.txt('.modal h3'),
      tip: __t.txt('.modal .pwtip'),
      buttons: __t.qa('.modalfoot button').map(b => ({
        t: b.textContent.replace(/\\s+/g, ' ').trim(), act: b.getAttribute('data-act'), strong: b.classList.contains('strong'),
      })),
    })`);
    record('29 密码弹窗', 'password 且无凭据：连接先弹密码框（不建会话）',
      opened && modal.type === 'password' && modal.tabs === tabs0, JSON.stringify(modal));
    record('29 密码弹窗', '底部两个按钮（仅本次连接 / 保存并连接），取消走 Esc 或点遮罩',
      modal.buttons.length === 2
        && (modal.buttons[0].t || '').includes('仅本次连接') && modal.buttons[0].act === 'pwsubmit'
        && (modal.buttons[1].t || '').includes('保存并连接') && modal.buttons[1].act === 'pwsaveconnect'
        && modal.buttons[1].strong === true
        && (modal.tip || '').includes('凭据管理器'),
      JSON.stringify({ buttons: modal.buttons, tip: modal.tip }));

    // Esc 取消：不创建任何会话
    await key('Escape');
    await sleep(500);
    const afterCancel = await ev(`({ modal: !!__t.q('.modal #pwInput'), tabs: __t.qa('.tabbar .tab').length })`);
    record('29 密码弹窗', '取消弹窗不创建任何会话', !afterCancel.modal && afterCancel.tabs === tabs0, JSON.stringify(afterCancel));

    // 点击外部（遮罩）取消：同样不建会话、不写 keyring
    await ev(`(() => { const el = __t.byText('.hostitem', 'e2e-pw'); el && el.dispatchEvent(new MouseEvent('dblclick', { bubbles: true })); return true; })()`);
    await waitUntil(`!!__t.q('.modal #pwInput')`, 3000);
    const beforeOutside = (await mockCalls()).set_password || 0;
    await ev(`(() => { __t.q('.scrim')?.click(); return true; })()`);
    await sleep(450);
    const outside = await ev(`({ modal: !!__t.q('.modal #pwInput'), tabs: __t.qa('.tabbar .tab').length })`);
    const afterOutside = (await mockCalls()).set_password || 0;
    record('29 密码弹窗', '点击弹窗外部取消：不建会话、不写 keyring',
      !outside.modal && outside.tabs === tabs0 && afterOutside === beforeOutside,
      JSON.stringify(outside) + ` set_password=${beforeOutside}→${afterOutside}`);

    // 点「仅本次连接」：密码随本次 connect 下行，不写 keyring
    // e2e-pw 首次连接未信任：先过指纹确认；接受后 requestConnect 不带密码，
    // 因此还要再输入一次（这正是真实后端 order：hostkey challenge 先于 auth）。
    const beforeTemp = (await mockCalls()).set_password || 0;
    await ev(`(() => { const el = __t.byText('.hostitem', 'e2e-pw'); el && el.dispatchEvent(new MouseEvent('dblclick', { bubbles: true })); return true; })()`);
    await waitUntil(`!!__t.q('.modal #pwInput')`);
    await ev(`(() => { __t.setValue(__t.q('#pwInput'), 'temp-pw'); return true; })()`);
    await clickText('.modalfoot button', '仅本次连接');
    const tofuPw = await waitUntil(`!!__t.q('.modal .fingerprint')`, 2500);
    if (tofuPw) {
      record('4 主机行', '密码主机首次连接同样先弹指纹确认（TOFU 不被绕过）', true, '');
      await clickText('.modalfoot button', '信任并连接');
      await waitUntil(`!!__t.q('.modal #pwInput')`, 3000);
      await ev(`(() => { __t.setValue(__t.q('#pwInput'), 'temp-pw'); return true; })()`);
      await clickText('.modalfoot button', '仅本次连接');
    }
    await sleep(1400);
    const tempConn = await ev(`({
      modal: !!__t.q('.modal'), tab: __t.qa('.tabbar .tab').find(t => t.textContent.includes('e2e-pw'))?.dataset.status,
      tabs: __t.qa('.tabbar .tab').length,
    })`);
    const afterTemp = (await mockCalls()).set_password || 0;
    record('29 密码弹窗', '「仅本次连接」：仅用本次内存密码连接，不写 keyring',
      !tempConn.modal && tempConn.tab === 'connected' && tempConn.tabs === tabs0 + 1 && afterTemp === beforeTemp,
      JSON.stringify(tempConn) + ` set_password=${beforeTemp}→${afterTemp}`);

    // SFTP 复用：同主机已有活动终端会话时不得重新弹密码
    await rightClickText('.tabbar .tab', 'e2e-pw');
    await waitUntil(`!!__t.q('.menu')`);
    await clickText('.menu .menuitem', '打开 SFTP');
    await sleep(1100);
    const sftpReuse = await ev(`({
      modal: !!__t.q('.modal #pwInput'),
      tabs: __t.qa('.tabbar .tab').length,
      sftp: __t.qa('.tabbar .tab').some(t => t.textContent.includes('SFTP')),
      panes: __t.qa('.pane').length,
    })`);
    record('29 闸门', 'SFTP 复用已有终端连接：不重复弹密码', !sftpReuse.modal && sftpReuse.sftp && sftpReuse.panes === 2, JSON.stringify(sftpReuse));

    // 同 host 同 kind 再来一次：切回已有标签，不新开
    const beforeDedupe = await tabCountOf();
    await rightClickText('.tabbar .tab', 'e2e-pw · SFTP');
    await waitUntil(`!!__t.q('.menu')`);
    await clickText('.menu .menuitem', '关闭标签');
    await sleep(600);
    await ev(`(() => { const t = __t.qa('.tabbar .tab').find(x => x.textContent.includes('e2e-pw')); t && t.click(); return true; })()`);
    await sleep(200);
    const beforeDedupe2 = await tabCountOf();
    await rightClickText('.hostitem', 'e2e-pw');
    await waitUntil(`!!__t.q('.menu')`);
    await clickText('.menu .menuitem', '连接终端');
    await sleep(700);
    const dedupe = await tabCountOf();
    record('29 闸门', '同 host + 同 kind 未断开时切回已有标签（不新建）',
      beforeDedupe2 === beforeDedupe - 1 && dedupe === beforeDedupe2,
      `${beforeDedupe}→${beforeDedupe2}→${dedupe}`);

    // 关闭 e2e-pw 的终端标签 → 再连接仍要密码（证明上次的临时密码没有落存储）
    await rightClickText('.tabbar .tab', 'e2e-pw');
    await waitUntil(`!!__t.q('.menu')`);
    await clickText('.menu .menuitem', '关闭标签');
    await sleep(600);

    // Enter 默认「保存并连接」：先 set_password 成功再 connect
    const beforeSaved = (await mockCalls()).set_password || 0;
    await ev(`(() => { const el = __t.byText('.hostitem', 'e2e-pw'); el && el.dispatchEvent(new MouseEvent('dblclick', { bubbles: true })); return true; })()`);
    await waitUntil(`!!__t.q('.modal #pwInput')`);
    const reopened = await ev(`!!__t.q('.modal #pwInput')`);
    await ev(`(() => { __t.setValue(__t.q('#pwInput'), 'saved-pw'); return true; })()`);
    await ev(`(() => { const ta = __t.q('#pwInput'); ta.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true, cancelable: true })); return true; })()`);
    await sleep(1200);
    const savedConn = await ev(`({
      modal: !!__t.q('.modal'), tab: __t.qa('.tabbar .tab').find(t => t.textContent.includes('e2e-pw'))?.dataset.status,
    })`);
    const afterSaved = (await mockCalls()).set_password || 0;
    record('29 密码弹窗', '临时密码未落存储：再次连接仍弹密码框', reopened, '');
    record('29 密码弹窗', '「保存并连接」：Enter 提交 → 写 keyring 后连接成功',
      !savedConn.modal && savedConn.tab === 'connected' && afterSaved === beforeSaved + 1,
      JSON.stringify(savedConn) + ` set_password=${beforeSaved}→${afterSaved}`);

    // 密码被拒绝：重开同一弹窗并提示可能过期，重试一次可成功
    await ev(`(() => { const t = __t.qa('.tabbar .tab').find(x => x.textContent.includes('e2e-pw')); t && t.click(); return true; })()`);
    await sleep(200);
    await rightClickText('.hostitem', 'pi-lab');
    await waitUntil(`!!__t.q('.menu')`);
    await clickText('.menu .menuitem', '连接终端');
    await waitUntil(`!!__t.q('.modal #pwInput')`, 3000);
    await ev(`(() => { __t.setValue(__t.q('#pwInput'), 'wrong-password'); return true; })()`);
    await clickText('.modalfoot button', '仅本次连接');
    const retried = await waitUntil(`!!__t.q('.modal #pwInput')`, 3000);
    const retryHint = await ev(`({ hint: __t.txt('.modal .pwretry'), modal: !!__t.q('.modal #pwInput') })`);
    record('29 密码弹窗', '密码被拒绝：重开同一弹窗并提示密码可能已过期',
      retried && retryHint.modal && (retryHint.hint || '').includes('可能已过期'), JSON.stringify(retryHint));
    await ev(`(() => { __t.setValue(__t.q('#pwInput'), 'pi-good-pw'); return true; })()`);
    await clickText('.modalfoot button', '仅本次连接');
    await sleep(1200);
    const piConn = await ev(`({
      modal: !!__t.q('.modal'),
      tab: __t.qa('.tabbar .tab').find(t => t.textContent.includes('pi-lab'))?.dataset.status,
    })`);
    record('29 密码弹窗', '重试一次后连接成功', !piConn.modal && piConn.tab === 'connected', JSON.stringify(piConn));
    await shot('19-password');
  }

  // ============ 30. 右键菜单：六个区域 / 键盘 / 禁用 / 删除走确认 ============
  {
    // 统一闸门：没有活动会话时不拿 hosts[0] 兜底
    await ev(`(() => { const b = __t.q('.tab.on .tx'); if (b) b.click(); return true; })()`);
    for (let i = 0; i < 10; i++) {
      const n = await tabCountOf();
      if (n === 0) break;
      await ev(`(() => { const b = __t.q('.tab.on .tx') || __t.q('.tabbar .tab .tx'); if (b) b.click(); return true; })()`);
      await sleep(250);
    }
    const noTabs = (await tabCountOf()) === 0;
    await clickText('.seg button', '片段');
    await sleep(350);
    await clickSel('.listitem');
    await sleep(500);
    const gate = await ev(`({ tabs: __t.qa('.tabbar .tab').length, toast: __t.txt('.toast') || '' })`);
    record('30 闸门', '无活动会话时提示而不是拿 hosts[0] 建会话',
      noTabs && gate.tabs === 0 && (gate.toast.includes('无活动会话') || gate.toast.includes('先连接')),
      JSON.stringify(gate));

    // port===0 的本地主机：跳过 SSH 认证，但仍走统一入口（不弹密码框）
    await clickText('.seg button', '主机');
    await sleep(350);
    await ev(`(() => { const el = __t.byText('.hostitem', 'localhost'); el && el.dispatchEvent(new MouseEvent('dblclick', { bubbles: true })); return true; })()`);
    await sleep(900);
    const localTerm = await ev(`({
      tabs: __t.qa('.tabbar .tab').length, modal: !!__t.q('.modal'),
      title: __t.txt('.tab.on .tn'), status: __t.q('.tab.on')?.dataset.status,
    })`);
    record('30 闸门', 'port===0 的本地主机跳过认证但仍走同一入口',
      localTerm.tabs === 1 && !localTerm.modal && localTerm.title === 'localhost' && localTerm.status === 'connected',
      JSON.stringify(localTerm));
    await ev(`(() => { const b = __t.q('.tab.on .tx'); if (b) b.click(); return true; })()`);
    await sleep(400);

    // 主机行右键：6 项 + 分隔线、先选中目标、无预选、菜单在视口内
    await clickText('.seg button', '主机');
    await sleep(350);
    await rightClickText('.hostitem', 'web-01');
    await waitUntil(`!!__t.q('.menu')`);
    await sleep(260);                     // 等入场动画结束再量尺寸
    const hostMenu = await menuInfo();
    const hostSel = await ev(`__t.byText('.hostitem', 'web-01')?.classList.contains('on')`);
    record('30 菜单·主机', '主机行右键 6 项 + 分隔线且高亮该主机',
      hostMenu.open && hostMenu.items.length === 6 && hostMenu.dividers === 2 && hostSel,
      hostMenu.items.map((i) => i.t).join(' | '));
    record('30 菜单·样式', 'role=menu/menuitem、条目高 30、宽 208–300、圆角 10、打开不预选',
      hostMenu.role === 'menu' && hostMenu.items.every((i) => i.role === 'menuitem')
      && hostMenu.itemH === 30 && hostMenu.minW === '208px' && hostMenu.maxW === '300px'
      && hostMenu.radius === '10px' && hostMenu.onCount === 0,
      JSON.stringify({ role: hostMenu.role, itemH: hostMenu.itemH, minW: hostMenu.minW, maxW: hostMenu.maxW, r: hostMenu.radius, on: hostMenu.onCount }));

    // 键盘 ↑↓ 在可用项间循环、Esc 关闭
    await key('ArrowDown');
    await sleep(150);
    const k1 = await ev(`__t.qa('.menu .menuitem.on').map(e => e.textContent.trim())[0] || null`);
    await key('ArrowDown');
    await sleep(150);
    const k2 = await ev(`__t.qa('.menu .menuitem.on').map(e => e.textContent.trim())[0] || null`);
    await key('ArrowUp');
    await sleep(150);
    const k3 = await ev(`__t.qa('.menu .menuitem.on').map(e => e.textContent.trim())[0] || null`);
    record('30 菜单·键盘', '↑↓ 在可用项间循环（首个 → 第二个 → 回第一个）',
      k1 === '连接终端' && k2 === '打开 SFTP' && k3 === '连接终端', `${k1}→${k2}→${k3}`);
    await key('Escape');
    await sleep(300);
    record('30 菜单·键盘', 'Esc 关闭菜单', !(await ev(`!!__t.q('.menu')`)), '');

    // Enter 执行第一个可用项（连接终端）→ 建会话
    await rightClickText('.hostitem', 'web-01');
    await waitUntil(`!!__t.q('.menu')`);
    await key('Enter');
    await sleep(1200);
    const enterRun = await ev(`({
      menu: !!__t.q('.menu'), tabs: __t.qa('.tabbar .tab').length,
      active: __t.txt('.tab.on .tn'),
    })`);
    record('30 菜单·键盘', '↵ 执行首个可用项且先关菜单', !enterRun.menu && enterRun.tabs >= 1 && enterRun.active === 'web-01', JSON.stringify(enterRun));

    // 视口右下角右键：向左上展开且不压状态栏
    await ev(`(() => {
      const el = __t.byText('.hostitem', 'web-01');
      el.dispatchEvent(new MouseEvent('contextmenu', { bubbles: true, cancelable: true, clientX: innerWidth - 2, clientY: innerHeight - 2 }));
      return true;
    })()`);
    await sleep(300);
    const edge = await menuInfo();
    record('30 菜单·定位', '贴右下角时向左上展开且不出屏、不压状态栏',
      edge.open && edge.inView && edge.rect.y + edge.rect.h <= edge.statusTop + 0.5,
      JSON.stringify({ rect: edge.rect, vw: edge.vw, vh: edge.vh, statusTop: edge.statusTop }));
    await key('Escape');
    await sleep(250);

    // 点外部关闭
    await rightClickText('.hostitem', 'web-01');
    await waitUntil(`!!__t.q('.menu')`);
    await ev(`(() => { document.body.dispatchEvent(new MouseEvent('mousedown', { bubbles: true })); return true; })()`);
    await sleep(250);
    record('30 菜单·关闭', '点外部关闭菜单', !(await ev(`!!__t.q('.menu')`)), '');

    // 输入框保留系统右键菜单
    const inputCtx = await ev(`(() => {
      const i = __t.q('.sidesearch input');
      i.dispatchEvent(new MouseEvent('contextmenu', { bubbles: true, cancelable: true, clientX: 120, clientY: 80 }));
      return { menu: !!__t.q('.menu'), prevented: null };
    })()`);
    record('30 菜单·输入框', '输入框保留系统右键菜单（不弹自绘菜单）', !inputCtx.menu, JSON.stringify(inputCtx));

    // 会话标签右键：7 项，重连仅断线可点（禁用项 aria-disabled 且不挂 data-act）
    await clickTitle('.tabplus', '模拟断线');
    await sleep(700);
    await rightClickText('.tabbar .tab', 'web-01');
    await waitUntil(`!!__t.q('.menu')`);
    const tabMenuInfo = await menuInfo();
    const items = tabMenuInfo.items.map((i) => i.t);
    record('30 菜单·标签', '会话标签右键 7 项齐全',
      tabMenuInfo.open && tabMenuInfo.items.length === 7
      && items[0].includes('重新连接') && items[1].includes('打开 SFTP') && items[2].includes('关闭标签')
      && items[3].includes('关闭其他') && items[4].includes('关闭右侧') && items[5].includes('新建本地终端') && items[6].includes('关闭全部'),
      items.join(' | '));
    const reconnectItem = tabMenuInfo.items[0];
    record('30 菜单·禁用', '断线标签「重新连接」可点；禁用项 aria-disabled 且无 data-act',
      !reconnectItem.dis && reconnectItem.act === 'reconnect'
      && tabMenuInfo.items.filter((i) => i.dis).every((i) => i.act === null),
      JSON.stringify(tabMenuInfo.items.map((i) => ({ t: i.t, dis: i.dis, act: i.act }))));
    // 用菜单重连
    await clickText('.menu .menuitem', '重新连接');
    await sleep(1400);
    const reconnected = await ev(`__t.qa('.tabbar .tab').find(t => t.textContent.includes('web-01'))?.dataset.status`);
    record('30 菜单·标签', '菜单「重新连接」恢复连接', reconnected === 'connected', reconnected || '');

    // 已连接标签：重新连接禁用、关闭右侧在最后一个标签时禁用
    await rightClickText('.tabbar .tab', 'web-01');
    await waitUntil(`!!__t.q('.menu')`);
    const tabMenu2 = await menuInfo();
    record('30 菜单·禁用', '已连接标签「重新连接」禁用且不挂 data-act',
      tabMenu2.items[0].dis && tabMenu2.items[0].act === null,
      JSON.stringify(tabMenu2.items[0]));
    await key('Escape');
    await sleep(250);

    // 终端右键：7 项；无选中时「复制」禁用；「全选」后可用
    await ev(`(() => { const t = __t.qa('.tabbar .tab').find(x => x.textContent.includes('web-01')); t && t.click(); return true; })()`);
    await sleep(700);
    await rightClickSel('.termwrap');
    await waitUntil(`!!__t.q('.menu')`);
    await sleep(260);
    const termMenuInfo = await menuInfo();
    const tItems = termMenuInfo.items.map((i) => i.t);
    record('30 菜单·终端', '终端右键 8 项齐全且无选中时「复制」禁用',
      termMenuInfo.items.length === 8 && tItems[0].includes('复制') && termMenuInfo.items[0].dis
      && tItems.some((t) => t.includes('全选')) && tItems.some((t) => t.includes('粘贴'))
      && tItems.some((t) => t.includes('清屏'))
      && tItems.some((t) => t.includes('新建本地终端')) && tItems.some((t) => t.includes('连接信息'))
      && tItems.some((t) => t.includes('打开日志文件夹'))
      && tItems.some((t) => t.includes('关闭标签')),
      `${tItems.join(' | ')} copy=${JSON.stringify(termMenuInfo.items[0])}`);
    await clickText('.menu .menuitem', '全选');
    await sleep(400);
    await rightClickSel('.termwrap');
    await waitUntil(`!!__t.q('.menu')`);
    const copyEnabled = await ev(`(() => { const i = __t.qa('.menu .menuitem')[0]; return { dis: i.getAttribute('aria-disabled'), act: i.getAttribute('data-act') }; })()`);
    record('30 菜单·终端', '全选后「复制」可用（挂 data-act）', copyEnabled.dis === null && copyEnabled.act === 'copyterm', JSON.stringify(copyEnabled));
    // 粘贴只写入输入行、不自动回车：应用内缓冲为空时提示，不应执行命令
    await key('Escape');
    await sleep(250);

    // SFTP 文件行 / 面板空白
    await rightClickText('.tabbar .tab', 'web-01');
    await waitUntil(`!!__t.q('.menu')`);
    await clickText('.menu .menuitem', '打开 SFTP');
    await sleep(1300);
    // 回到必定同时含目录与文件的远程目录（/home/deploy：README + app/backups/scripts）
    for (let i = 0; i < 2; i++) {
      await ev(`(() => { const b = __t.qa('.pane[data-side="remote"] .iconbtn').find(x => (x.getAttribute('title') || '').includes('返回上级')); if (b) b.click(); return !!b; })()`);
      await sleep(450);
    }
    const remoteRows = await ev(`({ dirs: __t.qa('.pane[data-side="remote"] .frow.dir').length, files: __t.qa('.pane[data-side="remote"] .frow:not(.dir)').length })`);
    await rightClickSel('.pane[data-side="remote"] .frow:not(.dir)', 0, 40, 8);
    await waitUntil(`!!__t.q('.menu')`);
    await sleep(260);
    const fileMenuInfo = await menuInfo();
    const fItems = (fileMenuInfo.items || []).map((i) => i.t);
    const fileSel = await ev(`__t.qa('.pane[data-side="remote"] .frow.sel').length`);
    record('30 菜单·文件', 'SFTP 文件行右键：下载/重命名/复制路径/删除，且文件先进入选中态',
      remoteRows.files > 0 && (fileMenuInfo.items || []).length === 4 && (fItems[0] || '').includes('下载到本地')
      && fItems.includes('重命名') && fItems.includes('复制路径') && fItems.includes('删除') && fileSel === 1,
      `${fItems.join(' | ')} sel=${fileSel} rows=${JSON.stringify(remoteRows)}`);
    // 删除走确认弹窗，取消不删除
    const beforeDel = await ev(`__t.qa('.pane[data-side="remote"] .frow:not(.dir)').length`);
    await clickText('.menu .menuitem', '删除');
    const confirmOpen = await waitUntil(`!!__t.q('.modal')`, 2500);
    const confirmTitle = await ev(`__t.txt('.modal h3')`);
    await clickText('.modalfoot button', '取消');
    await sleep(500);
    const afterDel = await ev(`__t.qa('.pane[data-side="remote"] .frow:not(.dir)').length`);
    record('30 菜单·删除', '删除文件走确认弹窗（取消不删除）',
      confirmOpen && (confirmTitle || '').includes('删除文件') && afterDel === beforeDel,
      `${confirmTitle} ${beforeDel}→${afterDel}`);

    // 目录行：右键给「打开」
    await rightClickSel('.pane[data-side="remote"] .frow.dir', 0, 40, 8);
    await waitUntil(`!!__t.q('.menu')`);
    await sleep(200);
    const dirMenu = await ev(`__t.qa('.menu .menuitem').map(e => e.textContent.replace(/\\s+/g,' ').trim())`);
    record('30 菜单·文件', 'SFTP 目录行右键首项为「打开」', (dirMenu[0] || '').includes('打开'), dirMenu.join(' | '));
    await key('Escape');
    await sleep(250);

    // 面板空白：按侧只给对应的「到此处」，远端不出现「下载到此处」
    await rightClickSel('.pane[data-side="remote"]', 0, 420, 400);
    await waitUntil(`!!__t.q('.menu')`);
    const remotePane = await ev(`__t.qa('.menu .menuitem').map(e => e.textContent.replace(/\\s+/g,' ').trim())`);
    await key('Escape');
    await sleep(200);
    await rightClickSel('.pane[data-side="local"]', 0, 420, 400);
    await waitUntil(`!!__t.q('.menu')`);
    const localPane = await ev(`__t.qa('.menu .menuitem').map(e => e.textContent.replace(/\\s+/g,' ').trim())`);
    await key('Escape');
    await sleep(200);
    record('30 菜单·面板', 'SFTP 面板空白：刷新/新建文件夹 + 按侧的上传或下载到此处',
      remotePane.length === 3 && (remotePane[2] || '').includes('上传选中项')
      && !remotePane.some((t) => t.includes('下载'))
      && localPane.length === 3 && (localPane[2] || '').includes('下载选中项')
      && !localPane.some((t) => t.includes('上传')),
      `remote=${remotePane.join('|')} local=${localPane.join('|')}`);

    // 复制路径：按平台格式化并写入应用内缓冲（剪贴板失败也不影响）
    await rightClickSel('.pane[data-side="remote"] .frow:not(.dir)', 0, 40, 8);
    await waitUntil(`!!__t.q('.menu')`);
    await clickText('.menu .menuitem', '复制路径');
    await sleep(400);
    const copiedToast = await ev(`__t.txt('.toast') || ''`);
    record('30 菜单·复制路径', '复制路径按平台格式化（远端 POSIX）', copiedToast.includes('/etc/') || copiedToast.includes('已复制路径'), copiedToast);

    // 侧栏空白：主机页 6 项（含「新增分组」）
    await rightClickSel('.sidelist', 0, 60, 400);
    await waitUntil(`!!__t.q('.menu')`);
    await sleep(150);
    const sideMenuInfo = await menuInfo();
    const sItems = sideMenuInfo.items.map((i) => i.t);
    record('30 菜单·侧栏', '侧栏空白右键 6 项（主机页，含新增分组）',
      sideMenuInfo.items.length === 6 && sItems[0].includes('新建主机') && sItems[1].includes('新增分组')
      && sItems[2].includes('导入配置')
      && sItems[3].includes('新建片段') && sItems[4].includes('新建转发规则') && sItems[5].includes('备份与迁移'),
      sItems.join(' | '));
    // 点条目：先关菜单再执行（打开新建主机 Sheet）
    await clickText('.menu .menuitem', '新建主机');
    await sleep(500);
    const closedFirst = await ev(`({ menu: !!__t.q('.menu'), sheet: !!__t.q('.sheet') })`);
    record('30 菜单·执行', '点条目先关菜单再执行动作', !closedFirst.menu && closedFirst.sheet, JSON.stringify(closedFirst));
    await closeOverlay();

    // 新增分组：右键 → 新增分组 → 命名弹窗 → 创建 → 侧栏出现空分组（计数 0）
    await rightClickSel('.sidelist', 0, 60, 400);
    await waitUntil(`!!__t.q('.menu')`);
    await sleep(120);
    await clickText('.menu .menuitem', '新增分组');
    await waitUntil(`!!__t.q('#groupName')`, 2500);
    const groupDialogShown = await ev(`!!__t.q('#groupName')`);
    await ev(`(() => { const i = __t.q('#groupName'); if (!i) return false;
      const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set;
      setter.call(i, 'E2E 新分组'); i.dispatchEvent(new Event('input', { bubbles: true })); return true; })()`);
    await sleep(120);
    await ev(`(() => { const b = [...__t.qa('.modal button')].find((x) => (x.textContent || '').includes('创建')); if (b) b.click(); return true; })()`);
    await sleep(900);
    const groupState = await ev(`({ dialog: !!__t.q('#groupName'),
      row: [...__t.qa('.grouprow')].map((g) => (g.textContent || '').replace(/\\s+/g, ' ').trim()).find((t) => t.includes('E2E 新分组')) || null })`);
    record('30 菜单·分组', '右键「新增分组」→ 命名 → 侧栏出现空分组（计数 0）',
      groupDialogShown && !groupState.dialog && !!groupState.row && groupState.row.includes('0'),
      JSON.stringify(groupState));
    await shot('20-newgroup');
  }


  // ============ 终端内查找 / 快速连接 / 断线自动重连 / 会话日志 ============
  {
    // 回到 web-01 终端：后面的断言都需要真实 xterm 实例
    await ev(`(() => { const t = __t.qa('.tabbar .tab').find(x => x.textContent.includes('web-01')); t && t.click(); return true; })()`);
    await waitUntil(`!!__t.q('.termhost .xterm')`, 3000);
    await sleep(450);

    // --- 每个实例注册 SearchAddon + WebLinksAddon ---
    const addons = await ev(`(window.__termAddonInfo ? window.__termAddonInfo() : null)`);
    record('终端查找', '每个 xterm 实例注册 SearchAddon 与 WebLinksAddon',
      !!addons && addons.terms >= 1 && addons.search === addons.terms && addons.links === addons.terms, JSON.stringify(addons));

    await focusXterm();
    await key('f', { ctrlKey: true });
    const findOpened = await waitUntil(`!!__t.q('.termfind')`, 2500);
    const findUi = await ev(`({
      sideFocused: document.activeElement === __t.q('#sideSearch'),
      input: !!__t.q('.termfind input[data-find="input"]'),
      prev: !!__t.q('.termfind [data-find="prev"]'),
      next: !!__t.q('.termfind [data-find="next"]'),
      close: !!__t.q('.termfind [data-find="close"]'),
      count: __t.txt('.termfind [data-find="count"]'),
      rect: __t.rect('.termfind'),
      termRect: __t.rect('.termwrap'),
      label: __t.q('.termfind')?.getAttribute('aria-label'),
    })`);
    record('终端查找', '终端聚焦 Ctrl+F 打开锚定右上角的查找条（输入 + ↑↓ + 计数 + 关闭）',
      findOpened && findUi.input && findUi.prev && findUi.next && findUi.close && findUi.count === '0/0' && !findUi.sideFocused
      && !!findUi.rect && !!findUi.termRect
      && findUi.rect.x > findUi.termRect.x + findUi.termRect.w / 2
      && findUi.rect.y < findUi.termRect.y + 90,
      JSON.stringify(findUi));

    // 输入即时 incremental 查找：命中并显示 n/m
    await setVal('.termfind input', 'Ruthenium');
    await sleep(500);
    const hit = await ev(`({ count: __t.txt('.termfind [data-find="count"]'), bad: __t.q('.termfind input').classList.contains('bad') })`);
    const hitM = /^(\d+)\/(\d+)$/.exec(hit.count || '');
    record('终端查找', '输入即时匹配并显示 n/m 计数', !!hitM && Number(hitM[2]) > 0 && Number(hitM[1]) >= 1 && !hit.bad, JSON.stringify(hit));
    await shot('30-termfind');

    // 明暗主题各验一次：高亮配色实时从 --accent / --accent2 派生，且深色下仍能命中
    const lightColors = await ev(`window.__termSearchColors ? window.__termSearchColors() : null`);
    await clickTitle('.titlebar .iconbtn', '外观');
    await sleep(700);
    await setVal('.termfind input', '');
    await sleep(200);
    await setVal('.termfind input', 'Ruthenium');
    await sleep(500);
    await shot('30-termfind-dark');
    const darkInfo = await ev(`({
      colors: window.__termSearchColors ? window.__termSearchColors() : null,
      count: __t.txt('.termfind [data-find="count"]'),
      dark: document.body.classList.contains('dark'),
    })`);
    await clickTitle('.titlebar .iconbtn', '外观');
    await sleep(600);
    const hexOk = (c) => !!c && /^#[0-9a-f]{6}$/i.test(c.matchBackground || '') && /^#[0-9a-f]{6}$/i.test(c.activeMatchBackground || '');
    record('终端查找', '明暗主题高亮配色都从 --accent / --accent2 派生且深色下仍可命中',
      darkInfo.dark === true && hexOk(lightColors) && hexOk(darkInfo.colors)
      && lightColors.matchBackground !== darkInfo.colors.matchBackground
      && /^[1-9]\d*\/[1-9]\d*$/.test(darkInfo.count || ''),
      JSON.stringify({ light: lightColors, dark: darkInfo.colors, count: darkInfo.count, dark: darkInfo.dark }));

    // ↑ / ↓ / Enter 在匹配间跳转
    await clickSel('.termfind [data-find="next"]');
    await sleep(250);
    const afterNext = await ev(`__t.txt('.termfind [data-find="count"]')`);
    await clickSel('.termfind input');
    await sleep(120);
    await key('Enter');
    await sleep(250);
    const afterEnter = await ev(`__t.txt('.termfind [data-find="count"]')`);
    record('终端查找', '↑↓ / Enter 在匹配间跳转且计数保持',
      /^\d+\/\d+$/.test(afterNext || '') && /^\d+\/\d+$/.test(afterEnter || ''), `${hit.count}→${afterNext}→${afterEnter}`);

    // Esc 关闭并清高亮（装饰 + 选中都被清掉）
    await key('Escape');
    await sleep(400);
    const findClosed = await ev(`({
      bar: !!__t.q('.termfind'),
      sel: window.__termSelection ? window.__termSelection() : '',
      active: document.activeElement?.className || '',
    })`);
    record('终端查找', 'Esc 关闭查找条并清除高亮/选中',
      !findClosed.bar && findClosed.sel === '' && String(findClosed.active).includes('xterm'), JSON.stringify(findClosed));

    // 无匹配：0/0 + 红色边框
    await focusXterm();
    await key('f', { ctrlKey: true });
    await waitUntil(`!!__t.q('.termfind')`, 2000);
    await setVal('.termfind input', 'zzz-not-in-buffer-zzz');
    await sleep(450);
    const noMatch = await ev(`(() => {
      const i = __t.q('.termfind input');
      return { count: __t.txt('.termfind [data-find="count"]'), bad: i.classList.contains('bad'),
        invalid: i.getAttribute('aria-invalid'), border: getComputedStyle(i).borderColor };
    })()`);
    record('终端查找', '无匹配显示 0/0 且输入框红色边框',
      noMatch.count === '0/0' && noMatch.bad && noMatch.invalid === 'true' && /217|255|211|d92c20|ff6b60/i.test(noMatch.border || ''),
      JSON.stringify(noMatch));
    await key('Escape');
    await sleep(300);
  }

  // ============ 快速连接（不建主机连一次） ============
  {
    await key('k', { ctrlKey: true });
    await waitUntil(`!!__t.q('.palette')`);
    await setVal('.palette input', '快速连接');
    await sleep(450);
    const hasQuick = await ev(`__t.qa('.palrow .pt').some(e => e.textContent.trim() === '快速连接')`);
    record('快速连接', '命令面板固定项「快速连接」存在', hasQuick, '');
    await key('Enter');
    await sleep(400);
    const quickMode = await ev(`({
      panel: !!__t.q('.palquick'),
      hint: __t.txt('.palquick .quickhint'),
      ph: __t.q('.palette input')?.placeholder,
      quickInput: __t.q('.palette input')?.getAttribute('data-quick'),
    })`);
    record('快速连接', '选中后输入框进入 user@host[:port] 解析模式',
      quickMode.panel && quickMode.quickInput === 'input' && /user@host/.test(quickMode.hint || '') && /user@host/.test(quickMode.ph || ''),
      JSON.stringify(quickMode));

    await ev(`__t.q('.palette input').focus()`);
    await setVal('.palette input', 'bad input');
    await key('Enter');
    await sleep(350);
    const badHint = await ev(`({
      err: __t.q('.palquick .quickhint')?.classList.contains('err'),
      text: __t.txt('.palquick .quickhint'),
      inputBad: __t.q('.palette input')?.classList.contains('bad'),
      palette: !!__t.q('.palette'),
      tabs: __t.qa('.tabbar .tab').length,
    })`);
    record('快速连接', '非法输入给提示且不建会话',
      badHint.err && /user@host/.test(badHint.text || '') && badHint.inputBad && badHint.palette, JSON.stringify(badHint));

    // 合法输入：known=false，仍先弹指纹确认（证明没绕过统一闸门）
    await ev(`__t.q('.palette input').focus()`);
    await setVal('.palette input', 'deploy@10.0.0.5:2222');
    await key('Enter');
    const hkShown = await waitUntil(`!!__t.q('.modal')`, 3000);
    const hkText = await ev(`(__t.q('.fingerprint')?.textContent || '').replace(/\\s+/g, ' ').trim() + ' | ' + (__t.txt('.modalfoot') || '')`);
    record('快速连接', '临时主机 known=false：仍先弹指纹确认（不绕过闸门）',
      hkShown && /10\.0\.0\.5/.test(hkText || '') && /信任并连接/.test(hkText || ''), hkText || '');
    await clickText('.modalfoot button', '信任并连接');
    const quickConnected = await waitUntil(`__t.qa('.tabbar .tab').some(t => t.textContent.includes('deploy@10.0.0.5') && t.dataset.status === 'connected')`, 6000);
    const quickTab = await ev(`(() => { const t = __t.qa('.tabbar .tab').find(x => x.textContent.includes('deploy@10.0.0.5')); return t ? { status: t.dataset.status, title: t.querySelector('.tn')?.textContent.trim() } : null; })()`);
    record('快速连接', '连接成功后标签标题为 user@host', quickConnected && quickTab && quickTab.title === 'deploy@10.0.0.5', JSON.stringify(quickTab));
    await shot('31-quickconnect');

    // 标签右键「保存为主机」→ 打开预填好的 hostform
    await rightClickText('.tabbar .tab', 'deploy@10.0.0.5');
    await waitUntil(`!!__t.q('.menu')`);
    const quickMenu = await menuInfo();
    const saveItem = quickMenu.items.find((i) => i.t.includes('保存为主机'));
    record('快速连接', '快速连接标签右键出现「保存为主机」', !!saveItem && saveItem.act === 'savequickhost', quickMenu.items.map((i) => i.t).join(' | '));
    await clickText('.menu .menuitem', '保存为主机');
    await waitUntil(`!!__t.q('.sheet')`, 3000);
    await sleep(300);
    const prefilled = await ev(`(() => {
      const ins = __t.qa('.sheet input.inp');
      return { title: __t.txt('.sheethead h3'), name: ins[0]?.value, addr: ins[1]?.value, user: ins[2]?.value, port: ins[3]?.value };
    })()`);
    record('快速连接', '「保存为主机」打开预填好的新建主机表单',
      /新建主机/.test(prefilled.title || '') && prefilled.name === 'deploy@10.0.0.5' && prefilled.addr === '10.0.0.5'
      && prefilled.user === 'deploy' && prefilled.port === '2222', JSON.stringify(prefilled));
    await closeOverlay();

    // 关标签即丢弃临时主机（不写 hosts.toml / 不残留内存）
    await ev(`(() => { const t = __t.qa('.tabbar .tab').find(x => x.textContent.includes('deploy@10.0.0.5')); const x = t?.querySelector('.tx'); if (x) x.click(); return !!x; })()`);
    await sleep(600);
    const quickGone = await ev(`({
      tab: __t.qa('.tabbar .tab').some(t => t.textContent.includes('deploy@10.0.0.5')),
      hosts: __t.qa('.hostitem .hname').some(e => (e.textContent || '').includes('deploy@')),
      called: ((window.__mockCalls || {}).connect_transient || 0) > 0,
    })`);
    record('快速连接', '关标签即丢弃临时主机（不写 hosts.toml，走 connect_transient）',
      !quickGone.tab && !quickGone.hosts && quickGone.called, JSON.stringify(quickGone));
  }

  // ============ 断线自动重连（默认关） ============
  {
    // 回到 web-01 终端再验证
    await ev(`(() => { const t = __t.qa('.tabbar .tab').find(x => x.textContent.includes('web-01')); t && t.click(); return true; })()`);
    await waitUntil(`!!__t.q('.termhost .xterm')`, 3000);
    await sleep(350);

    await key(',', { ctrlKey: true });
    await waitUntil(`!!__t.q('.sheet')`);
    const autoRow = await ev(`(() => {
      const row = __t.q('.sheet .setrow[data-setting="autoReconnect"]');
      if (!row) return null;
      const sw = row.querySelector('.switch');
      return { text: row.querySelector('.srt')?.textContent.trim(), checked: sw?.getAttribute('aria-checked'), sub: !!row.querySelector('.srs') };
    })()`);
    record('自动重连', '设置「行为」段有「断线自动重连」开关且默认关',
      !!autoRow && /自动重连/.test(autoRow.text || '') && autoRow.checked === 'false' && !autoRow.sub, JSON.stringify(autoRow));
    await ev(`(() => { const row = __t.q('.sheet .setrow[data-setting="autoReconnect"]'); row?.querySelector('.switch')?.click(); return true; })()`);
    await sleep(300);
    const autoOn = await ev(`__t.q('.sheet .setrow[data-setting="autoReconnect"] .switch')?.getAttribute('aria-checked')`);
    await closeOverlay();
    await sleep(300);

    // 模拟网络掉线：reason=lost → 进入 1s 倒计时，标签保持 connecting 样式
    const lossTarget = await ev(`(() => {
      const t = __t.q('.tab.on') || __t.qa('.tabbar .tab').find(x => x.textContent.includes('web-01'));
      if (!t) return null;
      window.__mockEmit('conn://state', { sessionId: t.dataset.arg, state: 'disconnected', reason: 'lost' });
      return t.dataset.arg;
    })()`);
    const retryShown = await waitUntil(`!!__t.q('.banner[data-retry]')`, 2500);
    const retryUi = await ev(`({
      status: __t.q('.tab.on')?.dataset.status,
      text: __t.txt('.banner[data-retry]'),
      retrying: __t.q('.termwrap')?.dataset.retrying,
      term: !!__t.q('.termhost .xterm'),
    })`);
    record('自动重连', '断线（lost）进入自动重连：标签保持 connecting + 横幅倒计时「将于 Ns 后重连（k/5）」',
      retryShown && autoOn === 'true' && retryUi.status === 'connecting' && retryUi.retrying === '1' && retryUi.term
      && /1\/5/.test(retryUi.text || '') && /立即重连/.test(retryUi.text || ''),
      JSON.stringify({ retryUi, lossTarget, autoOn }));

    // 1s 后自动重试（mock 约 900ms 握手）→ 恢复 connected、横幅消失、scrollback 保留
    const recovered = await waitUntil(`__t.q('.tab.on')?.dataset.status === 'connected' && !__t.q('.banner[data-retry]')`, 8000);
    const afterRetry = await ev(`({
      status: __t.q('.tab.on')?.dataset.status,
      banner: !!__t.q('.banner'),
      retrying: __t.q('.termwrap')?.dataset.retrying,
      rows: window.__termText ? window.__termText().length : 0,
    })`);
    record('自动重连', '自动重试成功后恢复 connected、横幅消失且终端缓冲区保留',
      recovered && afterRetry.status === 'connected' && !afterRetry.banner && !afterRetry.retrying && afterRetry.rows > 0,
      JSON.stringify(afterRetry));

    // 关掉开关后：lost 不再自动重连（停在断线终态，横幅给手动重连入口）
    await key(',', { ctrlKey: true });
    await waitUntil(`!!__t.q('.sheet')`);
    await ev(`(() => { const row = __t.q('.sheet .setrow[data-setting="autoReconnect"]'); row?.querySelector('.switch')?.click(); return true; })()`);
    await sleep(300);
    await closeOverlay();
    await sleep(300);
    await ev(`(() => { const t = __t.q('.tab.on'); if (!t) return false; window.__mockEmit('conn://state', { sessionId: t.dataset.arg, state: 'disconnected', reason: 'lost' }); return true; })()`);
    await sleep(1000);
    const noAuto = await ev(`({ status: __t.q('.tab.on')?.dataset.status, retry: !!__t.q('.banner[data-retry]'), banner: !!__t.q('.banner') })`);
    record('自动重连', '开关关闭时断线不自动重连（停在断线终态）',
      noAuto.status === 'disconnected' && !noAuto.retry && noAuto.banner, JSON.stringify(noAuto));
    // 手动重连恢复，供后续断言使用
    await ev(`(() => { const b = __t.byText('.banner button', '重新连接'); if (b) b.click(); return !!b; })()`);
    await waitUntil(`__t.q('.tab.on')?.dataset.status === 'connected'`, 6000);
    await sleep(300);
  }

  // ============ 连接选项与会话日志 ============
  {
    await key(',', { ctrlKey: true });
    await waitUntil(`!!__t.q('.sheet')`);
    const logRow = await ev(`(() => {
      const row = __t.q('.sheet .setrow[data-setting="logSessions"]');
      if (!row) return null;
      return { text: row.querySelector('.srt')?.textContent.trim(), checked: row.querySelector('.switch')?.getAttribute('aria-checked') };
    })()`);
    record('会话日志', '设置「行为」段有「默认记录会话日志」开关且默认关',
      !!logRow && /记录会话日志/.test(logRow.text || '') && logRow.checked === 'false', JSON.stringify(logRow));
    await closeOverlay();

    // 主机表单「连接选项」：initCmd 输入框 + 记录会话日志开关
    await rightClickText('.hostitem', 'web-01');
    await waitUntil(`!!__t.q('.menu')`);
    await clickText('.menu .menuitem', '编辑配置');
    await waitUntil(`!!__t.q('.sheet')`);
    await sleep(350);
    const connFields = await ev(`(() => {
      const sec = __t.qa('.sheet .sec').find(s => (s.querySelector('.sectitle')?.textContent || '') === '连接选项');
      if (!sec) return null;
      const inp = sec.querySelector('input[data-arg="initCommand"]');
      const field = inp?.closest('.field');
      const sw = sec.querySelector('.setrow[data-setting="hostlog"] .switch');
      return {
        label: field?.querySelector('label')?.textContent.trim(),
        ph: inp?.placeholder,
        log: !!sw, logChecked: sw?.getAttribute('aria-checked'),
      };
    })()`);
    record('连接选项', '表单「连接选项」有「连接后执行」输入框与「记录会话日志」开关',
      !!connFields && /连接后执行/.test(connFields.label || '') && /连接成功后自动执行/.test(connFields.ph || '')
      && connFields.log && connFields.logChecked === 'false', JSON.stringify(connFields));
    await ev(`(() => { const sw = __t.q('.sheet .setrow[data-setting="hostlog"] .switch'); sw && sw.click(); return true; })()`);
    await sleep(250);
    await clickText('.sheetfoot button', '保存主机');
    await sleep(900);

    // 会话级日志快照：关掉旧标签再连一次，新会话带 log=true
    await ev(`(() => { const t = __t.qa('.tabbar .tab').find(x => x.textContent.includes('web-01')); const x = t?.querySelector('.tx'); if (x) x.click(); return !!x; })()`);
    await sleep(500);
    await ev(`(() => { const el = __t.byText('.hostitem', 'web-01'); el && el.dispatchEvent(new MouseEvent('dblclick', { bubbles: true })); return true; })()`);
    await waitUntil(`__t.qa('.tabbar .tab').some(t => t.textContent.includes('web-01') && t.dataset.status === 'connected')`, 6000);
    await sleep(350);
    const recDot = await ev(`(() => {
      const d = __t.q('.statusbar .recdot[data-log="1"]');
      return d ? { title: d.getAttribute('title'), dot: !!d.querySelector('.dot'), next: d.nextElementSibling?.className || '' } : null;
    })()`);
    record('会话日志', '日志开启的会话在编码 chip 旁显示 ● 记录点',
      !!recDot && /正在记录会话日志/.test(recDot.title || '') && recDot.dot && /encbtn/.test(recDot.next || ''), JSON.stringify(recDot));

    // 终端右键「打开日志文件夹」
    await rightClickSel('.termwrap');
    let termMenuOpen = await waitUntil(`!!__t.q('.menu')`, 2500);
    if (!termMenuOpen) {
      await rightClickSel('.termwrap', 0, 320, 260);
      termMenuOpen = await waitUntil(`!!__t.q('.menu')`, 2500);
    }
    await sleep(240);
    const termMenu2 = await menuInfo();
    const t2Items = termMenu2.items || [];
    const termDiag = await ev(`({
      wraps: __t.qa('.termwrap').length, ctx: __t.qa('[data-ctx="term"]').length,
      xterm: !!__t.q('.termhost .xterm'), activeEl: document.activeElement?.className || '',
      overlays: __t.qa('.sheet,.modal,.palette,.menu,.encpop').map(e => e.className),
      tab: __t.q('.tab.on')?.dataset.status,
    })`);
    const logItem = t2Items.find((i) => i.t.includes('打开日志文件夹'));
    record('会话日志', '终端右键菜单有「打开日志文件夹」', termMenuOpen && !!logItem && logItem.act === 'openlogdir',
      JSON.stringify({ open: termMenu2.open, items: t2Items, diag: termDiag }));
    if (logItem) {
      await clickText('.menu .menuitem', '打开日志文件夹');
      await sleep(700);
      const logToast = await ev(`__t.txt('.toast') || ''`);
      record('会话日志', '浏览器 / mock 环境打开日志目录降级为路径提示', /日志目录/.test(logToast), logToast);
    } else {
      record('会话日志', '浏览器 / mock 环境打开日志目录降级为路径提示', false, '菜单未打开');
    }
    await shot('32-sessionlog');
  }


  // ============ 7/10/29/30. 覆盖表 30 行逐个「前往」 ============
  {
    await clickTitle('.sidefoot button', '覆盖表');
    await waitUntil(`!!__t.q('.covtable')`);
    const rowCount = await ev(`__t.qa('.covtable tbody tr').length`);
    record('29 覆盖表', '30 行功能↔组件', rowCount === 30, `rows=${rowCount}`);
    const row8 = await ev(`(() => { const tr = __t.qa('.covtable tbody tr')[7]; return tr ? [...tr.querySelectorAll('td')].map(td => td.textContent.trim()) : null; })()`);
    record('8 覆盖表', '第 8 行入口文案为「双击主机 / hover ▶」', !!row8 && (row8[2] || '').includes('双击') && (row8[2] || '').includes('▶'), JSON.stringify(row8));
    const row29 = await ev(`(() => { const tr = __t.qa('.covtable tbody tr')[28]; return tr ? [...tr.querySelectorAll('td')].map(td => td.textContent.trim()) : null; })()`);
    const row30 = await ev(`(() => { const tr = __t.qa('.covtable tbody tr')[29]; return tr ? [...tr.querySelectorAll('td')].map(td => td.textContent.trim()) : null; })()`);
    record('30 覆盖表', '新增「认证方式」「右键菜单与情境操作」两行',
      !!row29 && row29[0] === '认证方式' && !!row30 && row30[0] === '右键菜单与情境操作',
      `${row29?.[0]} / ${row30?.[0]}`);
    await shot('17-coverage');
    if (rowCount === 30) {
      let ok = 0;
      const fails = [];
      for (let i = 0; i < rowCount; i++) {
        // 每次重新打开覆盖表
        if (await overlayClosed() === false) await closeOverlay();
        await clickTitle('.sidefoot button', '覆盖表');
        const ready = await waitUntil(`!!__t.q('.covtable')`, 3000);
        if (!ready) { fails.push(`${i + 1}:未打开`); continue; }
        const name = await ev(`__t.qa('.covtable tbody tr')[${i}].querySelector('td')?.textContent.trim()`);
        const clicked = await ev(`(() => { const b = __t.qa('.covtable tbody tr')[${i}].querySelector('button'); if (!b) return false; b.click(); return true; })()`);
        await sleep(700);
        // 该行点击后：要么打开了某个 overlay，要么主区/侧栏状态变化（例如 empty/focus/theme）
        const stateChanged = await ev(`(() => {
          const overlay = !!(__t.q('.sheet') || __t.q('.modal') || __t.q('.palette') || __t.q('.menu') || __t.q('.encpop'));
          const bodyDark = document.body.classList.contains('dark');
          const focus = __t.q('.app').classList.contains('focus');
          const empty = !!__t.q('.empty');
          const xnav = __t.qa('.tabbar .tab.data').length > 0;
          const searchFocused = document.activeElement === __t.q('.sidesearch input');
          const banner = !!__t.q('.banner');
          const connwait = !!__t.q('.connwait');
          const term = !!__t.q('.xterm');
          const sftp = __t.qa('.pane').length === 2;
          return { overlay, bodyDark, focus, empty, xnav, searchFocused, banner, connwait, term, sftp };
        })()`);
        const any = Object.values(stateChanged).some(Boolean);
        if (clicked && any) ok += 1; else fails.push(`${i + 1}:${name}`);
        // 复位：关闭 overlay、退出专注、恢复浅色/经典布局
        if (stateChanged.focus) { await clickSel('.focusexit').catch(() => {}); await sleep(300); }
        if (stateChanged.bodyDark) { await clickTitle('.titlebar .iconbtn', '外观').catch(() => {}); await sleep(300); }
        if (await overlayClosed() === false) await closeOverlay();
      }
      record('29 覆盖表', '每行「前往」都能抵达对应状态', ok === rowCount, `${ok}/${rowCount} 行可达${fails.length ? '；失败: ' + fails.join(', ') : ''}`);
    }

    // goto 要真实切到对应状态：#29 打开表单到认证区、#30 打开一个示例右键菜单
    if (rowCount === 30) {
      if (await overlayClosed() === false) await closeOverlay();
      await clickTitle('.sidefoot button', '覆盖表');
      await waitUntil(`!!__t.q('.covtable')`);
      await ev(`(() => { const b = __t.qa('.covtable tbody tr')[28].querySelector('button'); b && b.click(); return true; })()`);
      await sleep(800);
      const authGoto = await ev(`({
        sheet: !!__t.q('.sheet'), field: !!__t.q('#authField'),
        pills: __t.qa('#authField .pill').map(p => p.textContent.trim()),
        on: __t.qa('#authField .pill.on').map(p => p.textContent.trim()),
        pw: !!__t.q('.sheet input[type=password]'),
        row: __t.txt('.authrow') || '',
      })`);
      record('30 覆盖表', '「认证方式」前往 → 打开表单到认证区（密码主机带状态行）',
        authGoto.sheet && authGoto.field && authGoto.pills.length === 3 && authGoto.on[0] === '密码' && authGoto.pw,
        JSON.stringify(authGoto));
      if (await overlayClosed() === false) await closeOverlay();

      await clickTitle('.sidefoot button', '覆盖表');
      await waitUntil(`!!__t.q('.covtable')`);
      await ev(`(() => { const b = __t.qa('.covtable tbody tr')[29].querySelector('button'); b && b.click(); return true; })()`);
      await sleep(700);
      const ctxGoto = await menuInfo();
      record('30 覆盖表', '「右键菜单与情境操作」前往 → 打开一个示例右键菜单',
        ctxGoto.open && ctxGoto.items.length >= 5, ctxGoto.open ? ctxGoto.items.map((i) => i.t).join(' | ') : '未打开');
      if (await overlayClosed() === false) await closeOverlay();
    }
  }

  // ============ 10. 空状态（关闭全部标签） ============
  {
    const closedAll = await ev(`(() => {
      // 走命令面板的「关闭全部标签」动作（覆盖表里的 goto 也能触发，这里用面板更接近真实操作）
      const ev2 = new KeyboardEvent('keydown', { key: 'k', ctrlKey: true, bubbles: true, cancelable: true });
      document.body.dispatchEvent(ev2);
      return true;
    })()`);
    await waitUntil(`!!__t.q('.palette')`);
    await setVal('.palette input', '关闭全部');
    await sleep(500);
    const palRows = await ev(`__t.qa('.palrow .pt').map(e => e.textContent.trim())`);
    if (palRows.length) {
      await key('Enter');
      await sleep(900);
    }
    for (let i = 0; i < 8; i++) {
      if (await ev(`!!__t.q('.empty')`)) break;
      // 兜底：逐个关闭标签
      const closedOne = await ev(`(() => { const b = __t.q('.tab.on .tx'); if (!b) return false; b.click(); return true; })()`);
      if (!closedOne) break;
      await sleep(400);
    }
    const empty = await ev(`({ shown: !!__t.q('.empty'), buttons: __t.qa('.empty .erow button').length, tabs: __t.qa('.tabbar .tab').length, menu: !!__t.q('.menu'), stage: __t.q('.stage')?.firstElementChild?.className })`);
    record('10 空状态', '关闭全部标签后显示空状态 + 3 按钮', empty.shown && empty.buttons === 3, `shown=${empty.shown} btn=${empty.buttons} tabs=${empty.tabs} stage=${empty.stage} palette=${palRows.join(',')}`);
    await shot('18-empty');
  }

  // ============ 主机标签选项（开发/测试/生产/堡垒机） ============
  {
    await ev(`document.dispatchEvent(new KeyboardEvent('keydown',{key:'n',ctrlKey:true,bubbles:true,cancelable:true}))`);
    await waitUntil(`!!__t.q('.sheet select[data-tag="1"]')`, 3000);
    const tagOpts = await ev(`[...__t.qa('.sheet select[data-tag="1"] option')].map((o) => ({ v: o.value, t: (o.textContent || '').trim() }))`);
    const vals = tagOpts.map((o) => o.v).join(',');
    record('19 设置', '主机标签下拉为 开发/测试/生产/堡垒机（值 dev/test/prod/bastion）',
      vals === 'dev,test,prod,bastion' && tagOpts.every((o) => o.t.length > 0),
      JSON.stringify(tagOpts));
    await closeOverlay();
  }

  // ============ 收尾：页面未捕获异常（读 CDP Runtime.exceptionThrown 事件） ============
    // 设备页 + 数字比较配对（mock：手填 IP → 入站弹窗 → 勾选授权 → 一致 → 已配对 → 解除）
      await closeOverlay();
      await clickText('.seg button', '设备');
      await sleep(500);
      const nearby = await ev(`__t.qa('.peerspanel [data-device-id]').length`);
      record('19 设备', '设备页列出附近设备', nearby === 2, `nearby=${nearby}`);
      const addrAlign = await ev(`(() => {
        const i = __t.q('[data-peer-addr]');
        const b = __t.q('.peers-add .tbtn');
        if (!i || !b) return null;
        const ri = i.getBoundingClientRect();
        const rb = b.getBoundingClientRect();
        return { ic: ri.top + ri.height / 2, bc: rb.top + rb.height / 2, ih: ri.height, bh: rb.height };
      })()`);
      record('19 设备', 'IP 输入框与连接配对按钮垂直对齐',
        !!addrAlign && Math.abs(addrAlign.ic - addrAlign.bc) <= 1 && Math.abs(addrAlign.ih - addrAlign.bh) <= 1,
        JSON.stringify(addrAlign));
      await ev(`(() => { const i = __t.q('[data-peer-addr]'); if (i) __t.setValue(i, '192.168.1.77'); return !!i; })()`);
      await clickText('.peerspanel .tbtn', '连接配对');
      await sleep(500);
      const pairCode = String(await ev(`__t.txt('[data-pair-code]')`) || '').trim();
      record('19 设备', '配对弹窗显示 6 位数字码', /^[0-9]{6}$/.test(pairCode), `code=${pairCode}`);
      const beforeGrants = await ev(`__t.qa('.modal [data-grant]').map(e => e.checked)`);
      for (const k of ['files', 'control', 'config', 'screen', 'clipboard', 'audio']) {
        await ev(`(() => { const c = __t.q('.modal [data-grant="${k}"]'); if (c) c.click(); return true; })()`);
      }
      await sleep(150);
      const afterGrants = await ev(`__t.qa('.modal [data-grant]').map(e => e.checked)`);
      record('19 设备', '三项授权默认全不勾、可逐项勾选',
        beforeGrants.length === 6 && beforeGrants.every((v) => v === false) && afterGrants.length === 6 && afterGrants.every((v) => v === true),
        `before=${beforeGrants} after=${afterGrants}`);
      await ev(`(() => { const b = __t.q('[data-pair-agree]'); b && b.click(); return true; })()`);
      await sleep(700);
      const pairedRow = await ev(`(() => {
        const r = __t.q('.peerrow[data-peer-id]');
        if (!r) return null;
        return { grants: __t.qa('.peerrow[data-peer-id] [data-grant]').map(e => e.checked), text: r.textContent.trim() };
      })()`);
      record('19 设备', '配对完成后出现在已配对列表且授权生效',
        !!pairedRow && pairedRow.grants.length === 6 && pairedRow.grants.every((v) => v === true),
        JSON.stringify(pairedRow));
      // 文件互传：已配对设备可打开对端 SFTP 标签（mock 会话）
      await ev(`(() => { const b = __t.q('.peerrow[data-peer-id] .peer-sftp'); b && b.click(); return !!b; })()`);
      await sleep(600);
      const sftpTab = await ev(`(() => {
        const t = [...document.querySelectorAll('.tabbar .tab .tn')].find((x) => x.textContent.includes('SFTP'));
        return t ? t.textContent.trim() : '';
      })()`);
      record('19 设备', '已配对设备可打开对端 SFTP 标签', sftpTab.includes('mock-laptop'), `title=${sftpTab}`);
      await ev(`(() => {
        const t = [...document.querySelectorAll('.tabbar .tab')].find((x) => x.textContent.includes('SFTP'));
        const x = t && t.querySelector('.tx');
        x && x.click();
        return !!x;
      })()`);
      await sleep(300);
      // 配置收件箱：默认有一条 → 导入后消失
      const inboxBefore = await ev(`__t.qa('[data-inbox-id]').length`);
      await ev(`(() => { const b = __t.q('[data-inbox-import]'); b && b.click(); return !!b; })()`);
      await sleep(700);
      const inboxAfter = await ev(`__t.qa('[data-inbox-id]').length`);
      record('19 设备', '配置收件箱可导入并移除条目',
        inboxBefore === 1 && inboxAfter === 0, `before=${inboxBefore} after=${inboxAfter}`);
      // 发送配置：成功提示
      await ev(`(() => { const b = __t.q('.peerrow[data-peer-id] .peer-sendcfg'); b && b.click(); return !!b; })()`);
      await sleep(400);
      const sendToast = await ev(`document.querySelector('.toast')?.textContent || ''`);
      record('19 设备', '发送本机配置给出成功提示', sendToast.includes('mock-laptop'), `toast=${sendToast}`);
      // 远程终端：打开控制标签（mock 会话）
      await ev(`(() => { const b = __t.q('.peerrow[data-peer-id] .peer-control'); b && b.click(); return !!b; })()`);
      await sleep(600);
      const termTab = await ev(`(() => {
        const t = [...document.querySelectorAll('.tabbar .tab .tn')].find((x) => x.textContent.includes('远程终端'));
        return t ? t.textContent.trim() : '';
      })()`);
      record('19 设备', '已配对设备可打开远程终端标签', termTab.includes('mock-laptop'), `title=${termTab}`);
      await ev(`(() => {
        const t = [...document.querySelectorAll('.tabbar .tab')].find((x) => x.textContent.includes('远程终端'));
        const x = t && t.querySelector('.tx');
        x && x.click();
        return !!x;
      })()`);
      await sleep(300);
      // 本机确认弹窗（对端请求控制本机）：允许后弹窗关闭
      await ev(`window.__mockEmit('peer://control-request', { consentId: 'c-mock-1', peerName: 'mock-laptop', peerFp: 'mockfp01', addr: '192.168.1.50', cols: 120, rows: 32, helloRequired: false })`);
      await sleep(300);
      const askShown = await ev(`!!__t.q('[data-control-allow]')`);
      await ev(`(() => { const b = __t.q('[data-control-allow]'); b && b.click(); return !!b; })()`);
      await sleep(300);
      const askGone = await ev(`!__t.q('[data-control-allow]')`);
      record('19 设备', '控制请求弹窗可允许并关闭', askShown === true && askGone === true, `shown=${askShown} gone=${askGone}`);
      // 受控横幅：started → 横幅出现；点「立即断开」→ 横幅消失
      await ev(`window.__mockEmit('peer://session', { action: 'started', sessionId: 'cs-1', peerName: 'mock-laptop', startedMs: Date.now(), addr: '192.168.1.50', cols: 120, rows: 32 })`);
      await sleep(400);
      const bannerShown = await ev(`!!__t.q('[data-control-banner]')`);
      await ev(`(() => { const b = __t.q('[data-kick]'); b && b.click(); return !!b; })()`);
      await sleep(400);
      const bannerGone = await ev(`!__t.q('[data-control-banner]')`);
      record('19 设备', '被控横幅可显示并一键断开', bannerShown === true && bannerGone === true, `shown=${bannerShown} gone=${bannerGone}`);
      // 拒绝提示
      await ev(`window.__mockEmit('peer://denied', { peerName: 'mock-laptop', reason: 'consent' })`);
      await sleep(300);
      const denyToast = await ev(`document.querySelector('.toast')?.textContent || ''`);
      record('19 设备', '拒绝控制请求给出提示', denyToast.includes('mock-laptop'), `toast=${denyToast}`);
      // 远程屏幕（只看）：打开查看标签（mock 无帧，只验证视图与工具栏）
      await ev(`(() => { const b = __t.q('.peerrow[data-peer-id] .peer-screen'); b && b.click(); return !!b; })()`);
      await sleep(700);
      const screenView = await ev(`(() => {
        const v = __t.q('.remoteview');
        const t = [...document.querySelectorAll('.tabbar .tab .tn')].find((x) => x.textContent.includes('屏幕'));
        return { view: !!v, quality: !!__t.q('[data-screen-quality]'), title: t ? t.textContent.trim() : '' };
      })()`);
      record('19 设备', '已配对设备可打开远程屏幕（只看）',
        screenView.view === true && screenView.quality === true && screenView.title.includes('mock-laptop'),
        JSON.stringify(screenView));
      await ev(`(() => { const b = __t.q('[data-screen-close]'); b && b.click(); return !!b; })()`);
      await sleep(400);
      // 屏幕查看请求弹窗
      await ev(`window.__mockEmit('peer://screen-request', { consentId: 's-mock-1', peerName: 'mock-laptop', peerFp: 'mockfp01', addr: '192.168.1.50', helloRequired: false })`);
      await sleep(300);
      const sAskShown = await ev(`!!__t.q('[data-screen-allow]')`);
      await ev(`(() => { const b = __t.q('[data-screen-allow]'); b && b.click(); return !!b; })()`);
      await sleep(300);
      const sAskGone = await ev(`!__t.q('[data-screen-allow]')`);
      record('19 设备', '屏幕查看请求可允许并关闭', sAskShown === true && sAskGone === true, `shown=${sAskShown} gone=${sAskGone}`);
      // 被查看横幅 + 一键断开
      await ev(`window.__mockEmit('peer://screen-session', { action: 'started', sessionId: 'vs-1', peerName: 'mock-laptop' })`);
      await sleep(400);
      const viewedShown = await ev(`!!__t.q('[data-viewed-id]')`);
      await ev(`(() => { const b = __t.q('[data-screen-kick]'); b && b.click(); return !!b; })()`);
      await sleep(400);
      const viewedGone = await ev(`!__t.q('[data-viewed-id]')`);
      record('19 设备', '被查看横幅可显示并一键断开', viewedShown === true && viewedGone === true, `shown=${viewedShown} gone=${viewedGone}`);
      // 最近控制记录（审计）
      const auditRow = await ev(`__t.q('.auditrow')?.textContent || ''`);
      record('19 设备', '设备页显示最近控制记录',
        auditRow.includes('mock-laptop') && auditRow.includes('192.168.1.50') && auditRow.includes('q65'),
        `row=${auditRow}`);
      await ev(`(() => { const b = __t.q('[data-audit-clear]'); b && b.click(); return !!b; })()`);
      await sleep(300);
      const auditGone = await ev(`!!__t.q('[data-audit-list]')`);
      record('19 设备', '可清空最近控制记录', auditGone === false, `list=${auditGone}`);
      await ev(`(() => { const c = __t.q('.peerrow[data-peer-id] [data-grant="control"]'); if (c) c.click(); return true; })()`);
      await sleep(300);
      const controlOff = await ev(`__t.q('.peerrow[data-peer-id] [data-grant="control"]')?.checked`);
      record('19 设备', '单台授权可即时撤销', controlOff === false, `control=${controlOff}`);
      await ev(`(() => { const b = __t.q('[data-peer-remove]'); b && b.click(); return true; })()`);
      await sleep(300);
      const remaining = await ev(`__t.qa('.peerrow[data-peer-id]').length`);
      record('19 设备', '解除配对后从列表移除', remaining === 0, `remaining=${remaining}`);
      await clickText('.seg button', '主机');
      await sleep(300);

  const exceptions = c.events
    .filter((e) => e.method === 'Runtime.exceptionThrown')
    .map((e) => e.params?.exceptionDetails?.text || 'exception');
  record(
    '0 稳定性',
    '运行期无未捕获异常',
    exceptions.length === 0,
    `${exceptions.length} 条${exceptions.length ? '：' + exceptions.slice(0, 3).join(' | ') : ''}`,
  );

  // ---------- 汇总 ----------
  const pass = results.filter((r) => r.ok === true).length;
  const fail = results.filter((r) => r.ok === false).length;
  const skip = results.filter((r) => r.ok === 'skip').length;
  console.log('\n===== 汇总 =====');
  console.log(`PASS ${pass} / FAIL ${fail} / SKIP ${skip} / TOTAL ${results.length}`);
  if (fail) {
    console.log('\n失败项：');
    for (const r of results.filter((x) => x.ok === false)) console.log(` - ${r.group} · ${r.name} ${r.detail ? '(' + r.detail + ')' : ''}`);
  }
  fs.writeFileSync(`${SHOTS}/results.json`, JSON.stringify({ results, shots, pass, fail, skip }, null, 2));
  console.log(`\n截图与结果: ${SHOTS}`);
  c.ws.close();
  process.exit(fail ? 1 : 0);
}

main().catch((e) => {
  console.error('SUITE_CRASH', e?.stack || e);
  process.exit(2);
});
