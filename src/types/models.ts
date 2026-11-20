// TypeScript 镜像（与 Rust 一一镜像，camelCase）
export type Theme = 'light' | 'dark' | 'system';
/** 图形加速：没有 on，只有 system（可用则用）与 off（强制禁用） */
export type GpuMode = 'system' | 'off';
export type NavStyle = 'classic' | 'x';
export type ViewState = 'terminal' | 'sftp' | 'screen' | 'empty' | 'vaults' | 'snipsPage' | 'rulesPage' | 'sftpPage' | 'peers';
export type OverlayId =
  | 'hostform' | 'hostdetail' | 'import' | 'backup' | 'settings' | 'coverage' | 'conninfo' | 'small'
  | 'hostkey' | 'confirm' | 'input' | 'password' | 'palette' | 'menu';
export type Overlay =
  | { kind: 'sheet';   id: Extract<OverlayId,'hostform'|'hostdetail'|'import'|'backup'|'settings'|'coverage'|'conninfo'|'small'>; arg?: string }
  | { kind: 'modal';   id: 'hostkey' | 'confirm' | 'input' | 'password' }
  | { kind: 'palette' }
  | { kind: 'menu';    x: number; y: number; items: MenuItem[] };

export interface Host { id:string; name:string; addr:string; user:string; port:number;
  group:string; /** 主机标签：dev/test/prod/bastion 为内置项，其它值原样保留 */ tag:string; keyRef:string;
  auth:AuthMethod; known:boolean; agent:boolean; note:string;
  /** 连接后自动执行的一行命令（默认空串；PTY 就绪后原样写入 channel） */
  initCommand:string;
  /** 记录会话日志（默认 false；全局默认值来自 Preferences.logSessions） */
  log:boolean;
  /** 仅前端标记：快速连接创建的临时主机不写入 hosts.toml */
  persist?:boolean }
export interface Snippet { id:string; name:string; cmd:string; tag:string }
export interface TunnelRule { id:string; name:string; kind:'local'|'remote';
  local:string; remote:string; host:string; on:boolean }
export type Lang = 'zh-CN' | 'zh-TW' | 'en' | 'ja';
/** 认证方式：随 hosts.toml 落盘；是否已存密码不落盘，一律问 has_password */
export type AuthMethod = 'key' | 'password' | 'agent';
export interface Preferences { lang:Lang; theme:Theme; nav:NavStyle; fontSize:number; font:string;
  /** 终端行高（字号倍数，1.0–2.0，默认 1.3）；渲染时换算成 xterm 的 lineHeight 系数 */
  lineHeight:number;
  cursor:'block'|'bar'; showStatus:boolean; showDots:boolean; showLineNumbers:boolean; confirmClose:boolean;
  restoreTabs:boolean; sidebarWidth:number;
  /** 图形加速（TOML `gpuMode`，默认 system；off 需重启 webview 完全生效） */
  gpuMode:GpuMode;
  /** 断线自动重连（默认 false；策略固定 1/2/4/8/16s，最多 5 次） */
  autoReconnect:boolean;
  /** 新主机「记录会话日志」的默认值（默认 false） */
  logSessions:boolean;
  /** SSH keepalive 间隔秒数（TOML `keepaliveSecs`，默认 30；0=禁用）。
   *  配置只有一份 `hosts.toml`（可手改），前端对它「原样透传」，不擅自改写用户设置。 */
  keepaliveSecs:number;
  /** 启动时的 Windows Hello 身份验证（TOML `requireWindowsHello`，默认 false）。
   *  开启后由 HelloGate 在加载配置与会话之前拦截，验证通过才挂载主界面。 */
  requireWindowsHello:boolean;
  /** 局域网可见常开开关（TOML `lanVisible`，默认 false）：平时不监听、不广播 */
  lanVisible:boolean;
  /** 设备名（TOML `lanDeviceName`，空 = 主机名） */
  lanDeviceName:string;
  /** 局域网 SSH 监听端口（TOML `lanPort`，默认 27410；0 = 自动选） */
  lanPort:number;
  /** 对端 SFTP 访问根目录（TOML `peerShareRoot`，空 = 全盘） */
  peerShareRoot:string;
  /** 严格配对：每次配对都核对 6 位码（TOML `lanStrictPairing`，默认 false = 一键允许） */
  lanStrictPairing:boolean }
/** 会话类型：远端终端 / SFTP / 本地终端（ConPTY，hostId 为空串） */
export type SessionKind = 'term' | 'sftp' | 'local' | 'peer' | 'screen';
/** 会话级连接状态：断线互不影响，标签点 / 侧栏 / 状态栏都以此为准 */
export type SessionStatus = 'connecting' | 'connected' | 'disconnected';
export interface Session { id:string; hostId:string; kind:SessionKind; title:string;
  status:SessionStatus; startedAt:number; encoding:string; error?:string;
  /** 自动重连进行中：标签保持 connecting 样式，终端不卸载，横幅显示倒计时 */
  retrying?:boolean;
  /** 最近一次进入 disconnected 的时间戳（ms；重连成功清除）。「最后响应」渲染用 */
  disconnectedAt?:number;
  /** 该会话是否在记录本地日志（连接时按 Host.log 快照；本地终端取全局默认） */
  log?:boolean;
  /** SFTP 是否复用同一 host 的终端连接（派生会话）。关闭终端时只级联这些标签。 */
  derived?:boolean }
/** `list_sessions` 的真实返回形状（后端 models::Session）：只是摘要，
 *  没有 title/status/error —— 不能用 `Session[]` 承接，那些字段只会是 undefined。 */
export interface SessionSummary { id:string; hostId:string; kind:SessionKind;
  connected:boolean; startedAt:number; encoding:string }
export interface Transfer { id:string; name:string; dir:'up'|'down'; bytesDone:number;
  bytesTotal:number; state:'running'|'done'|'error'|'cancelled' }

// ---- 远端指标快照（HostMetrics 镜像；不落盘，仅内存） ----
export interface HostMetrics { sessionId:string; sampledAt:number;
  cpuPct:number; cores:number; loadAvg:[number,number,number];
  memUsedGb:number; memTotalGb:number; diskUsedGb:number; diskTotalGb:number;
  netRxMbps:number; netTxMbps:number; diskReadMbps:number; diskWriteMbps:number }

// ---- 运行时辅助类型（镜像与 UI 状态） ----
export interface Folder { name:string; color:string }
export interface Config { version:number; preferences:Preferences; folders:Folder[];
  hosts:Host[]; snippets:Snippet[]; tunnels:TunnelRule[] }
export interface FileEntry { name:string; isDir:boolean; size:number|null; modified:number|null; mode:number|null }
export interface ParsedHost { name:string; addr:string; user:string; port:number; keyRef:string;
  group:string; tag:string; note:string; duplicate:boolean }
export interface AppInfo { version:string; configDir:string; os:string; configError?:string|null;
  /** 会话日志目录（<configDir>/logs）：打开/清空日志与隐私提示共用，前端不再自己拼路径 */
  logsDir?:string;
  /** 后端判定的低配设备（≤2 核）：前端据此自动关闭 xterm WebGL 渲染器 */
  lowEnd?:boolean }
/** 右键菜单条目。disabled 的条目只降透明度 + aria-disabled，不挂 data-act */
export interface MenuItem { t:string; i?:string; act:string; arg?:string; danger?:boolean; div?:boolean; k?:string; disabled?:boolean }
export interface MenuState { x:number; y:number; items:MenuItem[] }
export type SideTab = 'hosts'|'snips'|'rules'|'peers';
export interface FormDraft {
  _t:'host'|'snip'|'rule'; id?:string;
  name?:string; addr?:string; user?:string; port?:string|number; group?:string; tag?:string;
  keyRef?:string; known?:boolean; agent?:boolean; note?:string;
  /** 认证方式（表单三选一） */
  auth?:AuthMethod;
  /** 表单内密码明文：只活在 form 对象里，保存/取消时立即丢弃，绝不写进 Host */
  pw?:string;
  /** 状态行的「已保存」派生态，来自 has_password(hostId) */
  saved?:boolean;
  /** 用户在表单里点过「清除」（仅在保存主机时才真正 delete_password） */
  pwCleared?:boolean;
  /** 连接后自动执行的命令与记录会话日志，都在「连接选项」组 */
  initCommand?:string; log?:boolean;
  cmd?:string; local?:string; remote?:string; kind?:'local'|'remote'; host?:string;
  /** 转发规则是否启用：编辑表单必须带回原值，否则保存会把已启用规则静默写成 false */
  on?:boolean;
}
export interface ConfirmState { title:string; text:string; fn:string; arg:string }
/** 输入框弹窗：新建文件夹 / 重命名 / 新增分组 / 分组重命名共用（mode 决定保存动作） */
export interface InputState { title:string; text:string; side:'local'|'remote'; value:string;
  mode:'mkdir'|'rename'|'newgroup'|'grouprename'; from?:string }
/** 连接时的密码弹窗状态。密码明文不进 store：由 PasswordModal 组件本地持有 */
export interface PwState { hostId:string; kind:'term'|'sftp'; retry:boolean;
  /** 为「重连」索要密码时记录目标会话 id——submitPassword 走 reconnect 而不是新建连接 */
  reconnectId?:string }
export interface HostKeyState { sessionId:string; hostId:string; host:string; port:number;
  algo:string; fingerprint:string; changed:boolean }
