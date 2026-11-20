// 无 Rust 后端时的模拟实现。初始数据 HOSTS0/SNIPS0/RULES0/LOCAL0/REMOTE0。
// 仅开发期使用；发布构建在 Tauri 环境下不会走任何 mock 分支。
import type { Config, Host, Snippet, TunnelRule, FileEntry, ParsedHost, HostMetrics } from '../types/models';
import { EV } from '../types/events';
import type { ConnStateEvent, HostkeyChallengeEvent, SftpChangedEvent, SftpProgressEvent, TermDataEvent, TermExitEvent, TunnelStateEvent } from '../types/events';
import { uid } from '../lib/ids';
import { parseSshConfig } from '../lib/sshConfigParser';
import { T } from '../i18n';

export const DEFAULT_FOLDERS = [
  { name: '生产环境', color: 'orange' },
  { name: '预发布', color: 'purple' },
  { name: '开发机', color: 'green' },
  { name: '本地', color: 'blue' },
];

function h(partial: Omit<Host, 'port'|'agent'|'auth'|'initCommand'|'log'> & { port?: number; auth?: Host['auth']; initCommand?: string; log?: boolean }): Host {
  return { port: 22, agent: true, auth: 'key', initCommand: '', log: false, ...partial } as Host;
}
// 初始数据：h3 密码已保存（keyring 有值）、h7 密码未保存（连接时弹框）。
// 是否已保存不是配置字段，由 mock 的 keyring 表派生。
export const HOSTS0: Host[] = [
  h({ id: 'h1', name: 'web-01', addr: 'web-01.example.com', user: 'deploy', group: '生产环境', tag: 'prod', keyRef: '~\\.ssh\\id_ed25519', known: true, note: '主站 Nginx + 应用节点' }),
  h({ id: 'h2', name: 'web-02', addr: 'web-02.example.com', user: 'deploy', group: '生产环境', tag: 'prod', keyRef: '~\\.ssh\\id_ed25519', known: true, note: '主站备节点' }),
  h({ id: 'h3', name: 'db-primary', addr: '10.20.0.11', user: 'dba', port: 2222, group: '生产环境', tag: 'prod', auth: 'password', keyRef: '', known: true, note: 'PostgreSQL 主库，走跳板机' }),
  h({ id: 'h4', name: 'staging-api', addr: 'staging.internal', user: 'dev', group: '预发布', tag: 'test', keyRef: '~\\.ssh\\id_ed25519', known: true, note: '预发布 API' }),
  h({ id: 'h5', name: 'staging-web', addr: 'staging-web.internal', user: 'dev', group: '预发布', tag: 'test', keyRef: '~\\.ssh\\id_ed25519', known: false, note: '首次连接需校验指纹' }),
  h({ id: 'h6', name: 'dev-mbp', addr: '192.168.1.24', user: 'wayne', group: '开发机', tag: 'dev', keyRef: '~\\.ssh\\id_ed25519', known: true, note: '内网开发机' }),
  h({ id: 'h7', name: 'pi-lab', addr: 'pi.lab.local', user: 'pi', group: '开发机', tag: 'dev', auth: 'password', keyRef: '', known: true, note: '树莓派实验环境' }),
  h({ id: 'h8', name: 'localhost', addr: '127.0.0.1', user: 'wayne', port: 0, group: '本地', tag: 'dev', keyRef: '', known: true, note: '本地终端，无需 SSH' }),
];
/** mock 的「凭据管理器」：只有 set_password 写、delete_password 删，绝不进 hosts 配置 */
const SEED_PASSWORDS: Record<string, string> = { h3: 'mock-db-secret' };
export const SNIPS0: Snippet[] = [
  { id: 'n1', name: '查看系统负载', cmd: 'uptime && df -h / && free -h', tag: '诊断' },
  { id: 'n2', name: '跟随 Nginx 日志', cmd: 'tail -f /var/log/nginx/access.log', tag: '日志' },
  { id: 'n3', name: 'Docker 清理', cmd: 'docker system prune -af --volumes', tag: '维护' },
  { id: 'n4', name: 'Git 当前状态', cmd: 'git status --short --branch', tag: '开发' },
  { id: 'n5', name: '查看监听端口', cmd: 'ss -tulpn | head -30', tag: '网络' },
  { id: 'n6', name: '续期证书（演练）', cmd: 'sudo certbot renew --dry-run', tag: '维护' },
];
export const RULES0: TunnelRule[] = [
  { id: 'r1', name: '本地 → 数据库', kind: 'local', local: '127.0.0.1:5432', remote: '10.20.0.11:5432', host: 'h3', on: true },
  { id: 'r2', name: 'Grafana 面板', kind: 'local', local: '127.0.0.1:3000', remote: '10.20.0.11:3000', host: 'h3', on: false },
  { id: 'r3', name: '远程调试端口', kind: 'remote', local: '0.0.0.0:9229', remote: '127.0.0.1:9229', host: 'h6', on: false },
];
export const IMPORT_SAMPLE = 'Host web-prod-01\n  HostName 203.0.113.10\n  User ops\n  Port 22\n  IdentityFile ~\\.ssh\\id_ed25519\n\nHost edge-cache\n  HostName 203.0.113.24\n  User ops\n  Port 2202\n\nHost legacy-box\n  HostName 198.51.100.7\n  User admin\n  Port 22\n';

/* ---------- 局域网对等层（mock 状态机：E2E 可驱动开关/窗口） ---------- */
let mockPeerListening = false;
let mockPeerAlwaysOn = false;
let mockPeerWindowLeft = 0;
let mockPeers: any[] = [];
/** 本机正在被控制的会话（mock：由 E2E 注入事件） */
let mockControlled: any[] = [];
/** 配置收件箱（mock）：默认有一条，导入/忽略后消失 */
let mockInbox: any[] = [
  {
    id: 'inbox-mock-1',
    from: 'mock-laptop',
    receivedAt: '2026-11-17 10:20:00',
    bytes: 2048,
    hosts: 3,
    snippets: 2,
    tunnels: 1,
    folders: 1,
    exportedAt: 'unix:1',
    valid: true,
    error: null,
  },
];
let mockDiscovered = [
  { deviceId: 'dev-a', name: 'mock-laptop', addr: '192.168.1.50:27410', pairAddr: '192.168.1.50:28410', fp: 'mockfp01', pairing: true, paired: false },
  { deviceId: 'dev-b', name: 'mock-desktop', addr: '192.168.1.51:27410', pairAddr: '192.168.1.51:28410', fp: 'mockfp02', pairing: true, paired: false },
];
function mockPeerStatus() {
  return {
    listening: mockPeerListening,
    alwaysOn: mockPeerAlwaysOn,
    windowLeftSec: mockPeerListening && mockPeerWindowLeft > 0 ? mockPeerWindowLeft : null,
    port: mockPeerListening ? 27410 : null,
    pairPort: mockPeerListening ? 28410 : null,
    deviceName: 'mock-device',
    fingerprint: 'SHA256:mockmockmockmockmockmockmockmockmockmock',
    shareRoot: '',
    peerCount: 0,
  };
}

type FsFile = { t: 'f'; s: number };
type FsDir = { t: 'd'; c: Record<string, FsNode> };
type FsNode = FsFile | FsDir;
const D = (c: Record<string, FsNode> = {}): FsDir => ({ t: 'd', c });
const F = (s = 1024): FsFile => ({ t: 'f', s });

export function defaultLocalTree(): FsDir { return D({
  // Windows 盘符树：根为盘符列表（本地栏 Windows 化）
  'C:': D({
    'Users': D({ 'Wayne': D({
      'Documents': D({ 'notes.md': F(5410) }),
      'Downloads': D({ 'app-v2.3.1.tar.gz': F(4821102), 'schema.sql': F(18940), 'terms.pdf': F(233810) }),
      'Desktop': D({ 'desktop.ini': F(282) }),
    }) }),
    'Windows': D({ 'System32': D({ 'drivers': D({ 'etc': D({ 'hosts': F(824) }) }) }) }),
    'Program Files': D({ 'Ruthenium': D({ 'README.md': F(1180) }) }),
  }),
  'D:': D({
    'Projects': D({ 'Ruthenium': D({ 'src': D({ 'main.rs': F(8421), 'app.rs': F(12044) }), 'Cargo.toml': F(1120), 'README.md': F(2380) }), 'notes': D({ 'spec.md': F(5410) }) }),
    'Backups': D({ 'ruthenium-backup.json': F(22140) }),
  }),
}); }
export function defaultRemoteTree(): FsDir { return D({
  'home': D({ 'deploy': D({
    'app': D({ 'current': D({ 'package.json': F(2140), 'server.js': F(9820), '.env': F(480) }), 'releases': D({ 'v2.3.0': D({}), 'v2.3.1': D({}) }), 'logs': D({ 'access.log': F(182400), 'error.log': F(24110) }) }),
    'backups': D({ 'db-2026-11-08.sql.gz': F(28411000), 'db-2026-11-09.sql.gz': F(29104800) }),
    'scripts': D({ 'deploy.sh': F(3240), 'healthcheck.sh': F(910) }),
    'README': F(220),
  }) }),
  'etc': D({ 'nginx': D({ 'nginx.conf': F(4120) }), 'hosts': F(310) }),
  'var': D({ 'log': D({ 'syslog': F(90800) }) }),
}); }

export function defaultConfig(): Config {
  return {
    version: 1,
    preferences: {
      lang: 'zh-CN', theme: 'system', nav: 'classic', fontSize: 13, lineHeight: 1.3, font: 'JetBrains Mono', cursor: 'block',
      showStatus: true, showDots: true, showLineNumbers: false, confirmClose: false, restoreTabs: true, sidebarWidth: 262,
      gpuMode: 'system', autoReconnect: false, logSessions: false, keepaliveSecs: 30, requireWindowsHello: false,
      lanVisible: false, lanDeviceName: '', lanPort: 27410, peerShareRoot: '', lanStrictPairing: false,
    },
    folders: DEFAULT_FOLDERS.map((f) => ({ ...f })),
    hosts: HOSTS0.map((x) => ({ ...x })),
    snippets: SNIPS0.map((x) => ({ ...x })),
    tunnels: RULES0.map((x) => ({ ...x })),
  };
}

/* ============ 事件总线（mock 侧） ============ */
type Handler = (payload: unknown) => void;
const bus = new Map<string, Set<Handler>>();
export function mockListen(event: string, fn: Handler): () => void {
  if (!bus.has(event)) bus.set(event, new Set());
  bus.get(event)!.add(fn);
  return () => { bus.get(event)?.delete(fn); };
}
export function mockEmit(event: string, payload: unknown): void {
  bus.get(event)?.forEach((fn) => { try { fn(payload); } catch { /* ignore */ } });
}

/* ============ 内存状态 ============ */
interface MockState {
  config: Config;
  sessions: Map<string, { hostId: string; kind: 'term'|'sftp'|'local'; encoding: string }>;
  /** 快速连接的临时主机：只在内存里，关会话即丢弃，绝不进 config */
  transient: Map<string, Host>;
  local: FsDir;
  remote: FsDir;
  timers: Map<string, ReturnType<typeof setInterval>>;
  /** mock 凭据管理器：密码明文只在这里，绝不进 config */
  passwords: Map<string, string>;
}
let st: MockState = fresh();
function fresh(): MockState {
  return {
    config: defaultConfig(), sessions: new Map(), transient: new Map(),
    local: defaultLocalTree(), remote: defaultRemoteTree(),
    timers: new Map(), passwords: new Map(Object.entries(SEED_PASSWORDS)),
  };
}
const enc = new TextEncoder();

/**
 * Uint8Array → base64（STANDARD alphabet，与后端 `events::b64` 完全一致）。
 * mock 的 `term://data` 负载形状必须与真实后端相同，否则 e2e 保真度失效。
 * 分块理由同 `commands.bytesToB64`：`String.fromCharCode(...arr)` 超过 ~32K 会抛 RangeError。
 */
function b64Of(bytes: Uint8Array): string {
  let bin = '';
  for (let i = 0; i < bytes.length; i += 0x8000) {
    bin += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  }
  return btoa(bin);
}

/** base64 → Uint8Array（与后端 `b64_decode` 一致）；非法输入按后端语义抛 E_INVALID_INPUT，不静默兜底。 */
function bytesOfB64(s: string): Uint8Array {
  let bin: string;
  try {
    bin = atob(s);
  } catch {
    throw { code: 'E_INVALID_INPUT', detail: '终端输入不是合法的 base64' };
  }
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i += 1) out[i] = bin.charCodeAt(i);
  return out;
}

function nodeAt(root: FsDir, path: string): FsNode | null {
  // 本地 Windows 路径（反斜杠）与远端 POSIX 路径统一按分隔符切段后查找
  let n: FsNode = root;
  for (const p of String(path || '').replace(/\\/g, '/').split('/').filter(Boolean)) {
    if (n.t !== 'd' || !n.c[p]) return null;
    n = n.c[p];
  }
  return n;
}
function entries(root: FsDir, path: string): FileEntry[] {
  const n = nodeAt(root, path);
  if (!n || n.t !== 'd') return [];
  return Object.keys(n.c)
    .sort((a, b) => { const A = n.c[a], B = n.c[b]; if (A.t !== B.t) return A.t === 'd' ? -1 : 1; return a.localeCompare(b); })
    .map((k) => ({
      name: k,
      isDir: n.c[k].t === 'd',
      size: n.c[k].t === 'f' ? (n.c[k] as FsFile).s : null,
      modified: null,
      // mock 权限位：目录 755、普通文件 644，用于验证 SFTP 权限列渲染
      mode: n.c[k].t === 'd' ? 0o040755 : 0o100644,
    }));
}
function parentOf(p: string): string {
  const a = p.replace(/\\/g, '/').split('/').filter(Boolean); a.pop();
  return a.join('/');
}
function localParentOf(p: string): string {
  const a = p.split('\\').filter(Boolean); a.pop();
  return a.join('\\');
}
function joinPath(p: string, name: string): string { return (p === '/' ? '' : p) + '/' + name; }
/** Windows 风格拼接：C:\Users\a + b.txt → C:\Users\a\b.txt */
function localJoin(dir: string, name: string): string {
  return dir ? dir.replace(/\\+$/, '') + '\\' + name : name;
}

const MOCK_FINGERPRINT = 'SHA256:mZ4K9xVQ2rT8yH1pLcN6aF0sD3gB7wE5jR2uY9iO4qA';

// mock 的「密码被拒绝」哨兵：用于驱动「密码过期 → 重开弹窗重试一次」的链路（E2E 使用）
export const MOCK_REJECTED_PASSWORD = 'wrong-password';

/** 密码校验：password 认证时，本次调用带上来的密码优先，其次读 mock keyring。
 *  错误 detail 与真实后端保持同一套文案分支：未保存密码 / 密码被拒绝。 */
function checkPassword(host: Host, password?: string): void {
  if (host.auth !== 'password') return;
  const given = password ?? st.passwords.get(host.id);
  if (!given) throw { code: 'E_SSH_AUTH', detail: '未保存密码' };
  if (given === MOCK_REJECTED_PASSWORD) throw { code: 'E_SSH_AUTH', detail: '已保存的密码被服务器拒绝' };
}

/** mock 侧与真实后端 `start_tunnels_for_host` / `stop_tunnels_for_host` 对齐：
 *  会话连上 → 该 host 的 on=true 规则进入 active；会话关闭 → 全部 active=false。
 *  没有这条，前端的「运行态」展示（状态栏计数 / 规则列「监听中/待连接」）在 mock 里永远是 0。 */
function emitTunnelsForHost(hostId: string, up: boolean): void {
  for (const r of st.config.tunnels) {
    if (r.host !== hostId) continue;
    mockEmit(EV.tunnelState, { ruleId: r.id, active: up && r.on } satisfies TunnelStateEvent);
  }
}

/** 该 host 当前是否有活动会话（mock 版 connected_session_for_host）。 */
function hostHasSession(hostId: string): boolean {
  return Array.from(st.sessions.values()).some((s) => s.hostId === hostId);
}

/** 建连主路径（对齐后端 session.rs connect_inner）：host 由调用方先解析好 ——
 *  connect / reconnect 只认 config.hosts（临时主机必然 E_NOT_FOUND，由前端
 *  connectTransient 分支补偿），connect_transient 直接用前端传来的临时主机对象。 */
async function mockConnect(host: Host, kind: 'term'|'sftp', password?: string): Promise<string> {
  if (!host.known) {
    const challenge: HostkeyChallengeEvent = {
      sessionId: 'pending-' + host.id, hostId: host.id, host: host.name, port: host.port,
      algo: 'ED25519', fingerprint: MOCK_FINGERPRINT, changed: false,
    };
    mockEmit(EV.hostkey, challenge);
    throw { code: 'E_SSH_HOSTKEY_CHANGED', detail: '未知主机指纹，等待用户确认' };
  }
  // SFTP 复用：同主机已有活动会话时后端复用同一条 SSH 连接，不再重新认证
  const reused = kind === 'sftp' && Array.from(st.sessions.values()).some((x) => x.hostId === host.id);
  if (!reused) checkPassword(host, password);
  const id = uid();
  st.sessions.set(id, { hostId: host.id, kind, encoding: 'UTF-8' });
  // 与真实后端一致（session.rs:201-213）：握手 + 认证 + PTY 全部就绪后才 emit connected，
  // 且事件先于 invoke resolve 返回；mock 用约 900ms 模拟握手耗时，期间前端标签保持
  // provisional 的 connecting 态。真实后端没有 'connecting' 中间事件，这里同样不发。
  await new Promise<void>((resolve) => setTimeout(resolve, 900));
  if (!st.sessions.has(id)) return id; // 握手期间被关闭：不发事件、不留后台会话
  mockEmit(EV.connState, { sessionId: id, state: 'connected', latencyMs: 24 } satisfies ConnStateEvent);
  // 与真实后端一致：连接成功后自动拉起该主机已启用的转发规则
  emitTunnelsForHost(host.id, true);
  if (kind === 'term') {
    const banner = 'Last login: ' + new Date().toDateString() + ' · ED25519 · known_hosts verified\r\n' +
      T('term.welcome') + '\r\n';
    mockEmit(EV.termData, { sessionId: id, data: b64Of(enc.encode(banner)) } satisfies TermDataEvent);
  }
  return id;
}

/** 本地终端（ConPTY）在 mock 中复用同一会话 / term://data 回显链路，立即就绪。 */
function mockConnectLocal(): string {
  const id = uid();
  st.sessions.set(id, { hostId: '', kind: 'local', encoding: 'UTF-8' });
  mockEmit(EV.connState, { sessionId: id, state: 'connected', latencyMs: 0 } satisfies ConnStateEvent);
  mockEmit(EV.termData, {
    sessionId: id,
    data: b64Of(enc.encode(T('term.welcome') + '\r\n')),
  } satisfies TermDataEvent);
  return id;
}

function doTransfer(dir: 'up'|'down', transferIdStr: string, name: string, fromPath: string, toPath: string): void {
  const fromRoot = dir === 'up' ? st.local : st.remote;
  const toRoot = dir === 'up' ? st.remote : st.local;
  const src = nodeAt(fromRoot, fromPath);
  const dst = nodeAt(toRoot, toPath);
  const parentOfSrc = dir === 'up' ? localParentOf(fromPath) : parentOf(fromPath);
  const total = src && src.t === 'f' ? src.s : 4096;
  let done = 0;
  const timer = setInterval(() => {
    done = Math.min(total, done + total * 0.18 + 512);
    const isDone = done >= total;
    mockEmit(EV.sftpProgress, { transferId: transferIdStr, bytesDone: Math.floor(done), bytesTotal: total, state: isDone ? 'done' : 'running' } satisfies SftpProgressEvent);
    if (isDone) {
      clearInterval(timer);
      st.timers.delete(transferIdStr);
      if (src && dst && dst.t === 'd') {
        let nm = name, n = 1;
        while (dst.c[nm]) { nm = name.replace(/(\.[^.]+)?$/, (m) => ' (' + n + ')' + (m || '')); n++; }
        dst.c[nm] = JSON.parse(JSON.stringify(src));
      }
      mockEmit(EV.sftpChanged, { sessionId: '', side: dir === 'up' ? 'remote' : 'local', path: toPath } satisfies SftpChangedEvent);
      mockEmit(EV.sftpChanged, { sessionId: '', side: dir === 'up' ? 'local' : 'remote', path: parentOfSrc } satisfies SftpChangedEvent);
    }
  }, 170);
  st.timers.set(transferIdStr, timer);
}

/* ============ 指标模拟（2s 一帧，随机游走） ============ */
interface MetricsSim { timer: ReturnType<typeof setInterval>; last: HostMetrics; walk: (m: HostMetrics) => HostMetrics }
const metricsSims = new Map<string, MetricsSim>();
function baseMetrics(sessionId: string): HostMetrics {
  return {
    sessionId, sampledAt: Date.now(),
    cpuPct: 12.4, cores: 4, loadAvg: [0.18, 0.24, 0.21],
    memUsedGb: 2.1, memTotalGb: 7.8,
    diskUsedGb: 31, diskTotalGb: 80,
    netRxMbps: 0.8, netTxMbps: 0.3, diskReadMbps: 0.4, diskWriteMbps: 0.2,
  };
}
function walkMetrics(m: HostMetrics): HostMetrics {
  const rw = (v: number, amp: number, lo: number, hi: number) =>
    Math.min(hi, Math.max(lo, v + (Math.random() - 0.5) * amp));
  return {
    ...m, sampledAt: Date.now(),
    cpuPct: rw(m.cpuPct, 9, 1.5, 96),
    loadAvg: [rw(m.loadAvg[0], 0.1, 0, 4), rw(m.loadAvg[1], 0.08, 0, 4), rw(m.loadAvg[2], 0.05, 0, 4)],
    memUsedGb: rw(m.memUsedGb, 0.12, 1.2, m.memTotalGb * 0.92),
    netRxMbps: rw(m.netRxMbps, 0.9, 0.02, 12),
    netTxMbps: rw(m.netTxMbps, 0.5, 0.01, 6),
    diskReadMbps: rw(m.diskReadMbps, 0.6, 0, 8),
    diskWriteMbps: rw(m.diskWriteMbps, 0.4, 0, 5),
  };
}

/* ============ 命令调用计数（E2E 断言用） ============ */
// 记录各 mock 命令被调用的次数，暴露到 window.__mockCalls，便于 E2E 验证「没有重复调用」这类约束。
const mockCallCounts: Record<string, number> = {};
if (typeof window !== 'undefined') {
  (window as unknown as { __mockCalls: Record<string, number> }).__mockCalls = mockCallCounts;
}

/* ============ 命令分发 ============ */
// eslint-disable-next-line @typescript-eslint/no-explicit-any
export function mockInvoke<T = void>(cmd: string, args: any = {}): T | Promise<T> {
  mockCallCounts[cmd] = (mockCallCounts[cmd] ?? 0) + 1;
  switch (cmd) {
    case 'load_config':
      return JSON.parse(JSON.stringify(st.config)) as T;
    case 'save_preferences':
      st.config.preferences = { ...st.config.preferences, ...args.preferences };
      return undefined as T;
    case 'import_ssh_config': {
      const parsed: ParsedHost[] = parseSshConfig(String(args.text ?? ''));
      // 与后端一致：duplicate 按 addr + port 比对（config.rs parse_ssh_config）
      parsed.forEach((x) => { x.duplicate = st.config.hosts.some((y) => y.addr === x.addr && y.port === x.port); });
      return parsed as T;
    }
    case 'commit_import': {
      let added = 0;
      (args.hosts as Host[]).forEach((x) => {
        // 与后端 commit_hosts_into 一致：重复按 addr + port 跳过
        if (st.config.hosts.some((y) => y.addr === x.addr && y.port === x.port)) return;
        st.config.hosts.push({ ...x, id: uid() });
        added++;
      });
      return added as T;
    }
    case 'export_backup':
      // 与真实后端对齐：只认对话框签发的一次性 token；mock 没有对话框，
      // 任何 token 都无效 → E_INVALID_INPUT（浏览器路径走 Blob 下载，不经过这里）。
      throw { code: 'E_INVALID_INPUT', detail: '备份路径已失效：请重新选择文件' };
    case 'import_backup':
      // 同上：token 必须来自 pick_backup_open_path；mock 下无对话框，一律拒绝。
      throw { code: 'E_INVALID_INPUT', detail: '备份路径已失效：请重新选择文件' };
    case 'list_hosts':
      return JSON.parse(JSON.stringify(st.config.hosts)) as T;
    case 'upsert_host': {
      const host = args.host as Host;
      const i = st.config.hosts.findIndex((x) => x.id === host.id);
      if (i >= 0) st.config.hosts[i] = { ...host };
      else st.config.hosts.push({ ...host, id: host.id || uid() });
      return JSON.parse(JSON.stringify(i >= 0 ? st.config.hosts[i] : st.config.hosts[st.config.hosts.length - 1])) as T;
    }
    case 'delete_host':
      st.config.hosts = st.config.hosts.filter((x) => x.id !== args.id);
      return undefined as T;
    case 'set_password': {
      const hostId = String(args.hostId ?? '');
      if (!st.config.hosts.some((x) => x.id === hostId)) throw { code: 'E_NOT_FOUND', detail: '主机不存在' };
      st.passwords.set(hostId, String(args.password ?? ''));
      return undefined as T;
    }
    case 'delete_password':
      st.passwords.delete(String(args.hostId ?? ''));
      return undefined as T;
    case 'has_password':
      return st.passwords.has(String(args.hostId ?? '')) as T;
    case 'connect': {
      // 真实后端按 host.id 查 config.hosts，查不到 E_NOT_FOUND（session.rs:94-102）
      const host = st.config.hosts.find((x) => x.id === String(args.hostId ?? ''));
      if (!host) throw { code: 'E_NOT_FOUND', detail: '主机不存在: ' + args.hostId };
      return mockConnect(host, args.kind === 'sftp' ? 'sftp' : 'term', args.password) as T;
    }
    case 'connect_transient': {
      // 快速连接：主机对象由前端构造，只进内存；认证 / 指纹 / 事件链路与 connect 相同
      const host = args.host as Host;
      if (!host || !host.id) throw { code: 'E_INVALID_INPUT', detail: '临时主机缺少 id' };
      // 前端第二次调用（指纹确认后）可能仍带 known=false：并入 mock 侧已信任的状态
      const prev = st.transient.get(host.id);
      const merged: Host = { ...host, known: !!host.known || !!prev?.known };
      st.transient.set(host.id, merged);
      return mockConnect(merged, 'term', args.password) as T;
    }
    case 'connect_local':
      return mockConnectLocal() as T;
    case 'write_terminal': {
      // 与真实后端一致：入参 data 是 base64；先解码验证（非法 base64 抛 E_INVALID_INPUT），
      // 再按 base64 回显——mock 不产生 JSON 数字数组，事件形状与 Rust 侧完全相同。
      const data = bytesOfB64(String(args.data ?? ''));
      mockEmit(EV.termData, { sessionId: args.sessionId, data: b64Of(data) } satisfies TermDataEvent);
      return undefined as T;
    }
    case 'resize_terminal':
      return undefined as T;
    case 'close_session': {
      const closing = st.sessions.get(args.sessionId);
      if (closing) {
        st.sessions.delete(args.sessionId);
        // 临时主机随最后一个会话一起丢弃（关标签即丢弃，重启无残留）
        for (const [hid] of st.transient) {
          if (!Array.from(st.sessions.values()).some((x) => x.hostId === hid)) st.transient.delete(hid);
        }
        mockEmit(EV.connState, { sessionId: args.sessionId, state: 'disconnected', reason: 'closed_by_user' } satisfies ConnStateEvent);
        mockEmit(EV.termExit, { sessionId: args.sessionId, reason: 'closed' } satisfies TermExitEvent);
        // 与真实后端一致：会话关闭会释放该主机的转发监听（tunnel://state active:false）
        if (closing.hostId && !hostHasSession(closing.hostId)) emitTunnelsForHost(closing.hostId, false);
      }
      return undefined as T;
    }
    case 'reconnect': {
      // 对齐真实后端（session.rs:379-401）：先查会话表并关闭旧会话，再按 host.id 回
      // config.hosts 找主机 —— 临时主机必然 E_NOT_FOUND，由前端 connectTransient 分支补偿
      const s = st.sessions.get(String(args.sessionId ?? ''));
      if (!s) throw { code: 'E_NOT_FOUND', detail: '会话不存在' };
      st.sessions.delete(String(args.sessionId ?? ''));
      const host = st.config.hosts.find((x) => x.id === s.hostId);
      if (!host) throw { code: 'E_NOT_FOUND', detail: '主机不存在: ' + s.hostId };
      return mockConnect(host, s.kind === 'sftp' ? 'sftp' : 'term', args.password) as T;
    }
    case 'cancel_connect':
      // mock 的建连是同步完成的，没有可取消的在途握手；返回 false 表示"没找到在途连接"。
      return false as T;
    case 'list_sessions':
      // 与真实后端 SessionSummary 对齐：connected/startedAt 字段补齐，缺这些字段时
      // 依赖它们的测试在 mock 下“通过”但真实后端行为不同。
      return Array.from(st.sessions.entries()).map(([id, s]) => ({
        id, hostId: s.hostId, kind: s.kind, connected: true, startedAt: Date.now(), encoding: s.encoding,
      })) as T;
    case 'trust_host_key': {
      const hostId = String(args.sessionId ?? '').replace(/^pending-/, '');
      const host = st.config.hosts.find((x) => x.id === hostId) ?? st.transient.get(hostId);
      if (host) host.known = true;
      return undefined as T;
    }
    case 'reject_host_key':
      return undefined as T;
    case 'local_list': {
      const p = String(args.path ?? '');
      // 根为盘符列表（Windows 化本地栏）；后端返回的盘符名形如 "C:"（sftp.rs list_local_roots），
      // mock 树的键本身就带冒号，原样返回即与后端一致
      if (!p || p === '/' || p === '\\') {
        // mode: null —— 与真实后端一致（盘符条目无 mode 信息）
        return Object.keys(st.local.c).sort().map((k) => ({ name: k, isDir: true, size: null, modified: null, mode: null })) as T;
      }
      return entries(st.local, p) as T;
    }
    case 'sftp_home':
      // 与真实后端一致：SFTP 首次打开的目录由后端给出（mock 里用 /home/deploy）
      // mock 远端 home 指向有实际文件的目录，E2E 的 SFTP 用例从这里开始
      return '/home/deploy/app' as T;
    case 'sftp_list':
      return entries(st.remote, String(args.path ?? '/')) as T;
    case 'local_mkdir': case 'sftp_mkdir': {
      const root = cmd === 'local_mkdir' ? st.local : st.remote;
      const dir = nodeAt(root, String(args.path ?? '/'));
      if (dir && dir.t === 'd' && args.name && !dir.c[args.name]) dir.c[args.name] = D({});
      else throw { code: cmd === 'local_mkdir' ? 'E_FS' : 'E_SFTP_IO', detail: '目录不可用或名称已存在' };
      return undefined as T;
    }
    case 'local_delete': case 'sftp_delete': {
      const root = cmd === 'local_delete' ? st.local : st.remote;
      const p = String(cmd === 'local_delete' ? args.path : args.path);
      const isLocal = cmd === 'local_delete';
      const parent = nodeAt(root, isLocal ? localParentOf(p) : parentOf(p));
      const name = p.split(/[\\/]/).filter(Boolean).pop();
      if (parent && parent.t === 'd' && name && parent.c[name]) delete parent.c[name];
      else throw { code: cmd === 'local_delete' ? 'E_FS' : 'E_SFTP_IO', detail: '文件不存在' };
      return undefined as T;
    }
    case 'local_rename': case 'sftp_rename': {
      const isLocal = cmd === 'local_rename';
      const root = isLocal ? st.local : st.remote;
      const p = String(args.path ?? '');
      const newName = String(args.newName ?? '').trim();
      const parent = nodeAt(root, isLocal ? localParentOf(p) : parentOf(p));
      const name = p.split(/[\\/]/).filter(Boolean).pop();
      if (!newName) throw { code: 'E_INVALID_INPUT', detail: '名称为空' };
      if (/[\\/]/.test(newName)) throw { code: 'E_INVALID_INPUT', detail: '名称不能包含路径分隔符' };
      if (!parent || parent.t !== 'd' || !name || !parent.c[name]) {
        throw { code: isLocal ? 'E_FS' : 'E_SFTP_IO', detail: '文件不存在' };
      }
      if (parent.c[newName]) throw { code: isLocal ? 'E_FS' : 'E_SFTP_IO', detail: '同名文件已存在' };
      parent.c[newName] = parent.c[name];
      delete parent.c[name];
      // 与后端一致：返回新的完整路径（本地反斜杠、远端正斜杠）
      if (isLocal) return localJoin(localParentOf(p), newName) as T;
      return (parentOf(p) === '/' ? '' : parentOf(p)) + '/' + newName as T;
    }
    case 'sftp_upload': case 'sftp_download': {
      const tid = uid();
      const up = cmd === 'sftp_upload';
      // localPath/remotePath：一侧为完整源文件路径，另一侧为目标目录
      const srcPath = String(up ? args.localPath : args.remotePath);
      const dstDir = String(up ? args.remotePath : args.localPath);
      const srcName = srcPath.split(/[\\/]/).filter(Boolean).pop() ?? 'file';
      doTransfer(up ? 'up' : 'down', tid, srcName, up ? localParentOf(srcPath) : parentOf(srcPath), dstDir);
      return tid as T;
    }
    case 'sftp_cancel': {
      // 与后端一致（sftp.rs sftp_cancel）：表里查不到的 transferId 直接 E_NOT_FOUND，
      // 不再假装取消成功；查得到才清理并 emit cancelled（后端由任务自身上报）
      const t = st.timers.get(String(args.transferId ?? ''));
      if (!t) throw { code: 'E_NOT_FOUND', detail: '传输 ' + args.transferId + ' 不存在' };
      clearInterval(t);
      st.timers.delete(String(args.transferId ?? ''));
      mockEmit(EV.sftpProgress, { transferId: args.transferId, bytesDone: 0, bytesTotal: 0, state: 'cancelled' } satisfies SftpProgressEvent);
      return undefined as T;
    }
    case 'list_snippets':
      return JSON.parse(JSON.stringify(st.config.snippets)) as T;
    case 'upsert_snippet': {
      const sn = args.snippet as Snippet;
      const i = st.config.snippets.findIndex((x) => x.id === sn.id);
      if (i >= 0) st.config.snippets[i] = { ...sn };
      else st.config.snippets.push({ ...sn, id: sn.id || uid() });
      // 与真实后端一致（hosts.rs upsert_snippet）：返回落盘后的 Snippet
      return JSON.parse(JSON.stringify(i >= 0 ? st.config.snippets[i] : st.config.snippets[st.config.snippets.length - 1])) as T;
    }
    case 'delete_snippet':
      st.config.snippets = st.config.snippets.filter((x) => x.id !== args.id);
      return undefined as T;
    case 'list_tunnels':
      return JSON.parse(JSON.stringify(st.config.tunnels)) as T;
    case 'upsert_tunnel': {
      const r = args.tunnel as TunnelRule;
      const i = st.config.tunnels.findIndex((x) => x.id === r.id);
      if (i >= 0) st.config.tunnels[i] = { ...r };
      else st.config.tunnels.push({ ...r, id: r.id || uid(), on: r.on ?? false });
      return undefined as T;
    }
    case 'delete_tunnel':
      st.config.tunnels = st.config.tunnels.filter((x) => x.id !== args.id);
      // 与真实后端一致：删除规则时补发 active:false，清掉前端运行态
      mockEmit(EV.tunnelState, { ruleId: String(args.id ?? ''), active: false } satisfies TunnelStateEvent);
      return undefined as T;
    case 'toggle_tunnel': {
      const r = st.config.tunnels.find((x) => x.id === args.id);
      if (!r) throw { code: 'E_NOT_FOUND', detail: '规则不存在' };
      r.on = !!args.on;
      // 与真实后端一致：会话未连接时只是"待连接"（active=false），连上后才真正监听
      mockEmit(EV.tunnelState, { ruleId: r.id, active: r.on && hostHasSession(r.host) } satisfies TunnelStateEvent);
      return undefined as T;
    }
    case 'upsert_folder': {
      const name = String(args.name ?? '').trim();
      if (!name) throw { code: 'E_INVALID_INPUT', detail: '分组名不能为空' };
      const folders = st.config.folders;
      // 与后端一致：folders 为空时先用现有 host.group 播种，避免新增后其它分组消失
      if (!folders.length) {
        const seen: string[] = [];
        for (const h of st.config.hosts) {
          const g = String(h.group ?? '').trim();
          if (g && !seen.includes(g)) seen.push(g);
        }
        for (const g of seen) folders.push({ name: g, color: '' });
      }
      const color = String(args.color ?? '');
      const found = folders.find((f) => f.name === name);
      if (found) found.color = color; else folders.push({ name, color });
      return { name, color } as T;
    }
    case 'rename_folder': {
      // 与后端 rename_folder 同语义：folders 与成员主机 group 一并更新
      const o = String(args.oldName ?? '').trim();
      const n = String(args.newName ?? '').trim();
      if (!n) throw { code: 'E_INVALID_INPUT', detail: '分组名不能为空' };
      const old = st.config.folders.find((f) => f.name === o);
      if (!old) throw { code: 'E_NOT_FOUND', detail: '分组不存在' };
      if (st.config.folders.some((f) => f.name === n)) throw { code: 'E_INVALID_INPUT', detail: '分组已存在' };
      old.name = n;
      for (const h of st.config.hosts) if (h.group === o) h.group = n;
      return undefined as T;
    }
    case 'delete_folder': {
      // 与后端 delete_folder 同语义：成员主机移入剩余第一个分组（不删除主机）
      const name = String(args.name ?? '').trim();
      const found = st.config.folders.find((f) => f.name === name);
      if (!found) throw { code: 'E_NOT_FOUND', detail: '分组不存在' };
      st.config.folders = st.config.folders.filter((f) => f.name !== name);
      const fallback = st.config.folders[0]?.name ?? '';
      let moved = 0;
      for (const h of st.config.hosts) if (h.group === name) { h.group = fallback; moved += 1; }
      return moved as T;
    }
    case 'get_app_info':
      // 版本号与真实后端同源（tauri.conf.json → 构建期注入 __APP_VERSION__）
      return { version: __APP_VERSION__, configDir: '%APPDATA%\\Ruthenium', logsDir: '%APPDATA%\\Ruthenium\\logs', os: 'windows (mock)' } as T;
    case 'hello_requirement':
      // 浏览器 / E2E：不拦启动。真实设备的可用性由 Tauri 后端（WinRT）上报。
      return { required: false, availability: 'available' } as T;
    case 'verify_windows_hello':
      return 'verified' as T;
    // ---- 局域网对等层（mock：内存状态机，E2E 可驱动） ----
    case 'peer_status':
      return mockPeerStatus() as T;
    case 'peer_set_visible': {
      mockPeerAlwaysOn = !!args?.visible;
      mockPeerListening = mockPeerAlwaysOn;
      mockPeerWindowLeft = 0;
      return mockPeerStatus() as T;
    }
    case 'peer_open_window': {
      const minutes = Number(args?.minutes ?? 5);
      mockPeerListening = true;
      mockPeerWindowLeft = Math.max(1, Math.round(minutes)) * 60;
      if (mockPeerAlwaysOn) mockPeerWindowLeft = 0;
      return mockPeerStatus() as T;
    }
    case 'peer_close_window': {
      if (!mockPeerAlwaysOn) { mockPeerListening = false; mockPeerWindowLeft = 0; }
      return mockPeerStatus() as T;
    }
    case 'peer_list':
      return mockPeers as T;
    case 'peer_discover':
      mockPeerListening = true;
      return mockDiscovered as T;
    case 'peer_open_screen':
      return ('peer-screen-' + String(args?.peerId ?? 'x')) as T;
    case 'peer_screen_command':
      return true as T;
    case 'peer_close_screen':
      return true as T;
    case 'peer_screen_consent':
      return true as T;
    case 'peer_screen_sessions':
      return [] as T;
    case 'peer_audio_start':
      return { id: 'mock-audio-1', rate: 48000, channels: 2, frameMs: 20 } as T;
    case 'peer_audio_stop':
      return true as T;
    case 'peer_clipboard_start':
      return { id: 'mock-clip-1', text: 'mock clipboard' } as T;
    case 'peer_clipboard_get':
      return 'mock clipboard' as T;
    case 'peer_clipboard_set':
      return true as T;
    case 'peer_clipboard_stop':
      return true as T;
    case 'peer_clipboard_local_get':
      return 'mock local' as T;
    case 'peer_clipboard_local_set':
      return true as T;
    case 'peer_audit_recent':
      return [{ at: '2026-11-17 12:00:00', kind: 'screen', direction: 'incoming', peerName: 'mock-laptop', peerAddr: '192.168.1.50', quality: 65, width: 1920, height: 1080, frames: 120, bytes: 2000000, seconds: 12, reason: 'closed' }] as T;
    case 'peer_audit_clear':
      return true as T;
    case 'peer_input_start':
      return 'mock-input-1' as T;
    case 'peer_input_send':
      return true as T;
    case 'peer_input_stop':
      return true as T;
    case 'peer_screen_kick':
      return true as T;    case 'peer_open_terminal':
      return ('peer-term-' + String(args?.peerId ?? 'x')) as T;
    case 'peer_control_consent':
      return true as T;
    case 'peer_control_sessions':
      return JSON.parse(JSON.stringify(mockControlled)) as T;
    case 'peer_kick_session': {
      mockControlled = [];
      return true as T;
    }
    case 'peer_open_sftp':
      return ('peer-sftp-' + String(args?.peerId ?? 'x')) as T;
    case 'peer_send_config':
      return 4096 as T;
    case 'peer_inbox':
      return JSON.parse(JSON.stringify(mockInbox)) as T;
    case 'peer_import_inbox': {
      const id = String(args?.itemId ?? '');
      mockInbox = mockInbox.filter((x) => x.id !== id);
      return JSON.parse(JSON.stringify(st.config)) as T;
    }
    case 'peer_delete_inbox': {
      const id = String(args?.itemId ?? '');
      mockInbox = mockInbox.filter((x) => x.id !== id);
      return true as T;
    }
    case 'peer_pair_start': {
      const pairingId = 'pair-mock-1';
      setTimeout(() => {
        mockEmit(EV.peerPairing, {
          status: 'prompt',
          pairingId,
          code: '246810',
          peerName: 'mock-laptop',
          peerFp: 'mockfp01',
          peerAddr: '192.168.1.50',
          direction: args?.addr ? 'incoming' : 'outgoing',
          numeric: !!args?.addr,
          expiresAtMs: Date.now() + 60_000,
        });
      }, 60);
      return pairingId as T;
    }
    case 'peer_pair_confirm': {
      const pairingId = String(args?.pairingId ?? 'pair-mock-1');
      if (!args?.agree) {
        setTimeout(() => mockEmit(EV.peerPairing, { status: 'rejected', pairingId, reason: 'canceled' }), 30);
        return false as T;
      }
      const peer = {
        id: 'mock-peer-1',
        name: 'mock-laptop',
        pubkey: 'ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIMOCKPEERKEY mock',
        lastAddr: '192.168.1.50:27410',
        pairedAt: '',
        lastSeen: null,
        grantFiles: !!args?.grantFiles,
        grantControl: !!args?.grantControl,
        grantConfig: !!args?.grantConfig,
        grantScreen: !!args?.grantScreen,
        grantClipboard: !!args?.grantClipboard,
        grantAudio: !!args?.grantAudio,
        peerAllowsFiles: true,
        peerAllowsControl: false,
        peerAllowsConfig: true,
        peerAllowsScreen: true,
        peerAllowsClipboard: true,
        peerAllowsAudio: true,
      };
      mockPeers = [peer];
      mockDiscovered = mockDiscovered.map((d) => (d.deviceId === 'dev-a' ? { ...d, paired: true } : d));
      setTimeout(() => mockEmit(EV.peerPairing, { status: 'ok', pairingId, peer }), 30);
      return true as T;
    }
    case 'peer_pair_cancel': {
      const pairingId = String(args?.pairingId ?? '');
      setTimeout(() => mockEmit(EV.peerPairing, { status: 'rejected', pairingId, reason: 'canceled' }), 30);
      return true as T;
    }
    case 'peer_remove': {
      mockPeers = mockPeers.filter((p) => p.id !== args?.peerId);
      return true as T;
    }
    case 'peer_update_grants': {
      const p = mockPeers.find((x) => x.id === args?.peerId);
      if (p) {
        p.grantFiles = !!args?.grantFiles;
        p.grantControl = !!args?.grantControl;
        p.grantConfig = !!args?.grantConfig;
      p.grantScreen = !!args?.grantScreen;
      p.grantClipboard = !!args?.grantClipboard;
      p.grantAudio = !!args?.grantAudio;
      }
      return (p ?? null) as T;
    }
    case 'clear_session_logs':
      // mock 环境没有真实日志文件：返回 0（前端提示"已清空 0 个"）。
      return 0 as T;
    case 'pick_folder':
      return 'C:\\Users\\Wayne\\RutheniumShared' as T;
    case 'pick_backup_save_path':
    case 'pick_backup_open_path':
      // 浏览器/mock 没有原生对话框：调用方只在 isTauri 分支使用这两个命令，
      // 这里返回 null（= 用户取消）作为防御，避免未知命令错误。
      return null as T;
    case 'ui_ready':
      // 与真实后端对齐：后端有该命令（幂等显示窗口）。缺了它浏览器 mock 会抛
      // E_NOT_FOUND，main.tsx 首帧后的 uiReady 调用会变成未捕获 Promise 拒绝。
      return undefined as T;
    case 'start_metrics': {
      const sid = String(args.sessionId ?? '');
      if (!st.sessions.has(sid)) throw { code: 'E_NOT_FOUND', detail: '会话不存在' };
      if (!metricsSims.has(sid)) {
        let last = baseMetrics(sid);
        const timer = setInterval(() => {
          const sim = metricsSims.get(sid);
          if (!sim) return;
          last = walkMetrics(last);
          mockEmit(EV.metrics, last satisfies HostMetrics);
        }, 2000);
        metricsSims.set(sid, { timer, last, walk: walkMetrics });
      }
      return undefined as T;
    }
    case 'stop_metrics': {
      const sid = String(args.sessionId ?? '');
      const sim = metricsSims.get(sid);
      if (sim) { clearInterval(sim.timer); metricsSims.delete(sid); }
      return undefined as T;
    }
    case 'sample_metrics': {
      const sid = String(args.sessionId ?? '');
      if (!st.sessions.has(sid)) throw { code: 'E_SSH_CHANNEL', detail: '会话不存在或已断开' };
      const sim = metricsSims.get(sid);
      const m = sim ? (sim.last = walkMetrics(sim.last)) : baseMetrics(sid);
      mockEmit(EV.metrics, m satisfies HostMetrics);
      return m as T;
    }
    case 'set_session_encoding': {
      const s = st.sessions.get(String(args.sessionId ?? ''));
      if (!s) throw { code: 'E_NOT_FOUND', detail: '会话不存在' };
      s.encoding = String(args.encoding ?? 'UTF-8');
      // 与真实后端一致（session.rs set_session_encoding）：返回应用后的编码名
      return s.encoding as T;
    }
    default:
      throw { code: 'E_NOT_FOUND', detail: '未知命令: ' + cmd };
  }
}

// 供「覆盖表 → 断线」等纯前端演示使用：由 conn://state 事件驱动断线横幅
// reason='lost' 表示网络掉线（自动重连只在 lost 时触发）；用户主动断开用 closed_by_user
export function mockDisconnectSession(sessionId: string, reason: 'lost'|'closed_by_user' = 'closed_by_user'): void {
  mockEmit(EV.connState, { sessionId, state: 'disconnected', reason } satisfies ConnStateEvent);
}

// 测试钩子（仅 E2E 注入 __RUTHENIUM_E2E__ 时暴露）：让无头浏览器能模拟「网络掉线」事件，
// 否则前端无法在 mock 层验证自动重连横幅与退避节奏。正式运行不挂任何全局。
if (typeof window !== 'undefined' && (window as unknown as { __RUTHENIUM_E2E__?: boolean }).__RUTHENIUM_E2E__) {
  (window as unknown as Record<string, unknown>).__mockEmit = (event: string, payload: unknown) => mockEmit(event, payload);
}
export { joinPath, parentOf, nodeAt as mockNodeAt, entries as mockEntries };
