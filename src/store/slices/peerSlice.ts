// 局域网对等层 slice：发现 / 配对 / 设备 / 文件与配置互传 / 远程终端 / 屏幕查看。
import { T, terr } from '../../i18n';
import { dropScreen, pushScreen } from '../../components/screen/screenBus';
import { moveTerm } from '../../components/terminal/useXterm';
import * as api from '../../ipc/commands';
import type { Session, SessionStatus } from '../../types/models';
import type { SliceCreator } from './shared';
import { pendingId } from './guards';

export interface PeerSlice {
  // 局域网对等层（发现/配对/设备）
  /** 已发现（未配对）设备，由 peer://discovered 事件刷新 */
  discovered: api.DiscoveredDevice[];
  /** 已配对设备（peers.toml 快照） */
  peers: api.PeerRecord[];
  /** 当前配对弹窗（null = 无；其余排队） */
  pair: Extract<api.PairEvent, { status: 'prompt' }> | null;
  /** 等待确认的配对请求（避免后到请求覆盖先到） */
  pairQueue: Extract<api.PairEvent, { status: 'prompt' }>[];
  /** 当前 prompt 的确认请求在途（防止双击/重复 IPC） */
  pairBusy: boolean;
  /** 本机发起但尚未收到 Prompt 的 pairingId（用于出站预提示错误提示） */
  pendingOutgoing: string[];
  /** 一键模式：本机发起、正在等待对端允许的配对（轻量提示，不占弹窗队列） */
  pairWaiting: Extract<api.PairEvent, { status: 'waiting' }>[];
  /** 配置收件箱（不含内容，只有摘要） */
  inbox: api.InboxItem[];
  /** 控制请求弹窗（null = 无；其余排队，避免后到请求覆盖先到） */
  controlReq: api.ControlRequest | null;
  controlQueue: api.ControlRequest[];
  /** 确认 IPC 在途：防止双击把队列里的下一个请求也批准/拒绝 */
  controlBusy: boolean;
  /** 本机正在被控制的会话（横幅） */
  controlled: api.ControlledSession[];
  /** 屏幕查看请求弹窗（null = 无；其余排队） */
  screenReq: api.ScreenRequest | null;
  screenQueue: api.ScreenRequest[];
  screenBusy: boolean;
  /** 本机正在被查看的屏幕会话（横幅） */
  viewed: api.ViewedSession[];
  /** 输入通道延迟（sessionId → ms） */
  rtt: Record<string, number>;
  /** 输入通道已断开的屏幕会话（sessionId → 断开的 inputId；前端据此退出操作模式） */
  inputEnded: Record<string, string>;
  /** 查看端剪贴板通道已结束（sessionId → 结束的通道 id） */
  clipboardEnded: Record<string, string>;

  /** 刷新已发现设备（自动打开可发现窗口） */
  refreshDiscovered(): Promise<void>;
  /** 刷新已配对设备 */
  refreshPeers(): Promise<void>;
  /** 按需刷新「对方允许本机」的授权快照（设备页打开时调用） */
  refreshPeerGrants(): Promise<void>;
  /** 发起配对（deviceId 或手填 IP:端口） */
  pairStart(deviceId?: string, addr?: string): Promise<void>;
  pairConfirm(agree: boolean, grants: api.PeerGrants): Promise<void>;
  pairCancel(): void;
  /** 取消一键模式的等待（不占用弹窗队列） */
  pairCancelWaiting(pairingId: string): void;
  /** 配对事件（App 的事件订阅转发） */
  onPeerPairing(e: api.PairEvent): void;
  removePeer(id: string): Promise<void>;
  updatePeerGrants(id: string, grants: api.PeerGrants): Promise<void>;
  /** 打开对端文件互传（SFTP 标签，按 peer id 去重） */
  openPeerSftp(id: string): Promise<void>;
  /** 推送本机配置到对端收件箱 */
  sendPeerConfig(id: string): Promise<void>;
  /** 刷新收件箱列表 */
  refreshInbox(): Promise<void>;
  /** 导入收件箱条目（二次确认由调用方负责） */
  importInbox(id: string): Promise<void>;
  /** 忽略（删除）收件箱条目 */
  deleteInbox(id: string): Promise<void>;
  /** 收到对端推送的配置（App 的事件订阅转发） */
  onPeerInbox(n: api.InboxNotice): void;
  /** 打开对端终端（远程控制；被控端会弹本机确认） */
  openPeerTerminal(id: string): Promise<void>;
  /** 受控端：收到控制请求 */
  onPeerControlRequest(r: api.ControlRequest): void;
  /** 受控端：允许 / 拒绝控制请求 */
  consentControl(allow: boolean): Promise<void>;
  /** 刷新「本机正在被控制」列表（横幅） */
  refreshControlled(): Promise<void>;
  /** 立即断开某条受控会话 */
  kickControlled(sessionId: string): Promise<void>;
  /** 受控会话开始/结束事件（App 的事件订阅转发） */
  onPeerSession(ev: api.ControlSessionEvent): void;
  /** 控制被拒事件（本机拒绝 / Hello 未通过） */
  onPeerDenied(ev: api.PeerDeniedEvent): void;
  /** 打开对端屏幕（只看；被查看端会弹本机确认） */
  openPeerScreen(id: string, withInput?: boolean): Promise<void>;
  /** 受查看端：收到屏幕查看请求 */
  onScreenRequest(r: api.ScreenRequest): void;
  /** 受查看端：允许 / 拒绝屏幕查看 */
  screenConsent(allow: boolean, allowInput?: boolean): Promise<void>;
  /** 刷新「本机正在被查看」列表 */
  refreshViewed(): Promise<void>;
  /** 立即断开某条屏幕查看 */
  kickViewed(sessionId: string): Promise<void>;
  /** 受查看会话开始/结束事件 */
  onScreenSession(ev: api.ScreenSessionEvent): void;
  /** 输入延迟/通道结束事件 */
  onInputStats(p: { sessionId: string; rttMs: number; ended?: boolean; inputId?: string }): void;
  /** 重新开始操作前清除“输入已结束”标记 */
  clearInputEnded(sessionId: string): void;
  /** 查看端剪贴板通道结束事件 */
  onClipboardEnded(p: { sessionId: string; id: string }): void;
  /** 重新开启剪贴板前清除“已结束”标记 */
  clearClipboardEnded(sessionId: string): void;
}

/** 同一次确认的「拒绝」提示只弹一次（本地点击与后端 denied 事件各触发一次）。 */
const deniedToasted = new Map<string, number>();
/** 授权快照刷新防重入（页面焦点 + 60s 定时可能同时触发）。 */
let grantsRefreshing = false;
function firstDeniedToast(consentId: string | null | undefined): boolean {
  if (!consentId) return true;
  const now = Date.now();
  for (const [id, at] of deniedToasted) if (now - at > 30_000) deniedToasted.delete(id);
  if (deniedToasted.has(consentId)) return false;
  deniedToasted.set(consentId, now);
  return true;
}

export const createPeerSlice: SliceCreator<PeerSlice> = (set, get) => {
  /** 按 pairingId 结束一个提示：只有当前项才推进队列；排队项只移除自己。 */
  const settlePair = (id: string): boolean => {
    const s = get();
    const withoutWaiting = s.pairWaiting.filter((p) => p.pairingId !== id);
    if (s.pair?.pairingId === id) {
      set({
        pair: s.pairQueue[0] ?? null,
        pairQueue: s.pairQueue.slice(1),
        pairBusy: false,
        pendingOutgoing: s.pendingOutgoing.filter((x) => x !== id),
        pairWaiting: withoutWaiting,
      });
      return true;
    }
    if (s.pairQueue.some((p) => p.pairingId === id)) {
      set({
        pairQueue: s.pairQueue.filter((p) => p.pairingId !== id),
        pendingOutgoing: s.pendingOutgoing.filter((x) => x !== id),
        pairWaiting: withoutWaiting,
      });
      return true;
    }
    if (withoutWaiting.length !== s.pairWaiting.length) {
      set({
        pendingOutgoing: s.pendingOutgoing.filter((x) => x !== id),
        pairWaiting: withoutWaiting,
      });
      return true;
    }
    return false;
  };

  return {
  discovered: [], peers: [], pair: null, pairQueue: [], pairBusy: false, pendingOutgoing: [], pairWaiting: [], inbox: [], controlReq: null, controlQueue: [], controlBusy: false, controlled: [], screenReq: null, screenQueue: [], screenBusy: false, viewed: [], rtt: {}, inputEnded: {}, clipboardEnded: {},

  /* ---------- 局域网对等层（发现/配对/设备） ---------- */
  async refreshDiscovered() {
    try {
      set({ discovered: await api.peerDiscover() });
    } catch { /* 未开启/浏览器环境：保持现状 */ }
  },
  async refreshPeers() {
    try {
      set({ peers: await api.peerList() });
    } catch { /* ignore */ }
  },
  async refreshPeerGrants() {
    if (grantsRefreshing) return;
    grantsRefreshing = true;
    try {
      const peers = get().peers;
      if (peers.length === 0) return;
      // 授权在对方实时判定，但本地快照只在部分连接时更新；设备页打开时并发刷新一次。
      // 对端离线或版本较旧（不支持该命令）时失败静默，保持现有快照即可。
      const results = await Promise.allSettled(
        peers.map(async (p) => ({ id: p.id, rec: await api.peerRefreshGrants(p.id) })),
      );
      const updates = new Map<string, api.PeerRecord>();
      for (const r of results) {
        if (r.status === 'fulfilled' && r.value.rec) updates.set(r.value.id, r.value.rec);
      }
      if (updates.size > 0) {
        set({ peers: get().peers.map((p) => updates.get(p.id) ?? p) });
      }
    } finally {
      grantsRefreshing = false;
    }
  },
  async pairStart(deviceId, addr) {
    try {
      const id = await api.peerPairStart({ deviceId, addr });
      const s = get();
      // Prompt 事件可能先于 IPC 返回到达；已在当前/队列里就不要再记为 pending。
      if (s.pair?.pairingId !== id && !s.pairQueue.some((p) => p.pairingId === id)) {
        set({ pendingOutgoing: [...s.pendingOutgoing, id] });
      }
    } catch (e) {
      get().showToast(T('err.pairFail', { 0: terr((e as { code?: string })?.code, (e as { detail?: string })?.detail) }));
    }
  },
  async pairConfirm(agree, grants) {
    const p = get().pair;
    if (!p || get().pairBusy) return;
    const id = p.pairingId;
    set({ pairBusy: true });
    // 决策已送达后端时保持 busy，直到该 id 的终止事件（Ok/Rejected/Timeout/Error）
    // 或倒计时自动取消；否则按钮会重新可点，第二次 IPC 会被误当成超时。
    let awaitingTerminal = false;
    try {
      const applied = await api.peerPairConfirm(id, agree, grants);
      // 会话可能已超时/被取消：后端没命中时不会再有任何事件，弹窗必须自己收掉。
      // 只结算本次 id；期间新到的 prompt 不能被旧队列覆盖。
      if (!applied) {
        const wasCurrent = get().pair?.pairingId === id;
        settlePair(id);
        if (wasCurrent) get().showToast(T('toast.pairTimeout'));
      } else {
        awaitingTerminal = true;
      }
    } catch (e) {
      const wasCurrent = get().pair?.pairingId === id;
      settlePair(id);
      if (wasCurrent) {
        get().showToast(T('err.pairFail', { 0: terr((e as { code?: string })?.code, (e as { detail?: string })?.detail) }));
      }
    } finally {
      if (!awaitingTerminal && get().pair?.pairingId === id) set({ pairBusy: false });
    }
  },
  pairCancel() {
    const p = get().pair;
    if (!p || get().pairBusy) return;
    const id = p.pairingId;
    settlePair(id);
    void api.peerPairCancel(id).catch(() => { /* ignore */ });
  },
  pairCancelWaiting(pairingId) {
    const s = get();
    if (!s.pairWaiting.some((p) => p.pairingId === pairingId)) return;
    // 先从 UI 移除；后端的 rejected(canceled) 事件不会再命中任何提示。
    set({ pairWaiting: s.pairWaiting.filter((p) => p.pairingId !== pairingId) });
    void api.peerPairCancel(pairingId).catch(() => { /* ignore */ });
  },
  onPeerPairing(e) {
    if (e.status === 'waiting') {
      const s = get();
      if (s.pairWaiting.some((p) => p.pairingId === e.pairingId)) return;
      set({
        pairWaiting: [...s.pairWaiting, e],
        pendingOutgoing: s.pendingOutgoing.filter((x) => x !== e.pairingId),
      });
      return;
    }
    if (e.status === 'prompt') {
      const s = get();
      // 同时收到多个配对请求时排队，不能覆盖先到的（第一个会一直等到超时）。
      // 同一个 id 的重复事件直接忽略。
      if (s.pair?.pairingId === e.pairingId || s.pairQueue.some((p) => p.pairingId === e.pairingId)) {
        return;
      }
      const pendingOutgoing = s.pendingOutgoing.filter((x) => x !== e.pairingId);
      if (!s.pair) set({ pair: e, pairBusy: false, pendingOutgoing });
      else set({ pairQueue: [...s.pairQueue, e], pendingOutgoing });
      return;
    }

    const s = get();
    const id = e.pairingId;
    const wasCurrent = s.pair?.pairingId === id;
    const inQueue = s.pairQueue.some((p) => p.pairingId === id);
    const wasPending = s.pendingOutgoing.includes(id);
    const wasWaiting = s.pairWaiting.some((p) => p.pairingId === id);

    if (e.status === 'ok') {
      set({
        peers: [...get().peers.filter((p) => p.id !== e.peer.id), e.peer],
        pendingOutgoing: s.pendingOutgoing.filter((x) => x !== id),
      });
      // 成功事件必须更新设备列表（即使弹窗已被取消/推进）；只结算匹配的提示。
      if (wasCurrent || inQueue || wasWaiting) settlePair(id);
      get().showToast(T('toast.paired', { 0: e.peer.name }));
      void get().refreshDiscovered();
      return;
    }

    // 未知 id = 入站预提示失败/扫描噪声：不得影响当前弹窗，也不弹 Toast。
    if (!wasCurrent && !inQueue && !wasPending && !wasWaiting) return;

    if (wasCurrent || wasWaiting) {
      settlePair(id);
    } else {
      // 排队项失败只移除自己；pending 项（出站预提示失败）只清 pending。
      set({ pendingOutgoing: s.pendingOutgoing.filter((x) => x !== id) });
      if (inQueue) set({ pairQueue: s.pairQueue.filter((p) => p.pairingId !== id) });
    }
    if (e.status === 'rejected') get().showToast(T('toast.pairRejected'));
    else if (e.status === 'timeout') get().showToast(T('toast.pairTimeout'));
    else if (e.status === 'error') get().showToast(T('err.pairFail', { 0: e.detail }));
  },
  async removePeer(id) {
    try {
      await api.peerRemove(id);
      set({ peers: get().peers.filter((p) => p.id !== id) });
      // 解除后本机仍可能持有该设备的控制/屏幕/SFTP 标签；立即关闭，
      // 不要只等后端周期复核。
      for (const s of get().sessions.filter((x) => x.hostId === id)) {
        await get().closeSession(s.id);
      }
    } catch (e) {
      get().showToast(T('err.pairFail', { 0: terr((e as { code?: string })?.code, (e as { detail?: string })?.detail) }));
    }
  },
  async updatePeerGrants(id, grants) {
    const prev = get().peers.find((p) => p.id === id);
    if (!prev) return;
    const optimistic = {
      ...prev,
      grantFiles: grants.files,
      grantControl: grants.control,
      grantConfig: grants.config,
      grantScreen: grants.screen,
      grantClipboard: grants.clipboard,
      grantAudio: grants.audio,
    };
    // 乐观更新：连续勾选时第二个请求必须基于第一个的新值，而不是旧渲染快照。
    set({ peers: get().peers.map((p) => (p.id === id ? optimistic : p)) });
    try {
      const r = await api.peerUpdateGrants(id, grants);
      if (r) set({ peers: get().peers.map((p) => (p.id === id ? r : p)) });
    } catch (e) {
      set({ peers: get().peers.map((p) => (p.id === id ? prev : p)) });
      get().showToast(T('err.pairFail', { 0: terr((e as { code?: string })?.code, (e as { detail?: string })?.detail) }));
    }
  },

  /* ---------- 文件互传 / 配置互传 ---------- */
  async openPeerSftp(id) {
    const peer = get().peers.find((p) => p.id === id);
    if (!peer) { get().showToast(T('st.noSession')); return; }
    const existing = get().sessions.find((x) => x.hostId === id && x.kind === 'sftp' && x.status !== 'disconnected');
    if (existing) { set({ active: existing.id, selectedHost: id }); return; }
    // 先出「连接中」标签：对端连接可能要 1-2 秒（钉扎握手 + 身份认证）
    const pid = pendingId();
    const provisional: Session = {
      id: pid, hostId: id, kind: 'sftp', title: `${peer.name} · SFTP`,
      status: 'connecting', startedAt: Date.now(), encoding: 'UTF-8', log: false,
    };
    set({ sessions: [...get().sessions, provisional], active: pid, selectedHost: id });
    try {
      const sessionId = await api.peerOpenSftp(id);
      if (!get().sessions.some((x) => x.id === pid)) {
        // 用户在连接期间关掉了临时标签：后端可能已建立会话，必须回收。
        try { await api.closeSession(sessionId); } catch { /* ignore */ }
        return;
      }
      const sessions = get().sessions.map((x) => (x.id === pid
        ? { ...x, id: sessionId, status: 'connected' as SessionStatus, error: undefined }
        : x));
      set({ sessions, active: get().active === pid ? sessionId : get().active });
    } catch (e) {
      set({ sessions: get().sessions.filter((x) => x.id !== pid), active: get().active === pid ? null : get().active });
      get().showToast(T('err.peerSftpFail', {
        0: terr((e as { code?: string })?.code, (e as { detail?: string })?.detail ?? (e as Error)?.message),
      }));
    }
  },
  async sendPeerConfig(id) {
    const peer = get().peers.find((p) => p.id === id);
    try {
      await api.peerSendConfig(id);
      get().showToast(T('toast.configSent', { 0: peer?.name ?? '' }));
    } catch (e) {
      get().showToast(T('err.configSendFail', {
        0: terr((e as { code?: string })?.code, (e as { detail?: string })?.detail ?? (e as Error)?.message),
      }));
    }
  },
  async refreshInbox() {
    try {
      set({ inbox: await api.peerInbox() });
    } catch { /* 收件箱不可读时保持现状 */ }
  },
  async importInbox(id) {
    try {
      const cfg = await api.peerImportInbox(id);
      get().applyImportedConfig(cfg);
      set({ inbox: get().inbox.filter((x) => x.id !== id) });
      get().showToast(T('toast.inboxImported', { 0: String((cfg.hosts ?? []).length) }));
    } catch (e) {
      get().showToast(T('err.inboxImportFail', {
        0: terr((e as { code?: string })?.code, (e as { detail?: string })?.detail ?? (e as Error)?.message),
      }));
    }
  },
  async deleteInbox(id) {
    try {
      await api.peerDeleteInbox(id);
      set({ inbox: get().inbox.filter((x) => x.id !== id) });
    } catch (e) {
      get().showToast(T('err.inboxImportFail', {
        0: terr((e as { code?: string })?.code, (e as { detail?: string })?.detail ?? (e as Error)?.message),
      }));
    }
  },
  onPeerInbox(n) {
    set({ inbox: [n.item, ...get().inbox.filter((x) => x.id !== n.item.id)] });
    get().showToast(T('toast.inboxNew', { 0: n.item.from, 1: String(n.item.hosts) }));
  },

  /* ---------- 终端控制 ---------- */
  async openPeerTerminal(id) {
    const peer = get().peers.find((p) => p.id === id);
    if (!peer) { get().showToast(T('st.noSession')); return; }
    const existing = get().sessions.find((x) => x.hostId === id && x.kind === 'peer' && x.status !== 'disconnected');
    if (existing) { set({ active: existing.id, selectedHost: id }); return; }
    const pid = pendingId();
    const provisional: Session = {
      id: pid, hostId: id, kind: 'peer', title: `${peer.name} · ${T('peers.control')}`,
      status: 'connecting', startedAt: Date.now(), encoding: 'UTF-8', log: false,
    };
    set({ sessions: [...get().sessions, provisional], active: pid, selectedHost: id });
    try {
      const sessionId = await api.peerOpenTerminal(id, 120, 32, pid);
      if (!get().sessions.some((x) => x.id === pid)) {
        try { await api.closeSession(sessionId); } catch { /* ignore */ }
        return;
      }
      const sessions = get().sessions.map((x) => (x.id === pid
        ? { ...x, id: sessionId, status: 'connected' as SessionStatus, error: undefined }
        : x));
      set({ sessions, active: get().active === pid ? sessionId : get().active });
      moveTerm(pid, sessionId);
    } catch (e) {
      set({ sessions: get().sessions.filter((x) => x.id !== pid), active: get().active === pid ? null : get().active });
      get().showToast(T('err.peerControlFail', {
        0: terr((e as { code?: string })?.code, (e as { detail?: string })?.detail ?? (e as Error)?.message),
      }));
    }
  },
  onPeerControlRequest(r) {
    // 单槽会让第二个请求覆盖第一个（第一个只能等超时）；排队逐个确认
    if (!get().controlReq) set({ controlReq: r });
    else set({ controlQueue: [...get().controlQueue, r] });
  },
  async consentControl(allow) {
    const r = get().controlReq;
    if (!r || get().controlBusy) return;
    set({ controlBusy: true });
    try {
      await api.peerControlConsent(r.consentId, allow);
      if (!allow && firstDeniedToast(r.consentId)) {
        get().showToast(T('toast.controlDenied', { 0: r.peerName }));
      }
      // 只有本次请求仍是当前项才推进队列；期间 denied 事件可能已处理。
      if (get().controlReq?.consentId === r.consentId) {
        const q = get().controlQueue;
        set({ controlReq: q[0] ?? null, controlQueue: q.slice(1) });
      }
    } catch (e) {
      get().showToast(T('err.peerControlFail', {
        0: terr((e as { code?: string })?.code, (e as { detail?: string })?.detail ?? (e as Error)?.message),
      }));
      if (get().controlReq?.consentId === r.consentId) {
        const q = get().controlQueue;
        set({ controlReq: q[0] ?? null, controlQueue: q.slice(1) });
      }
    } finally {
      set({ controlBusy: false });
    }
  },
  async refreshControlled() {
    try {
      set({ controlled: await api.peerControlSessions() });
    } catch { /* ignore */ }
  },
  async kickControlled(sessionId) {
    try {
      await api.peerKickSession(sessionId);
      set({ controlled: get().controlled.filter((x) => x.sessionId !== sessionId) });
      get().showToast(T('toast.controlKicked'));
    } catch (e) {
      get().showToast(T('err.peerControlFail', {
        0: terr((e as { code?: string })?.code, (e as { detail?: string })?.detail ?? (e as Error)?.message),
      }));
    }
  },
  onPeerSession(ev) {
    if (ev.action === 'started') {
      const entry: api.ControlledSession = {
        sessionId: ev.sessionId,
        peerName: ev.peerName,
        peerAddr: ev.addr ?? '',
        cols: ev.cols ?? 0,
        rows: ev.rows ?? 0,
        startedAt: '',
        startedMs: ev.startedMs ?? Date.now(),
        elapsedSec: 0,
      };
      set({ controlled: [...get().controlled.filter((x) => x.sessionId !== ev.sessionId), entry] });
      get().showToast(T('toast.controlledStarted', { 0: ev.peerName }));
    } else {
      set({ controlled: get().controlled.filter((x) => x.sessionId !== ev.sessionId) });
      get().showToast(T('toast.controlledEnded', { 0: ev.peerName }));
      void get().refreshControlled();
    }
  },
  onPeerDenied(ev) {
    const isScreen = ev.kind === 'screen';
    // 超时/Hello 失败都带 consentId：关闭对应弹窗（或把它从队列里摘掉），
    // 队列里的下一个请求立刻顶上。
    if (isScreen) {
      const cur = get().screenReq;
      if (cur && (!ev.consentId || cur.consentId === ev.consentId)) {
        const q = get().screenQueue;
        set({ screenReq: q[0] ?? null, screenQueue: q.slice(1) });
      } else if (ev.consentId) {
        set({ screenQueue: get().screenQueue.filter((x) => x.consentId !== ev.consentId) });
      }
    } else {
      const cur = get().controlReq;
      if (cur && (!ev.consentId || cur.consentId === ev.consentId)) {
        const q = get().controlQueue;
        set({ controlReq: q[0] ?? null, controlQueue: q.slice(1) });
      } else if (ev.consentId) {
        set({ controlQueue: get().controlQueue.filter((x) => x.consentId !== ev.consentId) });
      }
    }
    const key = ev.reason === 'hello'
      ? (isScreen ? 'toast.screenHelloDenied' : 'toast.controlHelloDenied')
      : (isScreen ? 'toast.screenDenied' : 'toast.controlDenied');
    if (firstDeniedToast(ev.consentId)) {
      get().showToast(T(key, { 0: ev.peerName }));
    }
  },

  /* ---------- 远程桌面（只看） ---------- */
  async openPeerScreen(id, withInput = false) {
    const peer = get().peers.find((p) => p.id === id);
    if (!peer) { get().showToast(T('st.noSession')); return; }
    const existing = get().sessions.find((x) => x.hostId === id && x.kind === 'screen' && x.status !== 'disconnected');
    if (existing) { set({ active: existing.id, selectedHost: id }); return; }
    const pid = pendingId();
    const provisional: Session = {
      id: pid, hostId: id, kind: 'screen', title: `${peer.name} · ${T('rd.screenTab')}`,
      status: 'connecting', startedAt: Date.now(), encoding: 'UTF-8', log: false,
    };
    set({ sessions: [...get().sessions, provisional], active: pid, selectedHost: id });
    try {
      const ch = api.screenFrameChannel((f) => pushScreen(f));
      const sessionId = await api.peerOpenScreen(id, 65, ch, withInput);
      if (!get().sessions.some((x) => x.id === pid)) {
        try { await api.peerCloseScreen(sessionId); } catch { /* ignore */ }
        dropScreen(sessionId);
        return;
      }
      const sessions = get().sessions.map((x) => (x.id === pid
        ? { ...x, id: sessionId, status: 'connected' as SessionStatus, error: undefined }
        : x));
      set({ sessions, active: get().active === pid ? sessionId : get().active });
    } catch (e) {
      dropScreen(pid);
      set({ sessions: get().sessions.filter((x) => x.id !== pid), active: get().active === pid ? null : get().active });
      get().showToast(T('err.peerScreenFail', {
        0: terr((e as { code?: string })?.code, (e as { detail?: string })?.detail ?? (e as Error)?.message),
      }));
    }
  },
  onScreenRequest(r) {
    if (!get().screenReq) set({ screenReq: r });
    else set({ screenQueue: [...get().screenQueue, r] });
  },
  async screenConsent(allow, allowInput = false) {
    const r = get().screenReq;
    if (!r || get().screenBusy) return;
    set({ screenBusy: true });
    // 在点击瞬间取值，避免队列推进后复用到下一个请求的旧 allowInput。
    const allowInputNow = r.inputOnly ? true : allowInput;
    try {
      await api.peerScreenConsent(r.consentId, allow, allowInputNow);
      if (!allow && firstDeniedToast(r.consentId)) {
        get().showToast(T('toast.screenDenied', { 0: r.peerName }));
      }
      if (get().screenReq?.consentId === r.consentId) {
        const q = get().screenQueue;
        set({ screenReq: q[0] ?? null, screenQueue: q.slice(1) });
      }
    } catch (e) {
      get().showToast(T('err.peerScreenFail', {
        0: terr((e as { code?: string })?.code, (e as { detail?: string })?.detail ?? (e as Error)?.message),
      }));
      if (get().screenReq?.consentId === r.consentId) {
        const q = get().screenQueue;
        set({ screenReq: q[0] ?? null, screenQueue: q.slice(1) });
      }
    } finally {
      set({ screenBusy: false });
    }
  },
  async refreshViewed() {
    try {
      set({ viewed: await api.peerScreenSessions() });
    } catch { /* ignore */ }
  },
  async kickViewed(sessionId) {
    try {
      await api.peerScreenKick(sessionId);
      set({ viewed: get().viewed.filter((x) => x.sessionId !== sessionId) });
      get().showToast(T('toast.screenKicked'));
    } catch (e) {
      get().showToast(T('err.peerScreenFail', {
        0: terr((e as { code?: string })?.code, (e as { detail?: string })?.detail ?? (e as Error)?.message),
      }));
    }
  },
  onInputStats(p) {
    if (p.ended) {
      const rtt = { ...get().rtt };
      delete rtt[p.sessionId];
      set({
        rtt,
        inputEnded: { ...get().inputEnded, [p.sessionId]: p.inputId ?? '' },
      });
      return;
    }
    set({ rtt: { ...get().rtt, [p.sessionId]: p.rttMs } });
  },
  clearInputEnded(sessionId) {
    if (!(sessionId in get().inputEnded)) return;
    const next = { ...get().inputEnded };
    delete next[sessionId];
    set({ inputEnded: next });
  },
  onClipboardEnded(p) {
    set({ clipboardEnded: { ...get().clipboardEnded, [p.sessionId]: p.id } });
  },
  clearClipboardEnded(sessionId) {
    if (!(sessionId in get().clipboardEnded)) return;
    const next = { ...get().clipboardEnded };
    delete next[sessionId];
    set({ clipboardEnded: next });
  },
  onScreenSession(ev) {
    if (ev.action === 'started') {
      const entry: api.ViewedSession = {
        sessionId: ev.sessionId,
        peerName: ev.peerName,
        peerAddr: '',
        quality: 65,
        monitor: 0,
        startedMs: Date.now(),
        elapsedSec: 0,
      };
      set({ viewed: [...get().viewed.filter((x) => x.sessionId !== ev.sessionId), entry] });
      get().showToast(T('toast.screenStarted', { 0: ev.peerName }));
    } else {
      set({ viewed: get().viewed.filter((x) => x.sessionId !== ev.sessionId) });
      get().showToast(T('toast.screenEnded', { 0: ev.peerName }));
      void get().refreshViewed();
    }
  },
  };
};
