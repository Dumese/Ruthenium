// AppStore 完整接口（从 useAppStore.ts 原样搬出）。
//
// slice 拆分后接口本身**一字未改**：50 个消费方的 import 与字段访问不受影响；
// 各 slice 文件通过 `StateCreator<AppStore, ...>` 类型约束实现同一接口，
// useAppStore.ts 用 `...createXxxSlice()` 展开组合。字段 / action 的语义注释
// 是契约的一部分，随代码一起搬运，未做改写。
import type {
  Theme, NavStyle, ViewState, SideTab, Overlay, OverlayId, Host, Snippet, TunnelRule,
  Session, Transfer, Preferences, Folder, MenuState, FormDraft, ConfirmState, InputState,
  HostKeyState, ParsedHost, Lang, HostMetrics, PwState, GpuMode, Config,
} from '../types/models';
import type { AppErrorEvent } from '../types/events';
import type * as api from '../ipc/commands';

export interface AppStore {
  // 偏好（持久化）
  lang: Lang;
  theme: Theme; nav: NavStyle; sideTab: SideTab; sidebarWidth: number;
  fontSize: number; lineHeight: number; font: string; cursor: 'block'|'bar';
  showStatus: boolean; showDots: boolean; showLineNumbers: boolean; confirmClose: boolean; restoreTabs: boolean;
  /** 图形加速：system 可用则用 / off 强制禁用（off 需重启完全生效） */
  gpuMode: GpuMode;
  /** 断线自动重连（默认 false） */
  autoReconnect: boolean;
  /** 新主机默认记录会话日志（默认 false） */
  logSessions: boolean;
  /** SSH keepalive 秒数（默认 30；设置里不给 UI，但保存偏好时必须原样透传） */
  keepaliveSecs: number;
  /** 启动时的 Windows Hello 身份验证（默认 false；门禁在 HelloGate，见 security/） */
  requireWindowsHello: boolean;
  /** 局域网可见常开开关（默认 false）；可发现窗口由 peer_open_window 临时开启 */
  lanVisible: boolean;
  lanDeviceName: string;
  /** 监听端口（27410 默认；0 = 自动选） */
  lanPort: number;
  /** 对端 SFTP 根目录（空 = 全盘） */
  peerShareRoot: string;
  /** 严格配对：每次配对都核对 6 位码（默认 false = 一键允许） */
  lanStrictPairing: boolean;
  // 局域网对等层（发现/配对/设备）
  /** 已发现（未配对）设备，由 peer://discovered 事件刷新 */
  discovered: api.DiscoveredDevice[];
  /** 已配对设备（peers.toml 快照） */
  peers: api.PeerRecord[];
  /** 当前配对弹窗（null = 无） */
  pair: Extract<api.PairEvent, { status: 'prompt' }> | null;
  /** 等待确认的配对请求 */
  pairQueue: Extract<api.PairEvent, { status: 'prompt' }>[];
  /** 当前 prompt 的确认请求在途（防双击/重复 IPC） */
  pairBusy: boolean;
  /** 本机发起但尚未收到 Prompt 的 pairingId */
  pendingOutgoing: string[];
  /** 一键模式：本机发起、正在等待对端允许的配对（不占弹窗队列） */
  pairWaiting: Extract<api.PairEvent, { status: 'waiting' }>[];
  /** 配置收件箱（不含内容，只有摘要） */
  inbox: api.InboxItem[];
  /** 控制请求弹窗（null = 无；其余排队） */
  controlReq: api.ControlRequest | null;
  /** 等待确认的控制请求（避免后到请求覆盖先到） */
  controlQueue: api.ControlRequest[];
  /** 控制确认 IPC 在途（防双击批准队列下一个） */
  controlBusy: boolean;
  /** 本机正在被控制的会话（横幅） */
  controlled: api.ControlledSession[];
  /** 屏幕查看请求弹窗（null = 无；其余排队） */
  screenReq: api.ScreenRequest | null;
  /** 等待确认的屏幕请求 */
  screenQueue: api.ScreenRequest[];
  /** 屏幕确认 IPC 在途（防双击批准队列下一个） */
  screenBusy: boolean;
  /** 本机正在被查看的屏幕会话（横幅） */
  viewed: api.ViewedSession[];
  /** 输入通道延迟（sessionId → ms） */
  rtt: Record<string, number>;
  // 瞬时 UI
  collapsed: boolean; focus: boolean; search: string;
  closedGroups: Record<string, boolean>;
  /** 终端查找条：只控制显示，输入 / 计数在组件内 */
  findOpen: boolean;
  /** 自动重连倒计时：sessionId → { attempt, remain } */
  retry: Record<string, { attempt: number; remain: number }>;
  // 数据
  folders: Folder[];
  hosts: Host[]; snippets: Snippet[]; tunnels: TunnelRule[];
  /** 快速连接创建的临时主机：只在内存里，关标签即丢弃，绝不写 hosts.toml */
  transientHosts: Host[];
  // 会话
  sessions: Session[]; active: string | null; dataTab: ViewState;
  transfers: Transfer[];
  /** 侧栏当前选中主机（单击只选中，不弹详情） */
  selectedHost: string | null;
  // 指标（ConnInfoSheet，按 sessionId 缓存）
  metrics: Record<string, HostMetrics>; metricsLive: boolean;
  // 编码选择器
  encOpen: boolean;
  // SFTP（本地根为盘符列表：''）
  localPath: string; remotePath: string; selLocal: string | null; selRemote: string | null;
  /** 当前 remotePath 属于哪个主机（null = 尚未确定）。多主机 SFTP 并存时
   *  按它判断路径归属——切主机必须装载该主机的记忆路径，否则 A 的路径会串到 B。 */
  remotePathHostId: string | null;
  /** 各终端会话的当前工作目录（由 OSC 7 上报，SFTP 默认打开这里） */
  cwdBySession: Record<string, string>;
  /** 每台主机上次浏览的远端目录（再次打开 SFTP 时回到上次位置） */
  sftpPathByHost: Record<string, string>;
  sftpRev: number;
  // 覆盖层
  overlay: Overlay | null; menu: MenuState | null; toast: string | null;
  hostkey: HostKeyState | null;
  hostkeyQueue: HostKeyState[];
  confirm: ConfirmState | null; input: InputState | null;
  /** 连接时的密码弹窗状态（密码明文不在这里，只活在 PasswordModal 组件内） */
  pw: PwState | null;
  palQuery: string; palSel: number;
  form: FormDraft | null; importText: string | null; importParsed: ParsedHost[]; importSelected: Record<number, boolean>;
  /** 会话日志目录（get_app_info.logsDir 下发；打开/清空日志与隐私提示共用） */
  appLogsDir: string;
  /** 转发规则的**运行态**（来自 tunnel://state）。与配置里的 on（意图）分开：
   *  重连瞬间 / 会话未连接时 on=true 但 active=false，UI 必须能区分「已启用待连接」与「监听中」。 */
  tunnelState: Record<string, { active: boolean; error?: string }>;
  booted: boolean;

  // ---- actions ----
  init(): Promise<void>;
  reload(): Promise<void>;
  showToast(msg: string): void;
  dispatch(act: string, arg?: string): void;
  runNamed(n: string): void;
  goto(k: string): void;

  setLang(l: Lang): void;
  setTheme(t: Theme): void; toggleTheme(): void; setNav(v: NavStyle): void;
  setFontSize(v: number): void; setLineHeight(v: number): void; setFont(v: string): void; setCursor(v: 'block'|'bar'): void;
  setToggle(k: 'showStatus'|'showDots'|'showLineNumbers'|'confirmClose'|'autoReconnect'|'logSessions'|'requireWindowsHello'|'lanVisible'|'lanStrictPairing'): void;
  /** 局域网设置（设备名/端口/共享目录）：写入即保存偏好 */
  setLanDeviceName(v: string): void;
  setLanPort(v: number): void;
  setPeerShareRoot(v: string): void;
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
  /** 打开对端屏幕（只看；被查看端会弹本机确认）；withInput 表示同时请求键鼠操作 */
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
  /** 输入延迟事件 */
  inputEnded: Record<string, string>;
  onInputStats(p: { sessionId: string; rttMs: number; ended?: boolean; inputId?: string }): void;
  clearInputEnded(sessionId: string): void;
  /** 查看端剪贴板通道结束（对端断开 / 授权撤销） */
  clipboardEnded: Record<string, string>;
  onClipboardEnded(p: { sessionId: string; id: string }): void;
  clearClipboardEnded(sessionId: string): void;
  /** 图形加速开关：开=system / 关=off；off 立即让已打开终端回退 DOM */
  setGpuMode(v: GpuMode): void;
  setSidebarWidth(v: number): void;
  toggleSidebar(): void; toggleFocus(): void; setSearch(q: string): void;
  toggleGroup(g: string): void; setSideTab(t: SideTab): void;

  openSheet(id: Extract<OverlayId,'hostform'|'hostdetail'|'import'|'backup'|'settings'|'coverage'|'conninfo'|'small'>, arg?: string): void;
  openPalette(): void; closeOverlay(): void;
  askConfirm(fn: string, arg: string, title: string, text: string): void;
  askInput(side: 'local'|'remote', title: string, text: string): void;
  /** 主机页右键「新增分组」：打开命名弹窗 */
  newGroup(): void;
  /** 提交新增分组：落盘 folders 后刷新配置 */
  groupSave(): Promise<void>;
  askRenameGroup(name: string): void;
  renameGroupSave(): Promise<void>;
  askDeleteGroup(name: string): void;
  deleteGroup(name: string): Promise<void>;
  askRename(side: 'local'|'remote', name: string): void;
  confirmYes(): void; inputSave(): void; renameSave(): Promise<void>;
  setPalQuery(q: string): void; palMove(d: number): void; palRun(): void;
  openContextMenu(m: MenuState): void;
  /** 菜单动作：复制路径 / 复制终端选中 / 全选 / 粘贴 / 清屏 */
  copyPathOf(side: 'local'|'remote', name: string): void;
  copyTermSelection(sessionId: string): void;
  selectAllTerminal(sessionId: string): void;
  pasteIntoTerminal(sessionId: string): Promise<void>;
  clearTerm(sessionId: string): void;

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
  setEncOpen(b: boolean): void;
  applyEncoding(enc: string): Promise<void>;
  closeAll(): Promise<void>;

  // 表单
  openHostForm(host?: Host): void;
  openSnipForm(snip?: Snippet): void;
  openRuleForm(rule?: TunnelRule, hostId?: string): void;
  updateForm(key: string, value: unknown): void;
  saveForm(): Promise<void>;
  /** 表单「保存密码」：立即写 keyring；成功后状态切已保存并清空输入 */
  saveFormPassword(): Promise<void>;

  // 主机
  deleteHostById(id: string): Promise<void>;
  /** 拖拽主机到其它分组：乐观更新界面，落盘失败回滚并提示 */
  moveHostToGroup(hostId: string, group: string): Promise<void>;
  copySsh(id: string): void;

  // 导入
  openImport(): void; setImportText(t: string): void;
  parseImportText(): Promise<void>; toggleImportRow(i: number): void;
  importBack(): void; importSelectedRows(): Promise<void>;

  // 备份
  exportBackupJson(): void;
  /** 桌面版：原生文件选择器拿到路径后走 IPC 整体导入 */
  importBackupFile(path: string): void;
  /** 把一份完整 Config 一次性应用到 UI 状态（导入备份后全量刷新） */
  applyImportedConfig(cfg: Config): void;
  applyBackup(json: string): void;

  // 片段 / 转发
  insertSnippet(id: string): Promise<void>;
  deleteSnippetById(id: string): Promise<void>;
  deleteRuleById(id: string): Promise<void>;
  toggleRule(id: string): Promise<void>;

  // SFTP
  setSftpPath(side: 'local'|'remote', path: string): void;
  /** 终端通过 OSC 7 上报的 cwd */
  setSessionCwd(sessionId: string, path: string): void;
  /** 首次进入 SFTP 时决定远端起始目录：cwd → 上次路径 → 服务器 home */
  ensureRemotePath(): Promise<void>;
  selectFile(side: 'local'|'remote', name: string | null): void;
  openDir(side: 'local'|'remote', name: string): void;
  upDir(side: 'local'|'remote'): void;
  refreshSftp(quiet?: boolean): void;
  mkdirSave(): Promise<void>;
  /** 删除文件：target 来自右键菜单，保证删的是右键那一侧（不带则回退到当前选中） */
  deleteSelectedFile(target?: { side: 'local'|'remote'; name: string }): Promise<void>;
  doTransfer(dir: 'up'|'down', opts?: { name?: string; targetPath?: string }): Promise<void>;
  transferDrop(side: 'local'|'remote', name: string, targetPath?: string): Promise<void>;
  /** 从资源管理器拖入的本地文件批量上传（Tauri drag-drop 事件给出真实路径） */
  uploadDroppedFiles(paths: string[], targetDir?: string): Promise<void>;
  clearQueue(): void;
  cancelTransfer(id: string): Promise<void>;

  savePrefs(): void;
}
export type { Preferences, Folder };
