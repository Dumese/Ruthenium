/**
 * 真机（Tauri / WebView2）无人值守功能测试 —— 直接驱动**真实应用**的 IPC 与本地 PTY。
 *
 * 与 `e2e-ui.mjs`（浏览器 + mock）互补：这里走真正的 Rust 后端（命令注册、参数命名、
 * ConPTY、事件链路、配置落盘），覆盖 mock 无法验证的部分。
 *
 * 前置：
 *   1) 构建并启动应用（CDP 端口固定 9444）：
 *      $env:RUTHENIUM_LOW_END='1'   # 强制 DOM 渲染器，便于读 .xterm-rows 文本断言
 *      $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS='--remote-debugging-port=9444'
 *      src-tauri\target\debug\Ruthenium.exe
 *   2) node scripts/e2e-real-app.mjs
 *
 * 注意：本脚本会调用 upsert_host / save_preferences / clear_session_logs，
 * 会改动真实配置；调用方必须提前备份 %APPDATA%\Ruthenium 并在结束后还原。
 */
const CDP = process.env.REAL_CDP || 'http://127.0.0.1:9444';
const results = [];
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function record(group, name, ok, detail) {
  results.push({ group, name, ok, detail });
  console.log(`[${ok === true ? 'PASS' : ok === 'skip' ? 'SKIP' : 'FAIL'}] ${group} · ${name}${detail ? ' — ' + detail : ''}`);
}

async function findPage() {
  for (let i = 0; i < 120; i++) {
    try {
      const list = await (await fetch(CDP + '/json/list')).json();
      const page = list.find((t) => t.type === 'page' && t.webSocketDebuggerUrl);
      if (page) return page.webSocketDebuggerUrl;
    } catch { /* retry */ }
    await sleep(250);
  }
  throw new Error('CDP /json/list 不可达（应用是否带 --remote-debugging-port 启动？）');
}

function client(url) {
  const ws = new WebSocket(url);
  let id = 0;
  const pending = new Map();
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
  const send = (method, params = {}) =>
    new Promise((resolve, reject) => {
      const mid = ++id;
      pending.set(mid, { resolve, reject });
      ws.send(JSON.stringify({ id: mid, method, params }));
    });
  return { ws, ready, send, events };
}

const main = async () => {
  const c = client(await findPage());
  await c.ready;
  await c.send('Runtime.enable');
  await c.send('Page.enable');

  const exceptions = [];
  c.events.length = 0; // 丢弃 attach 前的事件，避免统计到历史
  const exceptionPoll = setInterval(() => {
    for (const e of c.events.splice(0)) {
      if (e.method === 'Runtime.exceptionThrown') exceptions.push(e.params?.exceptionDetails?.text || 'exception');
    }
  }, 200);

  /** 页面内求值（支持 awaitPromise / 返回对象） */
  const ev = async (expression, awaitPromise = true) => {
    const r = await c.send('Runtime.evaluate', {
      expression, awaitPromise, returnByValue: true, userGesture: true,
    });
    if (r.exceptionDetails) {
      throw new Error('页面求值异常: ' + (r.exceptionDetails.exception?.description || r.exceptionDetails.text));
    }
    return r.result?.value;
  };

  const waitUntil = async (expr, timeout = 8000) => {
    const t0 = Date.now();
    while (Date.now() - t0 < timeout) {
      try { if (await ev(expr, false)) return true; } catch { /* ignore */ }
      await sleep(120);
    }
    return false;
  };

  const key = (k, mods = {}) =>
    ev(`(() => { const t = document.activeElement || document.body;
      t.dispatchEvent(new KeyboardEvent('keydown', Object.assign({ key: ${JSON.stringify(k)}, bubbles: true, cancelable: true }, ${JSON.stringify(mods)})));
      return true; })()`, false);

  const invoke = (cmd, args) =>
    ev(`window.__TAURI_INTERNALS__.invoke(${JSON.stringify(cmd)}, ${JSON.stringify(args ?? {})})`);

  // ---------- 1. 启动 / 环境 ----------
  // 注意：debug 构建按 tauri.conf 的 devUrl 加载（需要 vite dev server :1420）；
  // 这里只断言 React 已挂载（.app + 主布局），状态栏受 showStatus 偏好影响不强求。
  const boot = await waitUntil(`!!document.querySelector('.app') && !!document.querySelector('.bodyrow')`, 20000);
  record('boot', '真实 Tauri 应用启动并完成首帧', boot, boot ? '' : '未找到 .app/.bodyrow');
  const tauriEnv = await ev(`({ internal: !!window.__TAURI_INTERNALS__, mock: !window.__TAURI_INTERNALS__ })`, false);
  record('boot', '运行在真实 IPC 层（非 mock）', tauriEnv.internal && !tauriEnv.mock, JSON.stringify(tauriEnv));

  // ---------- 2. IPC 契约（命令注册 / 参数命名） ----------
  const info = await invoke('get_app_info');
  record('IPC', 'get_app_info 返回 version/configDir/logsDir',
    !!info && !!info.version && !!info.configDir && !!info.logsDir && /logs$/i.test(String(info.logsDir)),
    JSON.stringify(info));

  const cfg = await invoke('load_config');
  record('IPC', 'load_config 返回 version=1 的配置快照',
    !!cfg && cfg.version === 1 && !!cfg.preferences && Array.isArray(cfg.hosts),
    `hosts=${cfg?.hosts?.length} prefers=${Object.keys(cfg?.preferences || {}).length}`);

  // cancel_connect 的入参是 camelCase attemptId → 验证 Tauri 参数映射真的生效
  const cancelledNoop = await invoke('cancel_connect', { attemptId: 'e2e-no-such-attempt' });
  record('IPC', 'cancel_connect(attemptId) 参数映射正确（不存在时返回 false）', cancelledNoop === false, String(cancelledNoop));

  const cleared0 = await invoke('clear_session_logs');
  record('IPC', 'clear_session_logs 返回删除数量（数字）', typeof cleared0 === 'number', String(cleared0));

  // ---------- 3. 真实在途连接可取消（黑洞地址） ----------
  const cancelProbe = await ev(`(async () => {
    const inv = window.__TAURI_INTERNALS__.invoke;
    const host = { id: 'e2e-cancel-host', name: 'e2e-cancel', addr: '192.0.2.1', user: 'e2e', port: 22,
      group: '', tag: 'dev', auth: 'key', keyRef: '', known: true, agent: false, note: '', log: false, initCommand: '' };
    await inv('upsert_host', { host });
    const p = inv('connect', { hostId: host.id, cols: 80, rows: 24, attemptId: 'e2e-attempt-1' })
      .then(() => ({ ok: true })).catch((e) => ({ ok: false, code: e && e.code, detail: e && e.detail }));
    await new Promise((r) => setTimeout(r, 500));
    const cancelled = await inv('cancel_connect', { attemptId: 'e2e-attempt-1' });
    const res = await Promise.race([p, new Promise((r) => setTimeout(() => r({ ok: false, code: 'TIMEOUT' }), 8000))]);
    await inv('delete_host', { id: host.id });
    return { cancelled, res };
  })()`);
  record('取消连接', '关闭 connecting 标签可真正中断在途握手（E_CANCELLED）',
    cancelProbe?.cancelled === true && cancelProbe?.res?.code === 'E_CANCELLED',
    JSON.stringify(cancelProbe));

  // ---------- 4. 本地终端（真实 ConPTY → term://data → xterm） ----------
  const tabsBefore = await ev(`document.querySelectorAll('.tabbar .tab').length`, false);
  await ev(`document.querySelector('.tabhost,.tabbar,.termwrap,.stage')?.click?.()`).catch(() => {});
  await key('t', { ctrlKey: true });
  const termMounted = await waitUntil(`!!document.querySelector('.termhost .xterm')`, 8000);
  record('本地终端', 'Ctrl+T 创建本地 PTY 会话并挂载 xterm', termMounted, '');

  const connectedTab = await waitUntil(`[...document.querySelectorAll('.tabbar .tab')].some(t => t.dataset.status === 'connected')`, 8000);
  record('本地终端', '会话状态进入 connected', connectedTab, `tabs ${tabsBefore}→${await ev(`document.querySelectorAll('.tabbar .tab').length`, false)}`);

  const canvasInfo = await ev(`({ canvas: document.querySelectorAll('.termhost canvas').length, rows: !!document.querySelector('.termhost .xterm-rows') })`, false);
  // 点进终端并输入（真实键盘事件 → xterm onData → write_terminal → ConPTY 回显）
  await ev(`(() => { const el = document.querySelector('.termhost'); const r = el.getBoundingClientRect();
    el.dispatchEvent(new MouseEvent('mousedown', { bubbles: true, clientX: r.x + r.width / 2, clientY: r.y + r.height / 2 }));
    el.dispatchEvent(new MouseEvent('mouseup', { bubbles: true, clientX: r.x + r.width / 2, clientY: r.y + r.height / 2 }));
    const ta = document.querySelector('.termhost .xterm-helper-textarea'); ta && ta.focus();
    return true; })()`, false);
  await sleep(300);
  const typeText = async (text) => {
    for (const ch of text) {
      await c.send('Input.dispatchKeyEvent', { type: 'keyDown', text: ch, unmodifiedText: ch, key: ch });
      await c.send('Input.dispatchKeyEvent', { type: 'keyUp', key: ch });
      await sleep(12);
    }
  };
  await typeText('echo RUTHENIUM_REAL_E2E_OK');
  await c.send('Input.dispatchKeyEvent', { type: 'keyDown', text: '\r', unmodifiedText: '\r', key: 'Enter', code: 'Enter', windowsVirtualKeyCode: 13 });
  await c.send('Input.dispatchKeyEvent', { type: 'keyUp', key: 'Enter', code: 'Enter', windowsVirtualKeyCode: 13 });
  const echoed = await waitUntil(`(document.querySelector('.termhost .xterm-rows')?.innerText || '').includes('RUTHENIUM_REAL_E2E_OK')`, 8000);
  if (canvasInfo.canvas === 0) {
    record('本地终端', '键入命令并读到 PTY 回显（DOM 渲染器读 .xterm-rows）', echoed,
      `canvas=${canvasInfo.canvas} rows=${String(await ev(`(document.querySelector('.termhost .xterm-rows')?.innerText || '').slice(0, 80)`, false)).replace(/\n/g, '\\n')}`);
  } else {
    record('本地终端', '键入命令并读到 PTY 回显', 'skip', `WebGL 渲染器（canvas=${canvasInfo.canvas}），无法读 DOM 文本`);
  }

  // ---------- 5. 设置面板的日志目录 / 明文提示 / 清空入口（真实 IPC） ----------
  await key(',', { ctrlKey: true });
  const sheet = await waitUntil(`!!document.querySelector('.sheet')`, 5000);
  const logSection = await ev(`(() => {
    const sheet = document.querySelector('.sheet');
    const txt = (sheet?.innerText || '').replace(/\\s+/g, ' ');
    return {
      hasPath: txt.includes('日志目录'),
      hasPrivacy: /明文|不加密/.test(txt),
      hasOpen: txt.includes('打开日志文件夹'),
      hasClear: txt.includes('清空会话日志'),
      // 只统计"语言说明"这类小字；图形加速行的 sub 是设计如此，按浏览器 E2E 同口径排除
      hintCount: sheet ? [...sheet.querySelectorAll('.srs, .hint, .sidehint')]
        .filter((e) => (e.closest('.setrow')?.dataset.setting || '') !== 'gpu').length : -1,
    };
  })()`, false);
  record('设置面板', '设置面板有「日志目录 + 明文说明 + 打开/清空」入口',
    sheet && logSection.hasPath && logSection.hasPrivacy && logSection.hasOpen && logSection.hasClear,
    JSON.stringify(logSection));
  record('设置面板', '设置面板未引入额外副标题（设计：仅语言说明 1 条）', logSection.hintCount <= 1, `hint=${logSection.hintCount}`);

  // 打开全局「记录会话日志」：应弹一次明文风险提示（若用户原本已开启，先关再开以触发提示）
  const toggled = await ev(`(async () => {
    const rows = [...document.querySelectorAll('.sheet .setrow')];
    const row = rows.find((r) => (r.querySelector('.srt')?.textContent || '').includes('记录会话日志'));
    const sw = row && row.querySelector('.switch');
    if (!sw) return { found: false };
    if (sw.getAttribute('aria-checked') === 'true') { sw.click(); await new Promise((r) => setTimeout(r, 350)); }
    sw.click();
    let toast = '';
    for (let i = 0; i < 20; i++) {
      await new Promise((r) => setTimeout(r, 120));
      toast = (document.querySelector('.toast')?.textContent || '').replace(/\\s+/g, ' ').trim();
      if (toast) break;
    }
    const after = sw.getAttribute('aria-checked');
    // 还原为关闭，避免影响后续断言（脚本外整体备份/还原兜底）
    if (sw.getAttribute('aria-checked') === 'true') { sw.click(); await new Promise((r) => setTimeout(r, 200)); }
    return { found: true, after, toast };
  })()`);
  record('设置面板', '开启会话日志会弹出明文写入位置提示', toggled?.found && toggled?.after === 'true' && /明文/.test(toggled?.toast || ''),
    JSON.stringify(toggled));

  // 清空会话日志（确认弹窗 → 真实 clear_session_logs）
  const clearFlow = await ev(`(async () => {
    const btn = [...document.querySelectorAll('.sheet button')].find((b) => (b.textContent || '').includes('清空会话日志'));
    if (!btn) return { found: false };
    btn.click();
    await new Promise((r) => setTimeout(r, 400));
    const confirm = document.querySelector('.modalfoot button.strong');
    const confirmText = (document.querySelector('.modal')?.innerText || '').slice(0, 40);
    if (!confirm) return { found: true, confirm: false };
    confirm.click();
    await new Promise((r) => setTimeout(r, 600));
    const toast = (document.querySelector('.toast')?.textContent || '').trim();
    return { found: true, confirm: true, confirmText, toast };
  })()`);
  record('设置面板', '「清空会话日志」走确认弹窗并提示已清空', !!clearFlow?.confirm && /已清空/.test(clearFlow?.toast || ''),
    JSON.stringify(clearFlow));

  // 还原：关掉日志开关（备份会在脚本外整体还原，这里保证测试期间状态可控）
  await ev(`(async () => {
    const rows = [...document.querySelectorAll('.sheet .setrow')];
    const row = rows.find((r) => (r.querySelector('.srt')?.textContent || '').includes('记录会话日志'));
    const sw = row && row.querySelector('.switch');
    if (sw && sw.getAttribute('aria-checked') === 'true') sw.click();
    await new Promise((r) => setTimeout(r, 300));
    return true;
  })()`);
  await key('Escape');
  await sleep(300);

  // ---------- 6. 关闭终端标签 + list_sessions 对账 ----------
  await ev(`(() => { const t = document.querySelector('.tabbar .tab'); t && t.click(); return true; })()`, false);
  await sleep(200);
  await key('w', { ctrlKey: true, shiftKey: true });
  // 用户偏好 confirmClose=true 时会先弹确认（真实配置行为），确认后再断言会话清空
  const confirmShown = await waitUntil(`!!document.querySelector('.modal .modalfoot button.strong')`, 1500);
  if (confirmShown) {
    await ev(`(() => { document.querySelector('.modal .modalfoot button.strong')?.click(); return true; })()`, false);
  }
  const tabsGone = await waitUntil(`document.querySelectorAll('.tabbar .tab').length === 0`, 6000);
  const sessions = await invoke('list_sessions');
  record('会话', 'Ctrl+Shift+W 关闭标签且后端会话表清空',
    tabsGone && Array.isArray(sessions) && sessions.length === 0,
    `confirmClose=${confirmShown} tabsGone=${tabsGone} sessions=${sessions?.length} exceptions=${exceptions.length}`);

  // ---------- 7. 稳定性 ----------
  clearInterval(exceptionPoll);
  for (const e of c.events.splice(0)) {
    if (e.method === 'Runtime.exceptionThrown') exceptions.push(e.params?.exceptionDetails?.text || 'exception');
  }
  record('稳定性', '运行期无未捕获异常', exceptions.length === 0, `${exceptions.length} 条${exceptions.length ? ': ' + exceptions.slice(0, 3).join(' | ') : ''}`);

  const pass = results.filter((r) => r.ok === true).length;
  const fail = results.filter((r) => r.ok === false).length;
  const skip = results.filter((r) => r.ok === 'skip').length;
  console.log(`\n===== 汇总 =====\nPASS ${pass} / FAIL ${fail} / SKIP ${skip} / TOTAL ${results.length}`);
  if (fail) for (const r of results.filter((x) => x.ok === false)) console.log(` - ${r.group} · ${r.name} (${r.detail})`);
  c.ws.close();
  process.exit(fail ? 1 : 0);
};

main().catch((e) => { console.error('REAL_E2E_CRASH', e?.stack || e); process.exit(2); });
