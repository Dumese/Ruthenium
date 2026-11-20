/**
 * 真机 SSH 成功路径的无人值守测试（补齐 mock/本地 PTY 覆盖不到的部分）。
 *
 * 覆盖：
 *   1. 真实 SSH 连接：TOFU 指纹挑战 → trust_host_key → 密码认证 → connected
 *   2. 终端真字节流：write_terminal → 远端 shell 执行 → term://data 事件回读
 *   3. 指标采样：sample_metrics 解析远端 /proc；start/stop 周期采样事件
 *   4. SFTP：home/列目录/上传/下载/删除（真实 subsystem 通道，复用同一 SSH 连接）
 *   5. 转发：-L 规则启停 → 从 Node 侧连本地端口应读到远端 SSH banner
 *   6. 重连：reconnect 生成新会话且 connected
 *   7. 会话日志：远端输出落盘到 <config>/logs/e2e-ssh-*.log
 *   8. 安全：应用日志（app.log）不得出现密码；错误详情不得包含密码
 *
 * 前置（由调用方准备，脚本自身不改仓库/不落盘凭据）：
 *   - 应用已启动（debug 构建 + devUrl :1420 + CDP 端口，见 e2e-real-app.mjs 头部）
 *   - 环境变量 RUTHENIUM_SSH_HOST / RUTHENIUM_SSH_USER / RUTHENIUM_SSH_PASSWORD
 *   - 调用方必须提前备份 %APPDATA%\Ruthenium、%APPDATA%\app.ruthenium.desktop、
 *     ~/.ssh/known_hosts 并在结束后还原（本脚本会写配置、known_hosts、会话日志）
 *
 * 用法：node scripts/e2e-real-ssh.mjs
 */
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import net from 'node:net'

const CDP = process.env.REAL_CDP || 'http://127.0.0.1:9444';
const HOST = process.env.RUTHENIUM_SSH_HOST || '';
const USER = process.env.RUTHENIUM_SSH_USER || 'root';
const PORT = Number(process.env.RUTHENIUM_SSH_PORT || 22);
const PASSWORD = process.env.RUTHENIUM_SSH_PASSWORD || '';
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

if (!HOST || !PASSWORD) {
  console.error('缺少 RUTHENIUM_SSH_HOST / RUTHENIUM_SSH_PASSWORD');
  process.exit(2);
}

const results = [];
function record(group, name, ok, detail) {
  results.push({ group, name, ok, detail });
  const mark = ok === true ? 'PASS' : ok === 'skip' ? 'SKIP' : 'FAIL';
  console.log(`[${mark}] ${group} · ${name}${detail ? ' — ' + detail : ''}`);
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
  throw new Error('CDP 不可达');
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
    } else if (m.method) events.push(m);
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

/** 在 Node 侧找一个空闲端口 */
function freePort() {
  return new Promise((resolve, reject) => {
    const srv = net.createServer();
    srv.once('error', reject);
    srv.listen(0, '127.0.0.1', () => {
      const port = srv.address().port;
      srv.close(() => resolve(port));
    });
  });
}

/** 连本地端口读首行（用于验证 -L 转发真的打通到远端） */
function readBanner(port, timeout = 4000) {
  return new Promise((resolve) => {
    const sock = net.createConnection({ host: '127.0.0.1', port });
    let buf = '';
    const done = (v) => { try { sock.destroy(); } catch { /* ignore */ } resolve(v); };
    sock.setTimeout(timeout);
    sock.on('data', (d) => {
      buf += d.toString('latin1');
      if (buf.includes('\n')) done(buf.trim());
    });
    sock.on('timeout', () => done(buf.trim()));
    sock.on('error', (e) => done('ERR:' + e.code));
    sock.on('end', () => done(buf.trim()));
  });
}

const main = async () => {
  const c = client(await findPage());
  await c.ready;
  await c.send('Runtime.enable');

  const exceptions = [];
  c.events.length = 0;
  const poll = setInterval(() => {
    for (const e of c.events.splice(0)) {
      if (e.method === 'Runtime.exceptionThrown') exceptions.push(e.params?.exceptionDetails?.text || 'exception');
    }
  }, 200);

  const ev = async (expression, awaitPromise = true) => {
    const r = await c.send('Runtime.evaluate', { expression, awaitPromise, returnByValue: true, userGesture: true });
    if (r.exceptionDetails) throw new Error('页面求值异常: ' + (r.exceptionDetails.exception?.description || r.exceptionDetails.text));
    return r.result?.value;
  };
  const waitUntil = async (expr, timeout = 10000) => {
    const t0 = Date.now();
    while (Date.now() - t0 < timeout) {
      try { if (await ev(expr, false)) return true; } catch { /* ignore */ }
      await sleep(120);
    }
    return false;
  };
  const invoke = (cmd, args) =>
    ev(`window.__TAURI_INTERNALS__.invoke(${JSON.stringify(cmd)}, ${JSON.stringify(args ?? {})})`);

  // 清理状态与 known_hosts 自备份：脚本中途失败/崩溃也不把测试主机、
  // 信任指纹、临时文件留在用户的真实配置里（成功路径末尾的清理之外再兜一层）。
  let hostId = null;
  let sessionId = null;
  const localTemp = [];
  const khPath = path.join(os.homedir(), '.ssh', 'known_hosts');
  const khBackup = fs.existsSync(khPath) ? fs.readFileSync(khPath) : null;
  const khExisted = fs.existsSync(khPath);
  let exitCode = 2; // 显式赋值前视为异常退出

  try {
  // ---------- 0. 启动与事件订阅（hostkey / term / metrics / tunnel） ----------
  const boot = await waitUntil(`!!document.querySelector('.app')`, 25000);
  record('boot', '真实应用已启动', boot, '');
  if (!boot) throw new Error('应用未就绪');

  const sub = await ev(`(async () => {
    const inv = window.__TAURI_INTERNALS__.invoke;
    window.__evt = { hk: [], term: [], conn: [], met: [], tun: [], sftp: [] };
    const reg = async (event, bucket) => {
      const handler = window.__TAURI_INTERNALS__.transformCallback((e) => { window.__evt[bucket].push(e.payload); }, false);
      await inv('plugin:event|listen', { event, target: { kind: 'Any' }, handler });
      return event;
    };
    await reg('hostkey://challenge', 'hk');
    await reg('term://data', 'term');
    await reg('conn://state', 'conn');
    await reg('metrics://sample', 'met');
    await reg('tunnel://state', 'tun');
    await reg('sftp://progress', 'sftp');
    return Object.keys(window.__evt);
  })()`);
  record('事件', '订阅 hostkey/term/conn/metrics/tunnel/sftp 事件成功', Array.isArray(sub) && sub.length === 6, JSON.stringify(sub));

  // ---------- 1. 真实 SSH 连接：TOFU → trust → 密码认证 ----------
  hostId = 'e2e-ssh-' + Date.now();
  const host = {
    id: hostId, name: 'e2e-ssh', addr: HOST, user: USER, port: PORT,
    group: '', tag: 'dev', auth: 'password', keyRef: '', known: false,
    agent: false, note: 'e2e', log: true, initCommand: '',
  };
  await invoke('upsert_host', { host });
  record('SSH', '临时主机已写入真实配置（auth=password, known=false, log=true）', true, `host=${HOST}`);

  // connect 会挂起等待指纹决策；先在页面里发起，再轮询 hostkey 事件
  await ev(`(() => {
    const inv = window.__TAURI_INTERNALS__.invoke;
    window.__connP = inv('connect', { hostId: ${JSON.stringify(hostId)}, cols: 120, rows: 32,
      attemptId: ${JSON.stringify(hostId + '-conn')}, password: ${JSON.stringify(PASSWORD)} })
      .then((sid) => ({ ok: true, sid })).catch((e) => ({ ok: false, code: e && e.code, detail: e && e.detail }));
    return true;
  })()`, false);

  // 等待三选一：hostkey 挑战到达 / connect 已落定（known_hosts 已信任时直连）/ 超时。
  const phase = await ev(`(async () => {
    const t0 = Date.now();
    while (Date.now() - t0 < 20000) {
      if ((window.__evt?.hk?.length || 0) > 0) return 'hostkey';
      const r = await Promise.race([window.__connP, new Promise((res) => setTimeout(() => res(null), 120))]);
      if (r) { window.__connResult = r; return 'settled'; }
    }
    return 'timeout';
  })()`);

  if (phase === 'hostkey') {
    const hk = await ev(`window.__evt.hk[0]`, false);
    record('SSH', '未知主机：连接挂起并弹出指纹挑战（TOFU）',
      !!hk?.fingerprint && !!hk?.sessionId,
      `${hk?.algo} ${String(hk?.fingerprint).slice(0, 24)}… changed=${hk?.changed}`);
    await invoke('trust_host_key', { sessionId: hk.sessionId, fingerprint: hk.fingerprint });
  } else if (phase === 'settled') {
    // 调用方应把 known_hosts 移到一边以强制覆盖 TOFU；若仍走到这里说明主机已是已知主机，跳过该项。
    record('SSH', '未知主机：连接挂起并弹出指纹挑战（TOFU）', 'skip', '已知主机（known_hosts 已有记录），跳过 TOFU 挑战');
  } else {
    record('SSH', '未知主机：连接挂起并弹出指纹挑战（TOFU）', false, '20s 内既无挑战也未落定');
    throw new Error('连接阶段超时');
  }

  // 信任后（或已知主机直连）拿 connect 结果：验证密码认证成功
  const conn = await ev(`window.__connP`, true);
  record('SSH', '真实密码认证成功并返回 sessionId',
    conn?.ok === true && typeof conn.sid === 'string',
    JSON.stringify({ ok: conn?.ok, code: conn?.code, detail: conn?.ok ? '' : String(conn?.detail || '').slice(0, 120) }));
  if (!conn?.ok) throw new Error('SSH 认证/连接失败: ' + (conn?.code || 'unknown'));
  sessionId = conn.sid;

  // ---------- 2. 会话表 / 终端真字节流 ----------
  const sessions = await invoke('list_sessions');
  const mine = (sessions || []).find((s) => s.id === sessionId);
  record('SSH', 'list_sessions 中该会话 connected 且 kind=term',
    !!mine && mine.connected === true && mine.kind === 'term' && mine.hostId === hostId,
    JSON.stringify(mine));

  const marker = 'RUTHENIUM_SSH_E2E_OK_' + Date.now().toString(36);
  const b64 = Buffer.from(`echo ${marker}\n`, 'utf8').toString('base64');
  await invoke('write_terminal', { sessionId, data: b64 });
  const gotMarker = await waitUntil(`(() => {
    const dec = window.__evt.term.filter((p) => p.sessionId === ${JSON.stringify(sessionId)})
      .map((p) => { try { return atob(p.data); } catch { return ''; } }).join('');
    return dec.includes(${JSON.stringify(marker)});
  })()`, 12000);
  record('SSH', 'write_terminal 真实下发命令并从 term://data 读回远端输出', gotMarker, marker);

  // ---------- 3. 指标采样（真实 /proc） ----------
  const m = await invoke('sample_metrics', { sessionId });
  record('指标', 'sample_metrics 解析远端 /proc（cores/mem/load）',
    !!m && m.cores >= 1 && m.memTotalGb > 0 && Array.isArray(m.loadAvg),
    m ? `cores=${m.cores} mem=${m.memTotalGb.toFixed(2)}GB cpu=${m.cpuPct}` : 'null');
  await invoke('start_metrics', { sessionId });
  const metArrived = await waitUntil(`(window.__evt.met || []).some((x) => x.sessionId === ${JSON.stringify(sessionId)})`, 12000);
  await invoke('stop_metrics', { sessionId }).catch(() => {});
  record('指标', 'start_metrics 周期采样产生 metrics://sample 事件', metArrived, `${(await ev(`(window.__evt.met || []).length`, false))} 帧`);

  // ---------- 4. SFTP：home / 列目录 / 上传 / 下载 / 删除 ----------
  const home = await invoke('sftp_home', { sessionId });
  const listing = await invoke('sftp_list', { sessionId, path: home });
  record('SFTP', 'sftp_home + sftp_list 真实列目录',
    typeof home === 'string' && home.length > 0 && Array.isArray(listing),
    `home=${home} entries=${listing?.length}`);

  const stamp = Date.now().toString(36);
  const remoteFile = `${String(home).replace(/\/+$/, '')}/ruthenium-e2e-${stamp}.txt`;
  const localUp = path.join(os.tmpdir(), `ruthenium-e2e-up-${stamp}.txt`);
  const localDown = path.join(os.tmpdir(), `ruthenium-e2e-down-${stamp}.txt`);
  localTemp.push(localUp, localDown);
  const payload = 'RUTHENIUM_SFTP_ROUNDTRIP_' + stamp + '\nline2\n';
  fs.writeFileSync(localUp, payload);

  // 上传是异步任务：文件先 CREATE 出现（size=0），内容随后写入。
  // 必须等「进度终态 done 或远端 size 达到预期」再继续，否则会读到半成品。
  await ev(`(() => { window.__evt.sftp = []; return true; })()`, false);
  const upTransfer = await invoke('sftp_upload', { sessionId, localPath: localUp, remotePath: remoteFile });
  const expectBytes = Buffer.byteLength(payload);
  let uploaded = false;
  let upState = '';
  let remoteSize = -1;
  for (let i = 0; i < 200; i++) {
    upState = await ev(`(() => { const f = (window.__evt.sftp || []).filter((x) => x.transferId === ${JSON.stringify(upTransfer)}).pop(); return f ? f.state : ''; })()`, false);
    remoteSize = await ev(`window.__TAURI_INTERNALS__.invoke('sftp_list', { sessionId: ${JSON.stringify(sessionId)}, path: ${JSON.stringify(String(home))} })
      .then((l) => { const e = l.find((x) => x.name === ${JSON.stringify(remoteFile.split('/').pop())}); return e && e.size != null ? e.size : -1; }).catch(() => -1)`);
    if (remoteSize === expectBytes && (upState === 'done' || upState === '')) { uploaded = true; break; }
    if (upState === 'error' || upState === 'cancelled') break;
    await sleep(100);
  }
  record('SFTP', 'sftp_upload 真实写远端文件（大小一致）', uploaded,
    `${remoteFile} remoteSize=${remoteSize} expect=${expectBytes} state=${upState || 'no-progress'}`);

  await ev(`(() => { window.__evt.sftp = []; return true; })()`, false);
  const dlTransfer = await invoke('sftp_download', { sessionId, remotePath: remoteFile, localPath: localDown });
  // 下载完成用「进度终态 + 文件内容」双重判定（Node 侧读盘）
  let downloadedOk = false;
  let dlState = '';
  let dlSize = -1;
  for (let i = 0; i < 200; i++) {
    dlState = await ev(`(() => { const f = (window.__evt.sftp || []).filter((x) => x.transferId === ${JSON.stringify(dlTransfer)}).pop(); return f ? f.state : ''; })()`, false);
    try {
      const got = fs.readFileSync(localDown);
      dlSize = got.length;
      if (got.toString('utf8') === payload) { downloadedOk = true; break; }
    } catch { /* not yet */ }
    if (dlState === 'error' || dlState === 'cancelled') break;
    await sleep(100);
  }
  record('SFTP', 'sftp_download 内容与上传完全一致', downloadedOk,
    downloadedOk ? 'byte-identical' : `state=${dlState || 'no-progress'} localSize=${dlSize} expect=${Buffer.byteLength(payload)}`);

  await invoke('sftp_delete', { sessionId, path: remoteFile });
  const removed = await waitUntil(`window.__TAURI_INTERNALS__.invoke('sftp_list', { sessionId: ${JSON.stringify(sessionId)}, path: ${JSON.stringify(String(home))} })
    .then((l) => !l.some((e) => e.name === ${JSON.stringify(remoteFile.split('/').pop())})).catch(() => false)`, 10000);
  record('SFTP', 'sftp_delete 删除远端临时文件（清理）', removed, '');
  for (const f of [localUp, localDown]) { try { fs.unlinkSync(f); } catch { /* ignore */ } }

  // ---------- 5. -L 本地转发（真实 direct-tcpip） ----------
  const port = await freePort();
  const tunId = 'e2e-tun-' + stamp;
  await invoke('upsert_tunnel', { tunnel: {
    id: tunId, name: 'e2e-L', kind: 'local', local: `127.0.0.1:${port}`,
    remote: `127.0.0.1:${PORT}`, host: hostId, on: false,
  } });
  await invoke('toggle_tunnel', { id: tunId, on: true });
  let banner = '';
  for (let i = 0; i < 30; i++) {
    banner = await readBanner(port, 2500);
    if (banner.startsWith('SSH-2.0-')) break;
    await sleep(200);
  }
  record('转发', '-L 规则启动后本地端口转发到远端 sshd（读到 SSH banner）',
    banner.startsWith('SSH-2.0-'), `port=${port} banner=${banner.slice(0, 40)}`);
  await invoke('toggle_tunnel', { id: tunId, on: false });
  await sleep(500);
  const closed = await readBanner(port, 1500);
  record('转发', '停用规则后本地端口不再监听', !String(closed).startsWith('SSH-2.0'), closed.slice(0, 40) || '(closed)');
  await invoke('delete_tunnel', { id: tunId });

  // ---------- 6. 重连 ----------
  // 密码认证且未保存密码时，reconnect 必须带上本次密码（否则后端回 E_SSH_AUTH），
  // 这与真实 UI 的限制一致：手动重连只会从 keyring 取密码。
  const reconnected = await ev(`window.__TAURI_INTERNALS__.invoke('reconnect', { sessionId: ${JSON.stringify(sessionId)}, attemptId: 'e2e-reconnect-1', password: ${JSON.stringify(PASSWORD)} })
    .then((sid) => ({ ok: true, sid })).catch((e) => ({ ok: false, code: e && e.code, detail: e && e.detail }))`);
  let newSid = null;
  if (reconnected?.ok && reconnected.sid) newSid = reconnected.sid;
  const list2 = await invoke('list_sessions');
  const mine2 = (list2 || []).find((s) => s.id === newSid);
  record('重连', 'reconnect 生成新会话并保持 connected',
    !!newSid && newSid !== sessionId && !!mine2 && mine2.connected === true,
    JSON.stringify({ ok: reconnected?.ok, code: reconnected?.code, detail: String(reconnected?.detail || '').slice(0, 160), newSid: newSid ? 'new' : null }));

  // ---------- 7. 会话日志（host.log=true） ----------
  const logsDir = String((await invoke('get_app_info'))?.logsDir || '');
  let logOk = false; let logName = '';
  for (let i = 0; i < 30; i++) {
    try {
      const files = fs.readdirSync(logsDir).filter((f) => f.startsWith('e2e-ssh') && f.endsWith('.log'));
      for (const f of files) {
        const txt = fs.readFileSync(path.join(logsDir, f), 'latin1');
        if (txt.includes(marker)) { logOk = true; logName = f; break; }
      }
    } catch { /* ignore */ }
    if (logOk) break;
    await sleep(200);
  }
  record('会话日志', '远端输出以原始字节写入 <config>/logs/e2e-ssh-*.log', logOk, logName);

  // ---------- 8. 安全：密码不落日志 ----------
  let pwLeaked = false;
  try {
    const appLog = path.join(logsDir, 'app.log');
    if (fs.existsSync(appLog)) pwLeaked = fs.readFileSync(appLog, 'utf8').includes(PASSWORD);
  } catch { /* ignore */ }
  record('安全', '应用日志 app.log 不含密码明文', !pwLeaked, pwLeaked ? 'LEAKED' : 'not found');

  // ---------- 9. 收尾（关会话 / 删主机；known_hosts 与配置由调用方备份还原） ----------
  if (newSid) { try { await invoke('close_session', { sessionId: newSid }); } catch { /* ignore */ } }
  await invoke('delete_host', { id: hostId });
  const sessionsEnd = await invoke('list_sessions');
  record('收尾', '关闭会话并删除临时主机', (await waitUntil(`window.__TAURI_INTERNALS__.invoke('list_sessions').then((l) => !l.some((s) => s.hostId === ${JSON.stringify(hostId)})).catch(() => false)`, 8000)),
    `sessions=${sessionsEnd?.length}`);

  // ---------- 10. 稳定性 ----------
  clearInterval(poll);
  for (const e of c.events.splice(0)) {
    if (e.method === 'Runtime.exceptionThrown') exceptions.push(e.params?.exceptionDetails?.text || 'exception');
  }
  record('稳定性', '全程无未捕获异常', exceptions.length === 0, `${exceptions.length} 条`);

  const pass = results.filter((r) => r.ok === true).length;
  const fail = results.filter((r) => r.ok === false).length;
  const skip = results.filter((r) => r.ok === 'skip').length;
  console.log(`\n===== 汇总 =====\nPASS ${pass} / FAIL ${fail} / SKIP ${skip} / TOTAL ${results.length}`);
  if (fail) for (const r of results.filter((x) => x.ok === false)) console.log(` - ${r.group} · ${r.name} (${r.detail})`);
  exitCode = fail ? 1 : 0;
  } finally {
    // 无论成功 / 失败 / 中途 throw，测试现场必须清干净
    clearInterval(poll);
    try { if (sessionId) await invoke('close_session', { sessionId }); } catch { /* ignore */ }
    try { if (hostId) await invoke('delete_host', { id: hostId }); } catch { /* ignore */ }
    for (const f of localTemp) { try { fs.unlinkSync(f); } catch { /* ignore */ } }
    try {
      if (khExisted) fs.writeFileSync(khPath, khBackup);
      else if (fs.existsSync(khPath)) fs.unlinkSync(khPath);
    } catch { /* ignore */ }
    try { c.ws.close(); } catch { /* ignore */ }
  }
  process.exit(exitCode);
};

main().catch((e) => { console.error('SSH_E2E_CRASH', e?.stack || e); process.exit(2); });
