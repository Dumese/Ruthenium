// 会话 slice：统一连接闸门、会话生命周期、重连、指纹与密码闸门、
// 自动重连调度、传输与指标等会话级事件。是业务上最核心的一块。
import { normalizeHostTags } from '../../lib/tags';
import { parseQuickConnect, buildQuickHost } from '../../lib/quickConnect';
import { defaultGroup } from '../../lib/defaults';
import { openDirectory } from '../../lib/openExternal';
import { MAX_RECONNECT_ATTEMPTS, reconnectSeconds } from '../../lib/reconnect';
import { setLowEndDevice } from '../../lib/gpu';
import { T, terr, setLang as i18nSetLang, isLang } from '../../i18n';
import { useMock } from '../../ipc/env';
import { dropScreen } from '../../components/screen/screenBus';
import {
  disposeTerm, moveTerm, writeTermData,
  hasTerm, clearTermSearch, focusTerm, applyGpuMode,
} from '../../components/terminal/useXterm';
import { normalizePrefs } from './prefsSlice';
import * as api from '../../ipc/commands';
import type { AppErrorEvent } from '../../types/events';
import type {
  Host, Snippet, TunnelRule, Session, SessionStatus, Transfer, HostMetrics,
  PwState, HostKeyState,
} from '../../types/models';
import type { AppStore } from '../types';
import type { SliceCreator } from './shared';
import {
  authFailure, findHostIn, isTransient, sessionLogging, pendingId,
  pwRetries, pendingKinds, cancelledConnects, earlyConnStates, quietSessions,
  reconnectingSessions, autoReconnecting, retryTimers, reconnectFailCode,
  retryGen, bumpRetryGen,
  clearRetryTimer, schedulePendingProgressDrop, pendingProgress, pendingProgressTimers,
} from './guards';

export interface SessionSlice {
  // 数据
  folders: Folder_[];
  hosts: Host[]; snippets: Snippet[]; tunnels: TunnelRule[];
  /** 快速连接创建的临时主机：只在内存里，关标签即丢弃，绝不写 hosts.toml */
  transientHosts: Host[];
  // 会话
  sessions: Session[]; active: string | null; dataTab: DataTab_;
  transfers: Transfer[];
  /** 侧栏当前选中主机（单击只选中，不弹详情） */
  selectedHost: string | null;
  // 指标（ConnInfoSheet，按 sessionId 缓存）
  metrics: Record<string, HostMetrics>; metricsLive: boolean;
  /** 自动重连倒计时：sessionId → { attempt, remain } */
  retry: Record<string, { attempt: number; remain: number }>;
  /** 终端查找条：只控制显示，输入 / 计数在组件内 */
  findOpen: boolean;
  /** 转发规则的**运行态**（来自 tunnel://state）。与配置里的 on（意图）分开：
   *  重连瞬间 / 会话未连接时 on=true 但 active=false，UI 必须能区分「已启用待连接」与「监听中」。 */
  tunnelState: Record<string, { active: boolean; error?: string }>;
  /** 连接时的密码弹窗状态（密码明文不在这里，只活在 PasswordModal 组件内） */
  pw: PwState | null;
  hostkey: HostKeyState | null;
  /** 待显示的指纹挑战（多主机并发连接时不能互相覆盖） */
  hostkeyQueue: HostKeyState[];
  booted: boolean;

  init(): Promise<void>;
  reload(): Promise<void>;

  /** 统一连接闸门：终端 / SFTP / 主机行 / 详情页 / 命令面板都走它 */
  requestConnect(hostId: string, kind?: 'term'|'sftp'|'local', password?: string): Promise<boolean>;
  connect(hostId: string, kind?: 'term'|'sftp', quiet?: boolean, password?: string): Promise<boolean>;
  connectLocal(host?: Host): Promise<void>;
  /** 快速连接：解析 user@host[:port] → 临时 Host → 同一 requestConnect 闸门 */
  quickConnect(input: string): Promise<boolean>;
  /** 快速连接标签右键「保存为主机」：打开预填好的 hostform */
  saveQuickHost(sessionId: string): void;
  /** 打开日志文件夹：opener.openPath(<configDir>/logs) */
  openLogFolder(): Promise<void>;
  /** 清空会话日志（先确认；后端只删 *.log，保留 app.log 诊断日志） */
  requestClearLogs(): void;
  clearSessionLogs(): Promise<void>;
  /** 终端查找条：Esc / 关闭按钮都走 closeTermFind（清装饰 + 清选中） */
  openTermFind(): void;
  closeTermFind(): void;
  /** 自动重连：进入倒计时 / 立即重连 / 取消 */
  scheduleAutoReconnect(id: string): void;
  runAutoReconnect(id: string, attempt: number): Promise<void>;
  retryNow(id: string): void;
  cancelAutoReconnect(id: string): void;
  openPasswordPrompt(hostId: string, kind: 'term'|'sftp', retry?: boolean, reconnectId?: string): void;
  submitPassword(password: string, remember: boolean): Promise<void>;
  selectHost(id: string): void;
  ensureTerm(): Promise<Session | null>;
  openSftpFor(hostId: string): Promise<void>;
  closeOthers(keepId: string): Promise<void>;
  closeRight(id: string): Promise<void>;
  setActive(id: string): void;
  closeSession(id: string): Promise<void>;
  reconnect(id: string, silent?: boolean, password?: string): Promise<boolean>;
  hostkeyAccept(): Promise<void>; hostkeyReject(): Promise<void>;
  onConnState(sessionId: string, state: 'connecting'|'connected'|'disconnected', latencyMs?: number, reason?: 'lost'|'closed_by_user'): void;
  onHostkeyChallenge(c: HostKeyState): void;
  /** app://error 全局错误（后端 emit）：弹 Toast；指纹挑战超时时一并收起失效弹窗 */
  onAppError(p: AppErrorEvent): void;
  /** 转发监听运行态（tunnel://state）：active/error 与配置里的 on 分开存 */
  onTunnelState(p: { ruleId: string; active: boolean; error?: string | null }): void;
  onSftpProgress(p: { transferId: string; bytesDone: number; bytesTotal: number; state: Transfer['state'] }): void;
  onSftpChanged(): void;
  addTransfer(t: Transfer): void;
  onMetricsSample(m: HostMetrics): void;
  toggleMetricsLive(): void;
  applyEncoding(enc: string): Promise<void>;
  closeAll(): Promise<void>;
}

type Folder_ = AppStore['folders'][number];
type DataTab_ = AppStore['dataTab'];

let initStarted = false;

export const createSessionSlice: SliceCreator<SessionSlice> = (set, get) => {
  /** 当前指纹挑战结束后切到队列下一项；队列为空才关闭 modal。 */
  const advanceHostkey = () => {
    const q = get().hostkeyQueue;
    if (q.length) {
      set({
        hostkey: q[0],
        hostkeyQueue: q.slice(1),
        overlay: { kind: 'modal', id: 'hostkey' },
      });
    } else {
      set({ hostkey: null, overlay: null });
    }
  };
  return {
  folders: [],
  hosts: [], snippets: [], tunnels: [], transientHosts: [],
  sessions: [], active: null, dataTab: 'vaults',
  transfers: [], selectedHost: null,
  metrics: {}, metricsLive: true,
  retry: {}, findOpen: false,
  tunnelState: {},
  pw: null,
  hostkey: null,
  hostkeyQueue: [],
  booted: false,

  async init() {
    if (initStarted) return;
    initStarted = true;
    try {
      const cfg = await api.loadConfig();
      const prefs = normalizePrefs(cfg.preferences);
      set({
        folders: cfg.folders ?? [],
        hosts: normalizeHostTags(cfg.hosts ?? []), snippets: cfg.snippets ?? [], tunnels: cfg.tunnels ?? [],
        ...prefs,
        booted: true,
      });
      applyGpuMode(prefs.gpuMode);
      if (isLang(cfg.preferences?.lang)) i18nSetLang(cfg.preferences.lang);
    } catch { /* keep defaults */ }
    // loadConfig 失败也要 booted：否则启动覆盖层永远不挂载，界面卡死在空白
    if (!get().booted) set({ booted: true });
    // list_sessions 是规格要求的会话入口。启动时对账一次：后端若仍有上次
    // webview 重载遗留的会话，前端已没有对应 xterm 实例、无法重新接管，主动关闭避免
    // 泄漏 PTY / SSH 连接。失败（旧后端 / mock）时忽略，不阻断启动。
    try {
      const live = await api.listSessions();
      const known = new Set(get().sessions.map((x) => x.id));
      for (const orphan of live) {
        if (!known.has(orphan.id)) {
          try { await api.closeSession(orphan.id); } catch { /* ignore */ }
        }
      }
    } catch { /* list_sessions 不可用时忽略 */ }
    // 配置损坏时后端会用默认配置继续启动（不覆盖磁盘），并把原因带在 app info 里；
    // 这里显式提示用户，避免"主机列表突然空了"却不知道为什么。
    try {
      const info = await api.getAppInfo();
      setLowEndDevice(info?.lowEnd);
      // 日志目录由后端下发（<config_dir>/logs），设置面板与打开/清空入口共用。
      set({ appLogsDir: String(info?.logsDir ?? '') });
      if (info?.configError) get().showToast(T('err.configBroken', { 0: info.configError }));
    } catch { /* ignore */ }
    if (useMock && !get().sessions.length) {
      // 预开两个终端标签
      await get().connect(get().hosts[0]?.id ?? '', 'term', true);
      await get().connect(get().hosts[3]?.id ?? '', 'term', true);
      const s = get().sessions[0];
      if (s) set({ active: s.id });
    }
  },

  async reload() {
    try {
      const cfg = await api.loadConfig();
      const tunnels = cfg.tunnels ?? [];
      // 刷新配置时把已不存在的规则的运行态一并清掉（避免幽灵「监听中」）。
      const live = new Set(tunnels.map((t) => t.id));
      const tunnelState: Record<string, { active: boolean; error?: string }> = {};
      for (const [id, v] of Object.entries(get().tunnelState)) if (live.has(id)) tunnelState[id] = v;
      set({ folders: cfg.folders ?? [], hosts: normalizeHostTags(cfg.hosts ?? []), snippets: cfg.snippets ?? [], tunnels, tunnelState });
    } catch { /* ignore */ }
  },
  /* ---------- 会话 ---------- */
  /**
   * 统一连接闸门：所有入口（终端标签 / SFTP / 主机行 / 详情页 / 命令面板）都走这里。
   * 顺序固定：会话去重 → 本地终端跳过认证 → SFTP 复用已有 term 连接 → 密码闸门 → connect。
   * password 只在本函数栈里存活，交给 connect 后即丢弃，绝不写进 host / store。
   */
  async requestConnect(hostId, kind = 'term', password) {
    const s = get();
    // 本地终端（Ctrl+T / port===0 的主机）：跳过 SSH 认证，但仍走同一入口
    if (kind === 'local') {
      const localHost = hostId ? findHostIn(s, hostId) : undefined;
      if (localHost) {
        const ex = s.sessions.find((x) => x.hostId === localHost.id && x.kind === 'local' && x.status !== 'disconnected');
        if (ex) { set({ active: ex.id, selectedHost: localHost.id }); get().closeOverlay(); return true; }
      }
      await get().connectLocal(localHost);
      return true;
    }
    const host = findHostIn(s, hostId);
    if (!host) {
      s.showToast(T('st.noSession'));
      return false;
    }
    // 本地主机（port 0）同样跳过 SSH 认证
    if (host.port === 0) return get().requestConnect(host.id, 'local');
    const realKind: 'term' | 'sftp' = kind;
    // 会话去重：同 host + 同 kind + 未断开 → 切回已有标签，不新开
    const existing = s.sessions.find((x) => x.hostId === host.id
      && (realKind === 'sftp' ? x.kind === 'sftp' : x.kind === 'term')
      && x.status !== 'disconnected');
    if (existing) {
      set({ active: existing.id, selectedHost: host.id });
      get().closeOverlay();
      return true;
    }
    if (realKind === 'sftp') {
      // 后端复用：同主机已有活动终端会话时直接开 SFTP channel，不得重新弹密码
      const live = s.sessions.find((x) => x.hostId === host.id && x.kind === 'term' && x.status !== 'disconnected');
      if (live) return get().connect(host.id, 'sftp');
    }
    // password 且本次没有带密码：先问 keyring 有没有，没有就弹框（此时不建会话）
    if (host.auth === 'password' && !password) {
      // 临时主机（快速连接）没有 keyring 记录：直接弹密码框，不查 has_password
      const saved = isTransient(s, host.id) ? false : await api.hasPassword(host.id).catch(() => false);
      if (!saved) { get().openPasswordPrompt(host.id, realKind); return false; }
    }
    return get().connect(host.id, realKind, false, password);
  },

  async connect(hostId, kind = 'term', quiet = false, password) {
    const host = findHostIn(get(), hostId);
    if (!host) return false;
    // 同一主机 + 同一类型（term / sftp 分别去重）：已有标签直接切回，不重复开。
    // connecting 也算已有；disconnected 不算（连接失败后允许重新开标签）。
    const existing = get().sessions.find((x) =>
      x.hostId === host.id && (kind === 'sftp' ? x.kind === 'sftp' : x.kind === 'term') && x.status !== 'disconnected');
    if (existing) { set({ active: existing.id, selectedHost: host.id }); return true; }

    const transient = isTransient(get(), host.id);
    const prevActive = get().active;
    const pid = pendingId();
    // 后端只会在同 host 已有活动终端时复用连接派生 SFTP；前端据此只级联
    // 这些标签，不能把独立 SFTP 会话一起删掉。
    const derived = kind === 'sftp' && get().sessions.some((x) =>
      x.hostId === host.id && x.kind === 'term' && x.status !== 'disconnected');
    const provisional: Session = {
      id: pid, hostId: host.id, kind,
      title: host.name + (kind === 'sftp' ? ' · SFTP' : ''),
      status: 'connecting', startedAt: Date.now(), encoding: 'UTF-8',
      // 与后端 host.log || log_sessions（session.rs）同一判定：临时主机同样吃全局开关
      log: sessionLogging(get(), host.id),
      derived,
    };
    if (quiet) quietSessions.add(pid);
    pendingKinds.set(host.id, kind);
    // 先出标签并进入 connecting；真实后端 / mock 的 conn://state 决定后续状态
    set({ sessions: [...get().sessions, provisional], active: pid, selectedHost: host.id });
    try {
      // 临时主机走 connect_transient，认证 / 指纹流程与普通主机完全相同，但不写 config
      // 把临时标签 id 作为 attemptId 传给后端，关标签时可中断在途握手/认证。
      const sessionId = transient
        ? await api.connectTransient(host, 120, 32, kind, password, pid)
        : await api.connectSession(host.id, 120, 32, kind, password, pid);
      const alive = get().sessions.find((x) => x.id === pid);
      if (!alive || cancelledConnects.has(pid)) {
        // 用户在握手期间点了「取消」：不创建后台标签，立即关闭刚建立的 SSH 会话
        cancelledConnects.delete(pid);
        quietSessions.delete(pid);
        disposeTerm(sessionId); // 丢弃握手期间缓冲的 term://data
        try { await api.closeSession(sessionId); } catch { /* ignore */ }
        return false;
      }
      const early = earlyConnStates.get(sessionId);
      earlyConnStates.delete(sessionId);
      if (quiet) { quietSessions.delete(pid); quietSessions.add(sessionId); }
      const sessions = get().sessions.map((x) => (x.id === pid
        ? { ...x, id: sessionId, status: early ?? (useMock ? 'connecting' : 'connected') as SessionStatus, error: undefined }
        : x));
      moveTerm(pid, sessionId);
      set({ sessions, active: get().active === pid ? sessionId : get().active });
      pwRetries.delete(host.id);
      // 连接成功后清掉“指纹挑战发生在哪个 kind”的暂存，避免下次确认指纹时用错类型。
      pendingKinds.delete(host.id);
      return true;
    } catch (e) {
      pendingKinds.delete(host.id);
      const code = (e as { code?: string })?.code;
      const detail = (e as { detail?: string })?.detail;
      cancelledConnects.delete(pid);
      quietSessions.delete(pid);
      // 用户取消导致的失败是预期结果——标签已由 closeSession 移除，静默收尾。
      if (code === 'E_CANCELLED') {
        const rest = get().sessions.filter((x) => x.id !== pid);
        const active = get().active === pid
          ? (prevActive && rest.some((x) => x.id === prevActive) ? prevActive : rest[rest.length - 1]?.id ?? null)
          : get().active;
        set({ sessions: rest, active });
        return false;
      }
      if (code === 'E_SSH_HOSTKEY_CHANGED') {
        // 指纹确认 Modal 已弹出；失败的临时标签不留在标签栏，确认后再重新连接
        const rest = get().sessions.filter((x) => x.id !== pid);
        const active = get().active === pid
          ? (prevActive && rest.some((x) => x.id === prevActive) ? prevActive : rest[rest.length - 1]?.id ?? null)
          : get().active;
        set({ sessions: rest, active });
        return false;
      }
      // 认证失败按 detail 分支，不新增错误码
      const kindOfAuth = code === 'E_SSH_AUTH' ? authFailure(detail) : null;
      if (kindOfAuth === 'missing' || kindOfAuth === 'rejected') {
        // 删掉临时标签后必须重算 active（与上面 hostkey 分支一致），
        // 否则 active 指向已不存在的会话，Stage 会渲染成「空状态」且无标签高亮。
        const rest = get().sessions.filter((x) => x.id !== pid);
        const active = get().active === pid
          ? (prevActive && rest.some((x) => x.id === prevActive) ? prevActive : rest[rest.length - 1]?.id ?? null)
          : get().active;
        set({ sessions: rest, active });
        if (kindOfAuth === 'missing') {
          get().openPasswordPrompt(host.id, kind, false);
        } else {
          // 密码被拒绝：允许重试一次并重开同一弹窗（提示密码可能已过期）
          const tries = pwRetries.get(host.id) ?? 0;
          if (tries === 0) {
            pwRetries.set(host.id, 1);
            get().openPasswordPrompt(host.id, kind, true);
          } else {
            pwRetries.delete(host.id);
            get().showToast(T('term.connectFailed', { 0: terr(code, detail) }));
          }
        }
        return false;
      }
      const message = terr(code, detail);
      set({
        sessions: get().sessions.map((x) => (x.id === pid ? { ...x, status: 'disconnected' as SessionStatus, error: message } : x)),
      });
      if (!quiet) {
        // 「未找到可用凭据」是配置问题，不能报成「认证失败」
        get().showToast(T(kindOfAuth === 'nocred' ? 'pw.noCred' : 'term.connectFailed', { 0: message }));
      }
      return false;
    }
  },

  /** Ctrl+T：新建本地终端（ConPTY）；port===0 的主机也走这里，跳过 SSH 认证 */
  async connectLocal(host) {
    const pid = pendingId();
    const provisional: Session = {
      id: pid, hostId: host?.id ?? '', kind: 'local', title: host?.name ?? T('app.localTerm'),
      status: 'connecting', startedAt: Date.now(), encoding: 'UTF-8',
      log: host ? sessionLogging(get(), host.id) : !!get().logSessions,
    };
    set({ sessions: [...get().sessions, provisional], active: pid });
    try {
      const sessionId = await api.connectLocal(120, 32);
      const alive = get().sessions.find((x) => x.id === pid);
      if (!alive) {
        disposeTerm(sessionId);
        try { await api.closeSession(sessionId); } catch { /* ignore */ }
        return;
      }
      const early = earlyConnStates.get(sessionId);
      earlyConnStates.delete(sessionId);
      const sessions = get().sessions.map((x) => (x.id === pid
        ? { ...x, id: sessionId, status: (early ?? 'connected') as SessionStatus, error: undefined }
        : x));
      moveTerm(pid, sessionId);
      set({ sessions, active: get().active === pid ? sessionId : get().active });
    } catch (e) {
      const message = terr((e as { code?: string })?.code, (e as { detail?: string })?.detail);
      set({ sessions: get().sessions.map((x) => (x.id === pid ? { ...x, status: 'disconnected' as SessionStatus, error: message } : x)) });
      get().showToast(T('term.connectFailed', { 0: message }));
    }
  },

  selectHost(id) { set({ selectedHost: id }); },

  /** 片段插入等动作需要「当前终端」：没有活动会话就提示，绝不拿 hosts[0] 兜底建会话
   *  会话必须是 connected——connecting/disconnected 的写入会被静默丢弃，
   *  却仍提示“已写入”，用户以为命令已进输入行。 */
  async ensureTerm() {
    const s = get();
    const isTerm = (x: Session) => x.kind === 'term' || x.kind === 'local' || x.kind === 'peer';
    const cur = s.sessions.find((x) => x.id === s.active && isTerm(x));
    if (cur && cur.status === 'connected') return cur;
    const any = s.sessions.find((x) => isTerm(x) && x.status === 'connected');
    if (any) { set({ active: any.id }); return any; }
    get().showToast(T('st.noSession'));
    return null;
  },

  async openSftpFor(hostId) {
    if (!hostId) { get().showToast(T('st.noSession')); return; }
    await get().requestConnect(hostId, 'sftp');
  },

  /** 密码弹窗：只记录目标主机与 kind，密码明文由组件本地持有。
   *  `reconnectId` 非空表示为「重连」索要密码（submitPassword 走 reconnect）。 */
  openPasswordPrompt(hostId, kind, retry = false, reconnectId) {
    set({ pw: { hostId, kind, retry, reconnectId }, overlay: { kind: 'modal', id: 'password' } });
  },

  async submitPassword(password, remember) {
    const p = get().pw;
    if (!p) return;
    const host = findHostIn(get(), p.hostId);
    set({ overlay: null, pw: null }); // 先关弹窗：明文随组件卸载立即丢弃
    if (!host) return;
    if (!password.trim()) { get().showToast(T('toast.pwEmpty')); get().openPasswordPrompt(p.hostId, p.kind, p.retry, p.reconnectId); return; }
    // 临时主机（快速连接）没有 keyring 记录：remember 无意义，密码只随本次连接下行
    if (remember && !isTransient(get(), host.id)) {
      try {
        await api.setPassword(host.id, password);
      } catch (e) {
        // keyring 写失败：报错并保持「未保存」，不能先把 UI 标成已保存
        get().showToast(terr((e as { code?: string })?.code, (e as { detail?: string })?.detail));
        get().openPasswordPrompt(p.hostId, p.kind, p.retry, p.reconnectId);
        return;
      }
      set({ form: null });
    }
    // 为「重连」索要的密码走 reconnect（沿用标签 / 终端实例 / id 映射），
    // 不新建一条连接。不勾选保存时密码只随这次调用下行，不落任何存储。
    if (p.reconnectId) {
      const ok = await get().reconnect(p.reconnectId, false, password);
      if (ok) get().showToast(T(remember ? 'toast.pwSaved' : 'toast.pwTemp'));
      return;
    }
    const ok = await get().requestConnect(host.id, p.kind, password);
    if (ok) get().showToast(T(remember ? 'toast.pwSaved' : 'toast.pwTemp'));
  },

  /* ---------- 快速连接 ---------- */
  /** 解析 user@host[:port] → 临时 Host → 与普通主机完全相同的 requestConnect 闸门 */
  async quickConnect(input) {
    const parsed = parseQuickConnect(input);
    if (!parsed) { get().showToast(T('pal.quickBad')); return false; }
    const host = buildQuickHost(parsed);
    set({ transientHosts: [...get().transientHosts, host] });
    // 先收起命令面板，再进入统一闸门（指纹 / 密码流程照常弹）
    set({ overlay: null });
    const ok = await get().requestConnect(host.id, 'term');
    // 失败且没有留下指纹挑战 / 会话：丢弃这个临时主机，避免内存里越积越多
    if (!ok && get().hostkey?.hostId !== host.id && !get().sessions.some((x) => x.hostId === host.id)) {
      set({ transientHosts: get().transientHosts.filter((h) => h.id !== host.id) });
    }
    return ok;
  },
  /** 快速连接标签右键「保存为主机」：打开预填好的新建主机表单（提交后成为正式主机） */
  saveQuickHost(sessionId) {
    const s = get();
    const sess = s.sessions.find((x) => x.id === sessionId) ?? s.sessions.find((x) => x.id === s.active);
    const host = sess ? s.transientHosts.find((h) => h.id === sess.hostId) : undefined;
    if (!host) return;
    // id 置空：走「新建主机」表单（不是编辑），提交后由后端生成正式 id
    get().openHostForm({ ...host, id: '', group: defaultGroup(s.folders), log: !!host.log || !!s.logSessions });
  },
  /** 打开日志文件夹（配置目录下的 logs） */
  async openLogFolder() {
    try {
      let dir = get().appLogsDir;
      if (!dir) {
        const info = await api.getAppInfo();
        dir = String(info?.logsDir ?? '');
        if (dir) set({ appLogsDir: dir });
      }
      if (!dir) {
        get().showToast(T('toast.logDirFail', { 0: 'logs_dir 不可用' }));
        return;
      }
      const ok = await openDirectory(dir);
      if (!ok) get().showToast(T('toast.logDirHint', { 0: dir }));
    } catch (e) {
      get().showToast(T('toast.logDirFail', { 0: terr((e as { code?: string })?.code, (e as { detail?: string })?.detail) }));
    }
  },
  /** 清空会话日志前先确认（不可撤销）。 */
  requestClearLogs() {
    get().askConfirm('clearlogs', '', T('del.logs'), T('toast.clearLogsQ'));
  },
  /** 清空会话日志（后端只删 *.log，保留 app.log 诊断日志）。 */
  async clearSessionLogs() {
    try {
      const n = await api.clearSessionLogs();
      get().showToast(T('toast.logsCleared', { 0: n }));
    } catch (e) {
      get().showToast(T('err.deleteFailed', { 0: terr((e as { code?: string })?.code, (e as { detail?: string })?.detail) }));
    }
  },

  /* ---------- 终端内查找 ---------- */
  openTermFind() {
    const s = get();
    const a = s.sessions.find((x) => x.id === s.active);
    if (!a || a.kind === 'sftp') return;
    // 连接中的普通标签还没有 xterm（数据在 pending 缓冲里），不打开查找条
    if (!hasTerm(a.id)) return;
    set({ findOpen: true });
    setTimeout(() => {
      const el = document.querySelector('.termfind input') as HTMLInputElement | null;
      el?.focus();
      el?.select();
    }, 20);
  },
  closeTermFind() {
    const id = get().active;
    if (id) clearTermSearch(id);
    set({ findOpen: false });
    if (id) focusTerm(id);
  },

  async closeOthers(keepId) {
    for (const x of get().sessions.filter((y) => y.id !== keepId)) await get().closeSession(x.id);
    if (get().sessions.some((x) => x.id === keepId)) set({ active: keepId });
  },

  async closeRight(id) {
    const list = get().sessions;
    const i = list.findIndex((x) => x.id === id);
    if (i < 0) return;
    for (const x of list.slice(i + 1)) await get().closeSession(x.id);
  },

  setActive(id) {
    const o = get().overlay;
    // 切换视图即收起右键菜单与终端查找条（查找条只在当前终端标签上出现）
    // 切到别的主机的会话时，远端路径必须跟随切换——按记忆恢复，
    // 没有记忆则清空（ensureRemotePath 会解析该主机的 home/cwd）。
    const target = get().sessions.find((x) => x.id === id);
    const curHost = get().remotePathHostId;
    let remoteReset: Partial<AppStore> = {};
    if (target && curHost !== null && curHost !== target.hostId) {
      const remembered = get().sftpPathByHost[target.hostId];
      remoteReset = remembered !== undefined
        ? { remotePath: remembered, remotePathHostId: target.hostId, selRemote: null }
        : { remotePath: '', remotePathHostId: null, selRemote: null };
    }
    set({
      active: id,
      findOpen: false,
      ...(Object.keys(remoteReset).length ? { ...remoteReset, sftpRev: get().sftpRev + 1 } : {}),
      ...(o?.kind === 'menu' ? { overlay: null, menu: null } : {}),
    });
  },

  async closeSession(id) {
    const sessions = get().sessions;
    const i = sessions.findIndex((x) => x.id === id);
    if (i < 0) return;
    const sess = sessions[i];
    // 级联：关闭“拥有底层连接”的终端时，后端会一并关闭同 host 的派生 SFTP
    // 会话（共享连接）；前端必须同步移除这些标签，否则会留下指向已销毁会话的死标签，
    // selectSftpSessionId 还会把死会话当成 SFTP 操作目标（E_NOT_FOUND）。
    const derivedIds = sess.kind === 'term' && !sess.id.startsWith('pending-')
      ? sessions
          .filter((x) => x.id !== id && x.hostId === sess.hostId && x.kind === 'sftp' && x.derived)
          .map((x) => x.id)
      : [];
    // connecting 中的临时标签被取消：invoke 返回后由 connect() 负责关闭真实会话
    if (sess.status === 'connecting' && sess.id.startsWith('pending-')) cancelledConnects.add(sess.id);
    // 中断在途连接（握手/认证），不让它在后台跑完。
    // attemptId 约定：connect / connect_transient 用临时标签 id，reconnect 用被重连的会话 id。
    if (sess.status === 'connecting') {
      void api.cancelConnect(id).catch(() => { /* 没有在途连接（已建立/已失败）时忽略 */ });
      // 若该主机的指纹弹窗还开着，一并收起：握手已取消，再点"信任"也没有意义。
      if (get().hostkey?.hostId === sess.hostId) {
        const o = get().overlay;
        set({ hostkey: null, ...(o?.kind === 'modal' && o.id === 'hostkey' ? { overlay: null } : {}) });
      }
    }
    quietSessions.delete(id);
    reconnectingSessions.delete(id);
    earlyConnStates.delete(id);
    // 关标签 / 用户主动断开：停止自动重连，并丢弃该会话的临时主机
    clearRetryTimer(id);
    // 在途自动重连的 await 返回后不得再碰这个已关闭会话
    bumpRetryGen(id);
    if (get().retry[id]) {
      const next = { ...get().retry };
      delete next[id];
      set({ retry: next });
    }
    try { await api.stopMetrics(id); } catch { /* ignore */ }
    if (sess.kind === 'screen') {
      // 屏幕查看不是 SshManager 会话：走专用关闭命令，并释放帧总线
      try { await api.peerCloseScreen(id); } catch { /* ignore */ }
      dropScreen(id);
    } else {
      try { await api.closeSession(id); } catch { /* ignore */ }
    }
    disposeTerm(id);
    for (const sid of derivedIds) {
      // 后端在关闭拥有者时已级联清理这些会话；这里只同步前端状态。
      try { await api.stopMetrics(sid); } catch { /* ignore */ }
      disposeTerm(sid);
      clearRetryTimer(sid);
    }
    const removeIds = new Set([id, ...derivedIds]);
    // await 期间可能有新建会话：set 前重取快照，避免把等待期新建的标签一并吞掉
    const rest = get().sessions.filter((x) => !removeIds.has(x.id));
    const near = Math.max(0, Math.min(i - 1, rest.length - 1));
    const curActive = get().active;
    const activeRemoved = !!curActive && removeIds.has(curActive);
    const active = activeRemoved ? (rest.length ? rest[near].id : null) : curActive;
    // 快速连接的临时主机：没有其它会话引用它就从内存里移除（关标签即丢弃）
    const transientHosts = get().transientHosts.filter((h) =>
      rest.some((x) => x.hostId === h.id));
    set({
      sessions: rest, active, transientHosts,
      // 关掉当前标签后若切到相邻终端，查找条不应跟着弹出来（findOpen 是全局状态）
      ...(activeRemoved ? { findOpen: false } : {}),
    });
  },

  /**
   * 重连。返回是否成功：自动重连据此决定下一次退避。
   * silent=true 时抑制 Toast（自动重连自己给一条结果提示，不刷屏）。
   * password：由重连密码弹窗传入的一次性密码，仅 password 认证使用。
   */
  async reconnect(id, silent = false, password?) {
    const s = get().sessions.find((x) => x.id === id);
    if (!s) return false;
    reconnectingSessions.add(id);
    set({ sessions: get().sessions.map((x) => (x.id === id ? { ...x, status: 'connecting' as SessionStatus, error: undefined } : x)) });
    try {
      // 本地重连走 connectLocal 新建 PTY，后端 reconnect 路径不会关闭旧会话：
      // 不先关掉旧 id 会每次重连泄漏一个 ConPTY/子进程句柄。
      if (s.kind === 'local') {
        try { await api.closeSession(id); } catch { /* 已退出 / 已关闭 */ }
      }
      let newId: string;
      try {
        // attemptId 用会话 id（关标签时 closeSession 会 cancelConnect(id)）。
        // password 透传给后端（一次性，不落盘）——重连不必只依赖 keyring。
        newId = s.kind === 'local'
          ? await api.connectLocal(120, 32)
          : s.kind === 'peer'
            ? await api.peerOpenTerminal(s.hostId, 120, 32, id)
            : s.kind === 'screen'
              ? (() => { throw { code: 'E_INVALID_INPUT', detail: T('rd.disconnected') }; })()
              : await api.reconnectSession(id, password, id);
      } catch (e) {
        // 失败的临时标签从未在后端注册；改走一次新的 connect
        if ((e as { code?: string })?.code !== 'E_NOT_FOUND') throw e;
        const host = findHostIn(get(), s.hostId);
        newId = s.kind === 'local'
          ? await api.connectLocal(120, 32)
          : s.kind === 'peer'
            ? await api.peerOpenTerminal(s.hostId, 120, 32, id)
            : host && isTransient(get(), host.id)
              ? await api.connectTransient(host, 120, 32, s.kind === 'sftp' ? 'sftp' : 'term', password, id)
              : await api.connectSession(s.hostId, 120, 32, s.kind === 'sftp' ? 'sftp' : 'term', password, id);
      }
      const alive = get().sessions.find((x) => x.id === id);
      if (!alive) {
        disposeTerm(newId);
        try { await api.closeSession(newId); } catch { /* ignore */ }
        reconnectingSessions.delete(id);
        return false;
      }
      if (newId && newId !== id) {
        const early = earlyConnStates.get(newId);
        earlyConnStates.delete(newId);
        const status = (early ?? (useMock ? 'connecting' : 'connected')) as SessionStatus;
        reconnectingSessions.delete(id);
        // 拥有底层连接的终端重连时，后端会关闭同 host 的派生 SFTP 会话；
        // 前端同步移除这些死标签，避免它们继续被当作 SFTP 操作目标。
        const derivedIds = s.kind === 'term'
          ? get().sessions
              .filter((x) => x.id !== id && x.hostId === s.hostId && x.kind === 'sftp' && x.derived)
              .map((x) => x.id)
          : [];
        for (const sid of derivedIds) {
          try { await api.stopMetrics(sid); } catch { /* ignore */ }
          disposeTerm(sid);
          reconnectingSessions.delete(sid);
        }
        const sessions = get().sessions
          .filter((x) => !derivedIds.includes(x.id))
          .map((x) => (x.id === id
            ? { ...x, id: newId, status, error: undefined, retrying: false, log: sessionLogging(get(), x.hostId) }
            : x));
        moveTerm(id, newId);
        if (reconnectingSessions.has(newId)) reconnectingSessions.delete(newId);
        reconnectingSessions.add(newId);
        const curActive = get().active;
        const nextActive = curActive === id || (!!curActive && derivedIds.includes(curActive))
          ? newId
          : curActive;
        set({ sessions, active: nextActive });
        if (status === 'connected') {
          reconnectingSessions.delete(newId);
          reconnectFailCode.delete(newId);
          if (!silent) get().showToast(T('toast.reconnected'));
        }
        return true;
      }
      set({ sessions: get().sessions.map((x) => (x.id === id ? { ...x, status: 'connected' as SessionStatus, error: undefined, retrying: false, log: sessionLogging(get(), x.hostId) } : x)) });
      reconnectingSessions.delete(id);
      reconnectFailCode.delete(id);
      if (!silent) get().showToast(T('toast.reconnected'));
      return true;
    } catch (e) {
      reconnectingSessions.delete(id);
      const code = (e as { code?: string })?.code;
      // 用户取消（关闭标签）导致的重连失败是预期结果：静默收尾，不提示、不重试。
      if (code === 'E_CANCELLED') {
        reconnectFailCode.delete(id);
        if (!get().sessions.some((x) => x.id === id)) return false;
        set({ sessions: get().sessions.map((x) => (x.id === id ? { ...x, retrying: false } : x)) });
        return false;
      }
      reconnectFailCode.set(id, code);
      const detail = (e as { detail?: string })?.detail;
      // 认证失败（未保存密码 / 密码被拒）不能只弹一条失败 Toast——
      // 打开密码弹窗并绑定「重连目标」，用户输入后走 reconnect 恢复同一标签。
      if (code === 'E_SSH_AUTH') {
        const kindOfAuth = authFailure(detail);
        const host = findHostIn(get(), s.hostId);
        if ((kindOfAuth === 'missing' || kindOfAuth === 'rejected') && host) {
          // 密码被拒只允许重试一次（与 connect 闸门的 pwRetries 语义一致）
          const tries = pwRetries.get(s.hostId) ?? 0;
          if (kindOfAuth === 'missing' || tries === 0) {
            if (kindOfAuth === 'rejected') pwRetries.set(s.hostId, tries + 1);
            get().openPasswordPrompt(s.hostId, s.kind === 'sftp' ? 'sftp' : 'term', kindOfAuth === 'rejected', id);
            set({ sessions: get().sessions.map((x) => (x.id === id ? { ...x, status: 'disconnected' as SessionStatus, retrying: false } : x)) });
            return false;
          }
          pwRetries.delete(s.hostId);
        }
      }
      const message = detail ?? terr(code);
      set({ sessions: get().sessions.map((x) => (x.id === id ? { ...x, status: 'disconnected' as SessionStatus, error: message, retrying: false } : x)) });
      if (!silent) get().showToast(T('err.reconnectFailed', { 0: message }));
      return false;
    }
  },

  /* ---------- 断线自动重连（只在 reason==='lost' 且偏好开启时进入） ---------- */
  scheduleAutoReconnect(id) {
    const cur = get().retry[id];
    const attempt = (cur?.attempt ?? 0) + 1;
    clearRetryTimer(id);
    // 新一轮：让任何仍在 await 的旧轮次收尾失效
    bumpRetryGen(id);
    if (attempt > MAX_RECONNECT_ATTEMPTS) {
      const next = { ...get().retry };
      delete next[id];
      set({
        retry: next,
        sessions: get().sessions.map((x) => (x.id === id ? { ...x, status: 'disconnected' as SessionStatus, retrying: false } : x)),
      });
      get().showToast(T('term.retryFailed'));
      return;
    }
    let remain = reconnectSeconds(attempt);
    set({
      retry: { ...get().retry, [id]: { attempt, remain } },
      // 重试期间标签保持 connecting 样式；终端实例不卸载（scrollback 保留）
      sessions: get().sessions.map((x) => (x.id === id
        ? { ...x, status: 'connecting' as SessionStatus, retrying: true, error: undefined }
        : x)),
    });
    const timer = setInterval(() => {
      const info = get().retry[id];
      if (!info || info.attempt !== attempt) { clearRetryTimer(id); return; }
      if (info.remain <= 1) {
        clearRetryTimer(id);
        set({ retry: { ...get().retry, [id]: { attempt, remain: 0 } } });
        void get().runAutoReconnect(id, attempt);
        return;
      }
      set({ retry: { ...get().retry, [id]: { attempt, remain: info.remain - 1 } } });
    }, 1000);
    retryTimers.set(id, timer);
  },

  async runAutoReconnect(id, attempt) {
    // 倒计时可能被「立即重连」或取消操作抢先替换：只处理当前这一轮
    const cur = get().retry[id];
    if (cur && cur.attempt !== attempt) return;
    const gen = retryGen.get(id) ?? 0;
    // 防重入：倒计时归零后横幅仍可点「立即重连」，上一轮 reconnect 的 await 窗口内
    // 再来一次会并发发起两次 reconnect（重复认证、可能留下无标签会话）。
    if (autoReconnecting.has(id)) return;
    autoReconnecting.add(id);
    try {
      const ok = await get().reconnect(id, true);
      // 用户在这期间取消或重新调度了：本轮结果作废，不写重试状态/不再排下一轮。
      if ((retryGen.get(id) ?? 0) !== gen) return;
      const sess = get().sessions.find((x) => x.id === id);
      if (!sess) { clearRetryTimer(id); const n = { ...get().retry }; delete n[id]; set({ retry: n }); return; }
      if (ok) {
        clearRetryTimer(id);
        const next = { ...get().retry };
        delete next[id];
        set({ retry: next });
        // 成功时若 conn://state(connected) 还没到，标签仍是 connecting；事件到达后 onConnState 收尾
        if (sess.status === 'connected') get().showToast(T('toast.autoReconnected'));
        return;
      }
      const code = reconnectFailCode.get(id);
      // 认证失败不自动重试（密码可能已过期，交给密码 Modal 的一次重试逻辑）
      if (code === 'E_SSH_AUTH') {
        clearRetryTimer(id);
        const next = { ...get().retry };
        delete next[id];
        set({
          retry: next,
          sessions: get().sessions.map((x) => (x.id === id ? { ...x, retrying: false } : x)),
        });
        get().showToast(T('term.retryAuthFail'));
        return;
      }
      get().scheduleAutoReconnect(id);
    } finally {
      autoReconnecting.delete(id);
    }
  },

  retryNow(id) {
    const info = get().retry[id];
    if (!info) return;
    clearRetryTimer(id);
    set({ retry: { ...get().retry, [id]: { attempt: info.attempt, remain: 0 } } });
    void get().runAutoReconnect(id, info.attempt);
  },

  cancelAutoReconnect(id) {
    clearRetryTimer(id);
    // 已有一轮 reconnect 在 await：作废它的收尾与后续调度
    bumpRetryGen(id);
    const next = { ...get().retry };
    delete next[id];
    set({
      retry: next,
      sessions: get().sessions.map((x) => (x.id === id
        ? { ...x, status: x.status === 'connecting' ? ('disconnected' as SessionStatus) : x.status, retrying: false }
        : x)),
    });
  },

  async hostkeyAccept() {
    const hk = get().hostkey;
    if (!hk) return;
    let trusted = false;
    try {
      await api.trustHostKey(hk.sessionId, hk.fingerprint);
      trusted = true;
    } catch {
      // 信任写入失败（挑战多半已超时失效）：不标 known，提示重新连接后再走一遍指纹
      get().showToast(T('hostkey.expired'));
    }
    set({
      // 只有确认写入成功才标记 known，避免「看似已信任、再连还弹」的假状态
      ...(trusted ? {
        hosts: get().hosts.map((x) => (x.id === hk.hostId ? { ...x, known: true } : x)),
        // 快速连接的临时主机：指纹确认后同步 known，避免第二次再弹同一挑战
        transientHosts: get().transientHosts.map((x) => (x.id === hk.hostId ? { ...x, known: true } : x)),
      } : {}),
    });
    advanceHostkey();
    if (trusted) get().showToast(T('toast.fpSaved'));
    // 确认指纹后回到同一闸门：密码闸门 / SFTP 复用 / 认证分支都要再走一遍
    const kind = pendingKinds.get(hk.hostId) ?? 'term';
    pendingKinds.delete(hk.hostId);
    void get().requestConnect(hk.hostId, kind);
  },
  async hostkeyReject() {
    const hk = get().hostkey;
    // 先推进 UI：后端 reject 失败（会话已超时 / 已消失）也不能把用户卡在 Modal 里
    advanceHostkey();
    if (!hk) return;
    pendingKinds.delete(hk.hostId);
    try {
      // 通知后端取消 oneshot，SSH 握手立即中止，不必等 120s 超时
      await api.rejectHostKey(hk.sessionId);
    } catch { /* 后端无挂起校验时 UI 已复位，无需再打扰用户 */ }
    // 快速连接的临时主机只有在"有会话引用"时才需要保留。
    // 拒绝指纹意味着本次连接作废；若没有任何会话引用它，就从内存里回收，
    // 否则每次「快速连接 → 拒绝指纹」都会永久留下一个 Host 对象。
    if (!get().sessions.some((x) => x.hostId === hk.hostId)) {
      set({ transientHosts: get().transientHosts.filter((h) => h.id !== hk.hostId) });
    }
    get().showToast(T('toast.rejected'));
  },

  onConnState(sessionId, state, _latencyMs, reason) {
    const s = get().sessions.find((x) => x.id === sessionId);
    if (!s) {
      // 事件早于 connect() 返回：先落地，等临时 id → 真实 id 的映射建立后回填。
      // 不能永久滞留——关闭标签后迟到的 disconnected 会命中这里，
      // 而该会话已不会再被映射。10s 后仍未消费就丢弃（正常情况下回填在毫秒级完成）。
      earlyConnStates.set(sessionId, state);
      setTimeout(() => {
        if (earlyConnStates.get(sessionId) === state) earlyConnStates.delete(sessionId);
      }, 10_000);
      return;
    }
    // 只有「网络掉线」才自动重连；用户主动断开（closed_by_user / 无 reason）永不触发。
    // 本地终端没有远端链路，不参与自动重连。
    if (state === 'disconnected' && reason === 'lost' && get().autoReconnect
      && s.kind !== 'local' && s.kind !== 'screen'
      && !get().retry[sessionId]) {
      get().scheduleAutoReconnect(sessionId);
      return;
    }
    const was = s.status;
    // reasonless disconnected（term://exit）可能落在重连倒计时期间：retry 还在时保留 retrying，
    // 否则横幅会一边倒计时一边显示矛盾的断线终态
    const keepRetrying = state === 'disconnected' && !!get().retry[sessionId];
    set({
      sessions: get().sessions.map((x) => (x.id === sessionId
        ? {
          ...x,
          status: state,
          error: state === 'connected' ? undefined : x.error,
          retrying: keepRetrying,
          // 「最后响应」时间戳（字段契约 disconnectedAt，TerminalView 渲染用）：
          // 仅在首次进入 disconnected 时记录；connected 时清除
          disconnectedAt: state === 'disconnected'
            ? (was === 'disconnected' ? x.disconnectedAt : Date.now())
            : undefined,
        }
        : x)),
    });
    if (state === 'connected') {
      // 自动重连成功：清倒计时与失败码（重试期间由 runAutoReconnect 负责收尾）
      clearRetryTimer(sessionId);
      if (get().retry[sessionId]) {
        const next = { ...get().retry };
        delete next[sessionId];
        set({ retry: next });
      }
      reconnectFailCode.delete(sessionId);
      if (s.hostId) pwRetries.delete(s.hostId);
      if (reconnectingSessions.delete(sessionId)) { /* 提示由调用方 / 自动重连负责 */ }
      else if (was === 'connecting' && !quietSessions.has(sessionId)) {
        const name = s.hostId ? (findHostIn(get(), s.hostId)?.name ?? s.title) : T('app.localTerm');
        get().showToast(T(s.kind === 'sftp' ? 'toast.sftpConnected' : 'toast.connected', { 0: name }));
      }
      quietSessions.delete(sessionId);
    }
  },
  onHostkeyChallenge(c) {
    const s = get();
    // 已有同 sessionId 的挑战：更新而不是重复排队。
    if (s.hostkey?.sessionId === c.sessionId) { set({ hostkey: c }); return; }
    if (s.hostkeyQueue.some((h) => h.sessionId === c.sessionId)) return;
    if (!s.hostkey) set({ hostkey: c, overlay: { kind: 'modal', id: 'hostkey' } });
    else set({ hostkeyQueue: [...s.hostkeyQueue, c] });
  },
  /** app://error 统一入口（App.tsx 订阅转发到这里）：Toast + 挑战超时收起失效弹窗 */
  onAppError(p) {
    get().showToast(T('app.error', { 0: p?.detail ?? '' }));
    if (p?.code === 'E_HOSTKEY_EXPIRED') {
      // 指纹挑战超时（后端 emit）：只收起对应会话的挑战，不能误关另一个
      // 仍在等待的弹窗。
      const hk = get().hostkey;
      const sid = p.sessionId;
      if (hk && (!sid || sid === hk.sessionId)) {
        advanceHostkey();
      } else if (sid) {
        set({ hostkeyQueue: get().hostkeyQueue.filter((x) => x.sessionId !== sid) });
      }
    }
  },
  onSftpProgress(p) {
    const t0 = get().transfers.find((t) => t.id === p.transferId);
    // 队列项由发起方在收到 transferId 后写入；后端进度可能先到（mock 首帧即发）。
    // 此时若凭空建项会把 transferId 前 6 位当文件名，产生一条多余的幽灵任务。
    // 首帧进度暂存，等真正入队后再由下一次进度补上。
    if (!t0) {
      if (p.state === 'cancelled' || p.state === 'done') return;
      // 10s 内仍未入队（清空队列/传输被丢弃）就淘汰，避免 Map 无界增长
      pendingProgress.set(p.transferId, p);
      schedulePendingProgressDrop(p.transferId, p.state);
      return;
    }
    // 原位替换（map），保持队列顺序稳定；filter + append 会把任务挪到队尾
    set({ transfers: get().transfers.map((t) => (t.id === p.transferId
      ? { ...t, bytesDone: p.bytesDone, bytesTotal: p.bytesTotal || t.bytesTotal, state: p.state }
      : t)) });
    if (p.state === 'done') get().showToast(T(t0.dir === 'up' ? 'toast.upDone' : 'toast.downDone', { 0: t0.name }));
  },
  /** 队列项入队：补上入队前已到达的首帧进度，避免进度条从 0 卡住。 */
  addTransfer(t) {
    const pending = pendingProgress.get(t.id);
    pendingProgress.delete(t.id);
    const timer = pendingProgressTimers.get(t.id);
    if (timer) { clearTimeout(timer); pendingProgressTimers.delete(t.id); }
    const merged = pending
      ? { ...t, bytesDone: pending.bytesDone, bytesTotal: pending.bytesTotal || t.bytesTotal, state: pending.state }
      : t;
    set({ transfers: [...get().transfers, merged] });
  },
  onSftpChanged() { set({ sftpRev: get().sftpRev + 1 }); },
  /** tunnel://state → 运行态（active=真的在监听；error 非空表示监听失败）。 */
  onTunnelState(p) {
    if (!p?.ruleId) return;
    set({
      tunnelState: {
        ...get().tunnelState,
        [p.ruleId]: { active: !!p.active, error: p.error ?? undefined },
      },
    });
  },

  /* ---------- 指标（ConnInfoSheet） ---------- */
  onMetricsSample(m) {
    if (!m || !m.sessionId) return;
    set({ metrics: { ...get().metrics, [m.sessionId]: m } });
  },
  toggleMetricsLive() {
    // 只切偏好；start/stop 由 ConnInfoSheet 的 effect 统一负责（依赖里含 metricsLive），
    // 避免暂停后切标签又被重新 start_metrics。
    const live = !get().metricsLive;
    set({ metricsLive: live });
    get().showToast(live ? T('toast.metricsOn') : T('toast.metricsOff'));
  },

  /* ---------- 编码（EncodingPicker：会话级状态） ---------- */
  async applyEncoding(enc) {
    const s = get().sessions.find((x) => x.id === get().active);
    if (!s) { get().showToast(T('enc.noSession')); return; }
    try {
      await api.setSessionEncoding(s.id, enc);
      set({ sessions: get().sessions.map((x) => (x.id === s.id ? { ...x, encoding: enc } : x)), encOpen: false });
      // 本地插入 dim 提示行（不发给远端，不重排已有缓冲区）
      writeTermData(s.id, new TextEncoder().encode('\r\n\x1b[2m' + T('enc.notice', { 0: enc }) + '\x1b[0m\r\n'));
      get().showToast(T('enc.applied', { 0: enc }));
      // SFTP 会话：文件名按会话编码解码，切编码后重新 list 当前目录
      if (s.kind === 'sftp') get().refreshSftp(true);
    } catch (e) {
      get().showToast(terr((e as { code?: string })?.code, (e as { detail?: string })?.detail));
    }
  },
  async closeAll() {
    // 关闭全部：自动重连计时器与查找状态一并清掉，避免对已销毁会话继续触发
    retryTimers.forEach((t) => clearInterval(t));
    retryTimers.clear();
    // 在途的自动重连 await 返回后必须作废（否则会给已关闭的会话排下一轮）
    for (const s of get().sessions) bumpRetryGen(s.id);
    // 走统一 closeSession：它知道 screen 要走 peerCloseScreen，也会处理
    // 终端派生 SFTP 的级联。直接 api.closeSession 对 screen 是 no-op。
    for (const s of [...get().sessions]) {
      await get().closeSession(s.id);
    }
    const rest = get().sessions;
    set({
      active: rest.some((x) => x.id === get().active) ? get().active : (rest.length ? rest[rest.length - 1].id : null),
      selectedHost: null, retry: {}, findOpen: false,
      transientHosts: get().transientHosts.filter((h) => rest.some((x) => x.hostId === h.id)),
    });
  },


  /* ---------- 命令面板动作表 ---------- */
  };
};
