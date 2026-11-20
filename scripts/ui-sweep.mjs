/**
 * 全功能扫描：四语言 × 两种窗口尺寸 × 主要覆盖层，检查
 *   1) 运行期异常 / console.error
 *   2) 任何覆盖层越出视口
 *   3) 覆盖层内关键控件文字被裁切（英文/日文通常更长）
 *
 * 用法：node scripts/ui-sweep.mjs   （需 preview 4173 + headless Edge 9333）
 */
const CDP = process.env.SWEEP_CDP || 'http://127.0.0.1:9333';
const URL_APP = process.env.SWEEP_URL || 'http://localhost:4173/';

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
  const errors = [];
  ws.addEventListener('message', (ev) => {
    const m = JSON.parse(ev.data);
    if (m.id && pending.has(m.id)) {
      const { resolve, reject } = pending.get(m.id);
      pending.delete(m.id);
      m.error ? reject(new Error(JSON.stringify(m.error))) : resolve(m.result);
    } else if (m.method === 'Runtime.exceptionThrown') {
      errors.push('EXC ' + String(m.params.exceptionDetails.exception?.description || m.params.exceptionDetails.text).slice(0, 160));
    } else if (m.method === 'Runtime.consoleAPICalled' && m.params.type === 'error') {
      errors.push('ERR ' + m.params.args.map((a) => a.value ?? a.description ?? '').join(' ').slice(0, 160));
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
  return { ws, ready, send, errors };
}
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const CHECK_OVERLAY = `(() => {
  const vw = innerWidth, vh = innerHeight;
  const problems = [];
  const ov = document.querySelector('.sheet, .modal, .palette, .menu, .encpop');
  if (!ov) return { present: false, problems: [] };
  const r = ov.getBoundingClientRect();
  if (r.left < -1) problems.push('overlay 左侧超出 ' + Math.round(-r.left));
  if (r.top < -1) problems.push('overlay 顶部超出 ' + Math.round(-r.top));
  if (r.right > vw + 1) problems.push('overlay 右侧超出 ' + Math.round(r.right - vw));
  if (r.bottom > vh + 1) problems.push('overlay 底部超出 ' + Math.round(r.bottom - vh));
  // 覆盖层内控件文字裁切（排除有意 ellipsis）
  const clip = [];
  for (const el of ov.querySelectorAll('button, .pill, .setrow .srt, .field label, .menuitem, .covtable td')) {
    if (!el.offsetParent) continue;
    const cs = getComputedStyle(el);
    if (cs.overflow === 'hidden' && cs.textOverflow === 'ellipsis') continue;
    if (el.scrollWidth > el.clientWidth + 2) {
      clip.push((el.textContent || '').replace(/\\s+/g, ' ').trim().slice(0, 24) + '+' + (el.scrollWidth - el.clientWidth));
    }
  }
  if (clip.length) problems.push('文字裁切: ' + clip.slice(0, 4).join(' | '));
  return { present: true, problems, w: Math.round(r.width), h: Math.round(r.height) };
})()`;

const LANGS = ['简体中文', '繁體中文', 'English', '日本語'];
const SIZES = [
  [1280, 800],
  [900, 560],
];

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
    if (r.exceptionDetails) throw new Error(JSON.stringify(r.exceptionDetails).slice(0, 200));
    return r.result.value;
  };
  const clickText = (sel, text) => ev(`(() => { const el=[...document.querySelectorAll('${sel}')].find(x=>(x.textContent||'').trim().includes(${JSON.stringify(text)})); if(!el) return false; el.click(); return true; })()`);
  const esc = async () => {
    await ev(`document.dispatchEvent(new KeyboardEvent('keydown',{key:'Escape',bubbles:true,cancelable:true})); true`);
    await sleep(220);
  };

  const failures = [];
  for (const lang of LANGS) {
    for (const [w, h] of SIZES) {
      await S('Emulation.setDeviceMetricsOverride', { width: w, height: h, deviceScaleFactor: 1.5, mobile: false });
      await S('Page.navigate', { url: URL_APP + '?t=' + Date.now() });
      await sleep(2600);

      // 切换语言
      await ev(`document.dispatchEvent(new KeyboardEvent('keydown',{key:',',ctrlKey:true,bubbles:true,cancelable:true})); true`);
      await sleep(500);
      const switched = await clickText('.sheet .pill', lang);
      await sleep(600);
      if (!switched) failures.push({ lang, size: `${w}x${h}`, where: '语言切换', problems: ['未找到语言 pill'] });
      // 校验语言真的生效：设置标题文案应随语言变化
      const titleAfter = await ev(`document.querySelector('.sheet .sheethead h3')?.textContent.trim() || ''`);
      const expected = { '简体中文': '设置', '繁體中文': '設定', 'English': 'Settings', '日本語': '設定' }[lang];
      if (expected && titleAfter && !titleAfter.includes(expected)) {
        failures.push({ lang, size: `${w}x${h}`, where: '语言切换', problems: [`标题仍为「${titleAfter}」，期望含「${expected}」`] });
      }
      await esc();

      const flows = [
        ['设置', async () => { await ev(`document.dispatchEvent(new KeyboardEvent('keydown',{key:',',ctrlKey:true,bubbles:true,cancelable:true})); true`); }],
        ['覆盖表', async () => { await ev(`(() => { const b=[...document.querySelectorAll('.sidefoot button')][2]; if(b) b.click(); return true; })()`); }],
        ['导入', async () => { await ev(`(() => { const b=[...document.querySelectorAll('.sidefoot button')][1]; if(b) b.click(); return true; })()`); }],
        ['备份', async () => { await ev(`(() => { const b=[...document.querySelectorAll('.sidefoot button')][0]; if(b) b.click(); return true; })()`); }],
        ['命令面板', async () => { await ev(`document.dispatchEvent(new KeyboardEvent('keydown',{key:'k',ctrlKey:true,bubbles:true,cancelable:true})); true`); }],
        ['连接信息', async () => { await ev(`(() => { const b=[...document.querySelectorAll('.statusbar button')].pop(); if(b) b.click(); return true; })()`); }],
        ['编码选择', async () => { await ev(`(() => { const b=document.querySelector('.encbtn'); if(b) b.click(); return true; })()`); }],
        ['新建主机', async () => { await ev(`document.dispatchEvent(new KeyboardEvent('keydown',{key:'n',ctrlKey:true,bubbles:true,cancelable:true})); true`); }],
      ];

      for (const [name, open] of flows) {
        const before = c.errors.length;
        await open();
        await sleep(650);
        const res = await ev(CHECK_OVERLAY);
        const newErrors = c.errors.slice(before);
        // present=false 说明流程根本没打开任何覆盖层——这本身是缺陷
        if (!res.present) failures.push({ lang, size: `${w}x${h}`, where: name, problems: ['未打开任何覆盖层'] });
        if (res.problems?.length) failures.push({ lang, size: `${w}x${h}`, where: name, problems: res.problems });
        if (newErrors.length) failures.push({ lang, size: `${w}x${h}`, where: name, problems: newErrors });
        await esc();
        await esc();
      }

      // 主机右键菜单
      const before = c.errors.length;
      await ev(`(() => { const el=document.querySelector('.hostitem'); if(!el) return false; const r=el.getBoundingClientRect(); el.dispatchEvent(new MouseEvent('contextmenu',{bubbles:true,clientX:r.x+8,clientY:r.y+8})); return true; })()`);
      await sleep(500);
      const menu = await ev(CHECK_OVERLAY);
      if (!menu.present) failures.push({ lang, size: `${w}x${h}`, where: '主机右键菜单', problems: ['菜单未打开'] });
      if (menu.problems?.length) failures.push({ lang, size: `${w}x${h}`, where: '主机右键菜单', problems: menu.problems });
      if (c.errors.length > before) failures.push({ lang, size: `${w}x${h}`, where: '主机右键菜单', problems: c.errors.slice(before) });
      await esc();
    }
  }

  console.log(`\n扫描完成：${LANGS.length} 语言 × ${SIZES.length} 尺寸 × 9 覆盖层 = ${LANGS.length * SIZES.length * 9} 次检查`);
  console.log(`失败：${failures.length}`);
  for (const f of failures) console.log(`  [${f.lang} ${f.size}] ${f.where}: ${f.problems.join('; ')}`);
  c.ws.close();
  process.exit(failures.length ? 1 : 0);
};

main().catch((e) => {
  console.error('SWEEP_FAILED', e?.stack || e);
  process.exit(2);
});
