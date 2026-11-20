// invoke<T>() 的类型安全包装 —— 前端调用后端的唯一出口；命令名 snake_case。
// 无 Tauri 环境（纯浏览器 npm run dev）自动回退到 mock.ts。
import { invoke, Channel } from '@tauri-apps/api/core';
import { isTauri } from './env';
import { mockInvoke } from './mockBridge';
import { writeTermData } from '../components/terminal/useXterm';
import type {
  Config, Preferences, Host, Snippet, TunnelRule, SessionSummary, FileEntry, ParsedHost, AppInfo, HostMetrics,
} from '../types/models';
import type { TermDataEvent } from '../types/events';
import type {
  ImportSshConfigArgs, CommitImportArgs, ExportBackupArgs, ImportBackupArgs, PickedPath,
  UpsertHostArgs, DeleteHostArgs, RenameFolderArgs, DeleteFolderArgs,
  ConnectArgs, ConnectLocalArgs, ResizeTerminalArgs, CloseSessionArgs, ReconnectArgs, ConnectTransientArgs, CancelConnectArgs,
  TrustHostKeyArgs, RejectHostKeyArgs,
  LocalListArgs, LocalMkdirArgs, LocalDeleteArgs, LocalRenameArgs,
  SftpListArgs, SftpUploadArgs, SftpDownloadArgs, SftpMkdirArgs, SftpDeleteArgs, SftpRenameArgs, SftpCancelArgs,
  UpsertSnippetArgs, DeleteSnippetArgs, UpsertTunnelArgs, DeleteTunnelArgs, ToggleTunnelArgs,
  StartMetricsArgs, StopMetricsArgs, SampleMetricsArgs, SetSessionEncodingArgs, SnapOverlayArgs,
  ConnectResult, CommitImportResult, IdResult,
} from '../types/ipc';

// eslint-disable-next-line @typescript-eslint/no-explicit-any
async function call<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  if (isTauri) return invoke<T>(cmd, args ?? {});
  return mockInvoke<T>(cmd, args);
}

/**
 * 终端数据 Channel（官方流式 IPC）。建连命令携带 on_data 时，后端 reader 的
 * 输出直达这里（有序、按调用方隔离，无全局事件分发）；数据仍走 term://data 的
 * 同一解码/写队列入口（writeTermData），行为与事件路径完全一致。
 * 事件监听（App.tsx 的 term://data）保留作为 mock/回退路径，两条链路互不重复：
 * 后端只在 on_data 缺席时才 emit 事件。
 */
function termDataChannel(): Channel<TermDataEvent> {
  const ch = new Channel<TermDataEvent>();
  ch.onmessage = (p) => writeTermData(p.sessionId, p.data);
  return ch;
}

/* ---------- 配置 ---------- */
export const loadConfig = () => call<Config>('load_config');
export const savePreferences = (preferences: Preferences) => call<void>('save_preferences', { preferences });
export const importSshConfig = (text: ImportSshConfigArgs['text']) => call<ParsedHost[]>('import_ssh_config', { text });
export const commitImport = (hosts: CommitImportArgs['hosts']) => call<CommitImportResult>('commit_import', { hosts });
export const exportBackup = (saveToken: ExportBackupArgs['saveToken']) => call<void>('export_backup', { saveToken });
export const importBackup = (openToken: ImportBackupArgs['openToken']) => call<Config>('import_backup', { openToken });
/**
 * 原生「另存为」对话框（后端创建、父窗口 = 主窗口 → 一定弹在应用前面）。
 * 返回 `{token, path}`（token 供 export_backup 一次性消费）或 null（用户取消）；
 * 命令失败会 reject（调用方必须提示，不能静默）。
 */
export const pickBackupSavePath = (title: string, fileName: string) =>
  call<PickedPath | null>('pick_backup_save_path', { title, fileName });
/** 原生「打开」对话框（同上）。null = 用户取消。 */
export const pickBackupOpenPath = (title: string) =>
  call<PickedPath | null>('pick_backup_open_path', { title });

/** 原生目录选择（局域网共享根）；null = 用户取消。 */
export const pickFolder = (title: string, defaultPath?: string | null) =>
  call<string | null>('pick_folder', { title, defaultPath: defaultPath ?? null });

/* ---------- 主机 ---------- */
// list_hosts 无前端调用方：主机总表始终由 load_config 全量下发。
// 后端命令保留，这里不重复封装。
export const upsertHost = (host: UpsertHostArgs['host']) => call<Host>('upsert_host', { host });
export const deleteHost = (id: DeleteHostArgs['id']) => call<void>('delete_host', { id });
/** 新增/重命名主机分组；name 已存在时只更新颜色。返回落盘后的分组。 */
export const upsertFolder = (name: string, color?: string) =>
  call<{ name: string; color: string }>('upsert_folder', { name, color });
/** 重命名分组（folders 条目与成员主机的 group 同步更新）。 */
export const renameFolder = (oldName: string, newName: string) =>
  call<void>('rename_folder', { oldName, newName } satisfies RenameFolderArgs);
/** 删除分组；成员主机移入剩余第一个分组，返回被移动的主机数。 */
export const deleteFolder = (name: string) =>
  call<number>('delete_folder', { name } satisfies DeleteFolderArgs);

/* ---------- 秘密（密码只走 keyring；详情页状态一律来自 has_password） ---------- */
export const setPassword = (hostId: string, password: string) => call<void>('set_password', { hostId, password });
export const hasPassword = (hostId: string) => call<boolean>('has_password', { hostId });
export const deletePassword = (hostId: string) => call<void>('delete_password', { hostId });

/* ---------- 会话 ---------- */
/** password 仅 password 认证使用，只传本次调用，不落盘
 *  attemptId：关闭 connecting 标签时交给 cancel_connect 中断在途握手/认证。
 *  onData：Tauri 环境下创建数据 Channel，后端终端输出直达 writeTermData；
 *  mock 环境不传（后端回退 term://data 事件，mock 的事件回放照常工作）。 */
export const connectSession = (hostId: string, cols = 120, rows = 32, kind?: 'term' | 'sftp', password?: string, attemptId?: string) =>
  call<ConnectResult>('connect', {
    hostId, cols, rows, ...(kind ? { kind } : {}), ...(password ? { password } : {}), ...(attemptId ? { attemptId } : {}),
    ...(isTauri ? { onData: termDataChannel() } : {}),
  } satisfies ConnectArgs);
/** 新建本地终端（ConPTY）；返回裸 sessionId，事件复用 term://data / conn://state / term://exit */
export const connectLocal = (cols = 120, rows = 32) =>
  call<ConnectResult>('connect_local', { cols, rows, ...(isTauri ? { onData: termDataChannel() } : {}) } satisfies ConnectLocalArgs);
/**
 * 快速连接：临时主机不写 hosts.toml，但认证 / 指纹流程与普通主机完全相同。
 * 返回裸 sessionId，后续事件与 connect 一致。
 */
export const connectTransient = (host: Host, cols = 120, rows = 32, kind?: 'term' | 'sftp', password?: string, attemptId?: string) =>
  call<ConnectResult>('connect_transient', {
    host, cols, rows, ...(kind ? { kind } : {}), ...(password ? { password } : {}), ...(attemptId ? { attemptId } : {}),
    ...(isTauri ? { onData: termDataChannel() } : {}),
  } satisfies ConnectTransientArgs);
/**
 * Uint8Array → base64（STANDARD alphabet，与后端 `events::b64` 一致）。
 *
 * 为什么要分块：`String.fromCharCode(...arr)` 是把每个字节当成一个实参展开，
 * V8/SpiderMonkey 的实参上限约 64K，超过会抛 `RangeError: Maximum call stack
 * size exceeded` / `too many arguments`。终端粘贴 100 KiB 一次就能触发。
 * 这里每 32 KiB 拼一段再 `btoa`，既避开实参上限，也避免逐字节字符串拼接的 O(n²)。
 */
const B64_CHUNK_SIZE = 0x8000; // 32 KiB：单次 spread 的实参数量安全上限内

export function bytesToB64(data: Uint8Array): string {
  let bin = '';
  for (let i = 0; i < data.length; i += B64_CHUNK_SIZE) {
    bin += String.fromCharCode(...data.subarray(i, i + B64_CHUNK_SIZE));
  }
  return btoa(bin);
}

/**
 * 终端输入走 `write_terminal`：`data` 为 base64 文本。
 * 不能传 `Array.from(data)`：JSON 数字数组会让 100 KiB 粘贴膨胀成 ~400 KB 文本。
 */
export const writeTerminal = (sessionId: string, data: Uint8Array) =>
  call<void>('write_terminal', { sessionId, data: bytesToB64(data) });
export const resizeTerminal = (sessionId: string, cols: number, rows: number) => call<void>('resize_terminal', { sessionId, cols, rows } satisfies ResizeTerminalArgs);
export const closeSession = (sessionId: string) => call<void>('close_session', { sessionId } satisfies CloseSessionArgs);
export const reconnectSession = (sessionId: string, password?: string, attemptId?: string) =>
  call<ConnectResult>('reconnect', {
    sessionId, ...(password ? { password } : {}), ...(attemptId ? { attemptId } : {}),
    ...(isTauri ? { onData: termDataChannel() } : {}),
  } satisfies ReconnectArgs);
/** 中断在途连接（握手/认证）。返回是否真的有在途连接被取消。 */
export const cancelConnect = (attemptId: string) =>
  call<boolean>('cancel_connect', { attemptId } satisfies CancelConnectArgs);
/**
 * 会话列表入口。启动时由 useAppStore.init() 对账一次：
 * webview 重载后后端可能仍有已失去 xterm 实例的遗留会话，前端据此外层关闭，避免泄漏。
 */
export const listSessions = () => call<SessionSummary[]>('list_sessions');

/* ---------- 指纹 ---------- */
export const trustHostKey = (sessionId: string, fingerprint: string) => call<void>('trust_host_key', { sessionId, fingerprint } satisfies TrustHostKeyArgs);
export const rejectHostKey = (sessionId: string) => call<void>('reject_host_key', { sessionId } satisfies RejectHostKeyArgs);

/* ---------- 本地 FS / SFTP ---------- */
export const localList = (path: string) => call<FileEntry[]>('local_list', { path } satisfies LocalListArgs);
export const localMkdir = (path: string, name: string) => call<void>('local_mkdir', { path, name } satisfies LocalMkdirArgs);
export const localDelete = (path: string) => call<void>('local_delete', { path } satisfies LocalDeleteArgs);
// 重命名返回**新的完整路径**（后端契约），调用方可据此选中新名字；
// 不能用 call<void>，否则返回值会被丢掉。
export const localRename = (path: string, newName: string) => call<string>('local_rename', { path, newName } satisfies LocalRenameArgs);
/** 远端默认目录（登录用户 home）：SFTP 首次打开时的起点 */
export const sftpHome = (sessionId: string) => call<string>('sftp_home', { sessionId });
export const sftpList = (sessionId: string, path: string) => call<FileEntry[]>('sftp_list', { sessionId, path } satisfies SftpListArgs);
export const sftpUpload = (sessionId: string, localPath: string, remotePath: string) => call<IdResult>('sftp_upload', { sessionId, localPath, remotePath } satisfies SftpUploadArgs);
export const sftpDownload = (sessionId: string, remotePath: string, localPath: string) => call<IdResult>('sftp_download', { sessionId, remotePath, localPath } satisfies SftpDownloadArgs);
export const sftpMkdir = (sessionId: string, path: string) => call<void>('sftp_mkdir', { sessionId, path } satisfies SftpMkdirArgs);
export const sftpDelete = (sessionId: string, path: string) => call<void>('sftp_delete', { sessionId, path } satisfies SftpDeleteArgs);
export const sftpRename = (sessionId: string, path: string, newName: string) => call<string>('sftp_rename', { sessionId, path, newName } satisfies SftpRenameArgs);
export const sftpCancel = (transferId: string) => call<void>('sftp_cancel', { transferId } satisfies SftpCancelArgs);

/* ---------- 片段 / 转发 ---------- */
// list_snippets / list_tunnels 同样无前端调用方：全量数据来自 load_config。
export const upsertSnippet = (snippet: UpsertSnippetArgs['snippet']) => call<Snippet>('upsert_snippet', { snippet } satisfies UpsertSnippetArgs);
export const deleteSnippet = (id: string) => call<void>('delete_snippet', { id } satisfies DeleteSnippetArgs);
export const upsertTunnel = (tunnel: UpsertTunnelArgs['tunnel']) => call<TunnelRule>('upsert_tunnel', { tunnel } satisfies UpsertTunnelArgs);
export const deleteTunnel = (id: string) => call<void>('delete_tunnel', { id } satisfies DeleteTunnelArgs);
export const toggleTunnel = (id: string, on: boolean) => call<void>('toggle_tunnel', { id, on } satisfies ToggleTunnelArgs);

/* ---------- 指标（ConnInfoSheet） ---------- */
export const startMetrics = (sessionId: string) => call<void>('start_metrics', { sessionId } satisfies StartMetricsArgs);
export const stopMetrics = (sessionId: string) => call<void>('stop_metrics', { sessionId } satisfies StopMetricsArgs);
export const sampleMetrics = (sessionId: string) => call<HostMetrics>('sample_metrics', { sessionId } satisfies SampleMetricsArgs);

/* ---------- 会话编码（EncodingPicker） ---------- */
export const setSessionEncoding = (sessionId: string, encoding: string) =>
  call<string>('set_session_encoding', { sessionId, encoding } satisfies SetSessionEncodingArgs);

/* ---------- 系统 ---------- */
export const getAppInfo = () => call<AppInfo>('get_app_info');

/** 局域网对等层：可见性/监听状态（设置页与设备页共用）。 */
export interface PeerStatus {
  listening: boolean;
  alwaysOn: boolean;
  windowLeftSec: number | null;
  port: number | null;
  /** 配对监听实际端口（null = 配对不可用） */
  pairPort: number | null;
  deviceName: string;
  fingerprint: string;
  shareRoot: string;
  peerCount: number;
}
export const peerStatus = () => call<PeerStatus>('peer_status');
export const peerSetVisible = (visible: boolean) => call<PeerStatus>('peer_set_visible', { visible });
export const peerOpenWindow = (minutes?: number) => call<PeerStatus>('peer_open_window', { minutes });
export const peerCloseWindow = () => call<PeerStatus>('peer_close_window');

/** 已配对设备（peers.toml 的记录，前端只读展示）。 */
export interface PeerRecord {
  id: string; name: string; pubkey: string;
  lastAddr?: string | null; pairedAt: string; lastSeen?: string | null;
  grantFiles: boolean; grantControl: boolean; grantConfig: boolean; grantScreen: boolean; grantClipboard: boolean; grantAudio: boolean;
  peerAllowsFiles: boolean; peerAllowsControl: boolean; peerAllowsConfig: boolean; peerAllowsScreen: boolean; peerAllowsClipboard: boolean; peerAllowsAudio: boolean;
}
export const peerList = () => call<PeerRecord[]>('peer_list');

/** 局域网发现到的设备（未配对）。 */
export interface DiscoveredDevice {
  deviceId: string;
  name: string;
  /** `ip:SSH端口`（连接目标） */
  addr: string;
  /** `ip:配对端口`（配对握手目标，与 addr 不同） */
  pairAddr: string;
  fp: string;
  pairing: boolean;
  paired: boolean;
}
export const peerDiscover = () => call<DiscoveredDevice[]>('peer_discover');

/** 配对事件（`peer://pairing`）。 */
export type PairEvent =
  | { status: 'prompt'; pairingId: string; code: string; peerName: string; peerFp: string; peerAddr: string; direction: 'incoming' | 'outgoing'; numeric: boolean; expiresAtMs: number }
  | { status: 'waiting'; pairingId: string; peerName: string; peerAddr: string; expiresAtMs: number }
  | { status: 'ok'; pairingId: string; peer: PeerRecord }
  | { status: 'rejected'; pairingId: string; reason: string }
  | { status: 'timeout'; pairingId: string }
  | { status: 'error'; pairingId: string; detail: string };

export interface PeerGrants { files: boolean; control: boolean; config: boolean; screen: boolean; clipboard: boolean; audio: boolean }

export const peerPairStart = (target: { deviceId?: string; addr?: string }) =>
  call<string>('peer_pair_start', { deviceId: target.deviceId, addr: target.addr });
export const peerPairConfirm = (pairingId: string, agree: boolean, grants: PeerGrants) =>
  call<boolean>('peer_pair_confirm', {
    pairingId,
    agree,
    grantFiles: grants.files,
    grantControl: grants.control,
    grantConfig: grants.config,
    grantScreen: grants.screen,
    grantClipboard: grants.clipboard,
    grantAudio: grants.audio,
  });
export const peerPairCancel = (pairingId: string) => call<boolean>('peer_pair_cancel', { pairingId });
export const peerRemove = (peerId: string) => call<boolean>('peer_remove', { peerId });
export const peerUpdateGrants = (peerId: string, grants: PeerGrants) =>
  call<PeerRecord | null>('peer_update_grants', {
    peerId,
    grantFiles: grants.files,
    grantControl: grants.control,
    grantConfig: grants.config,
    grantScreen: grants.screen,
    grantClipboard: grants.clipboard,
    grantAudio: grants.audio,
  });

/** 打开对端文件互传会话（返回 SFTP sessionId，前端据此开标签）。 */
export const peerOpenSftp = (peerId: string) => call<string>('peer_open_sftp', { peerId });
/** 推送本机配置到对端收件箱（返回写入字节数）。 */
export const peerSendConfig = (peerId: string) => call<number>('peer_send_config', { peerId });

/** 配置收件箱条目（摘要，不含内容）。 */
export interface InboxItem {
  id: string;
  from: string;
  receivedAt: string;
  bytes: number;
  hosts: number;
  snippets: number;
  tunnels: number;
  folders: number;
  exportedAt?: string | null;
  valid: boolean;
  error?: string | null;
}
/** 收件箱事件 payload（`peer://inbox`）。 */
export interface InboxNotice { item: InboxItem }
export const peerInbox = () => call<InboxItem[]>('peer_inbox');
export const peerImportInbox = (itemId: string) => call<Config>('peer_import_inbox', { itemId });
export const peerDeleteInbox = (itemId: string) => call<boolean>('peer_delete_inbox', { itemId });

/** 控制请求（`peer://control-request`）：对端请求控制本机，需本机确认。 */
export interface ControlRequest {
  consentId: string;
  peerName: string;
  peerFp: string;
  addr: string;
  cols: number;
  rows: number;
  helloRequired: boolean;
}
/** 受控会话（横幅）。 */
export interface ControlledSession {
  sessionId: string;
  peerName: string;
  peerAddr: string;
  cols: number;
  rows: number;
  startedAt: string;
  /** 开始时间（epoch ms）：前端本地计时，不轮询后端 */
  startedMs: number;
  elapsedSec: number;
}
/** 受控会话事件（`peer://session`）。 */
export interface ControlSessionEvent {
  action: 'started' | 'ended';
  sessionId: string;
  peerName: string;
  startedMs?: number | null;
  addr?: string | null;
  cols?: number | null;
  rows?: number | null;
  reason?: string | null;
}
/** 控制/屏幕被拒（`peer://denied`）。 */
export interface PeerDeniedEvent {
  peerName: string;
  reason: 'consent' | 'hello' | string;
  /** 对应的确认请求 id（超时也带，前端据此关闭正确弹窗） */
  consentId?: string | null;
  /** `control` / `screen`（决定提示文案） */
  kind?: 'control' | 'screen' | string;
}

export const peerOpenTerminal = (peerId: string, cols: number, rows: number, attemptId?: string) =>
  call<string>('peer_open_terminal', { peerId, cols, rows, attemptId });
export const peerControlConsent = (consentId: string, allow: boolean) =>
  call<boolean>('peer_control_consent', { consentId, allow });
export const peerControlSessions = () => call<ControlledSession[]>('peer_control_sessions');
export const peerKickSession = (sessionId: string) => call<boolean>('peer_kick_session', { sessionId });
/** 刷新「对方允许本机」的授权快照（按需连接 ruthenium-info）。 */
export const peerRefreshGrants = (peerId: string) =>
  call<PeerRecord | null>('peer_refresh_grants', { peerId });

/* ---------- 远程桌面（只看） ---------- */

export interface MonitorInfo { index: number; width: number; height: number; name: string }
export interface WindowInfo { hwnd: number; title: string }
export interface CursorShapeInfo { width: number; height: number; hotspotX: number; hotspotY: number; rgba: string }

/** 屏幕帧/状态（ipc Channel 推送）。 */
export type ScreenFrame =
  | {
      kind: 'frame';
      sessionId: string;
      width: number;
      height: number;
      data: string;
      bytes: number;
      /** 服务端编码：`jpeg` 或 `h264`（Annex B 访问单元） */
      codec: 'jpeg' | 'h264';
      /** H.264 IDR / JPEG 恒为 true */
      key: boolean;
      /** 源画面是否 HD（H.264 解码端据此声明 BT.709；JPEG 忽略） */
      hd?: boolean;
      /** 服务端时间戳（微秒） */
      tsUs: number;
    }
  | { kind: 'monitors'; sessionId: string; items: MonitorInfo[] }
  | { kind: 'info'; sessionId: string; monitor: number; width: number; height: number }
  | { kind: 'error'; sessionId: string; detail: string }
  | {
      kind: 'auth';
      sessionId: string;
      token: string;
      allowInput: boolean;
      clipboard: boolean;
      audio: boolean;
      /** 对端是否支持「查看并操作」（对端版本较旧时不支持时为 false） */
      inputSupported?: boolean;
      /** 查看端是否请求了操作权限 */
      inputRequested?: boolean;
    }
  | { kind: 'pong'; sessionId: string; t: number }
  | {
      kind: 'cursor';
      sessionId: string;
      visible: boolean;
      nx: number;
      ny: number;
      srcWidth: number;
      srcHeight: number;
      shape?: CursorShapeInfo | null;
    }
  | { kind: 'windows'; sessionId: string; items: WindowInfo[] }
  | {
      kind: 'source';
      sessionId: string;
      source: 'desktop' | 'window';
      title?: string | null;
      reason?: string | null;
    }
  | {
      kind: 'stream';
      sessionId: string;
      codec: 'jpeg' | 'h264';
      fps: number;
      bitrate: number;
      width: number;
      height: number;
      reason?: string | null;
    };

/** 屏幕帧通道（Tauri ipc Channel；mock 下返回 null，不构造 Channel）。 */
export function screenFrameChannel(onFrame: (f: ScreenFrame) => void): Channel<ScreenFrame> | null {
  if (!isTauri) return null;
  const ch = new Channel<ScreenFrame>();
  ch.onmessage = onFrame;
  return ch;
}
export const peerOpenScreen = (
  peerId: string,
  quality: number,
  onFrame: Channel<ScreenFrame> | null,
  inputRequested = false,
) => call<string>('peer_open_screen', { peerId, quality, onFrame, inputRequested });
export const peerScreenCommand = (sessionId: string, cmd: Record<string, unknown>) =>
  call<boolean>('peer_screen_command', { sessionId, cmd });
export const peerCloseScreen = (sessionId: string) => call<boolean>('peer_close_screen', { sessionId });

/** 屏幕查看请求（`peer://screen-request`）。 */
export interface ScreenRequest {
  consentId: string;
  peerName: string;
  peerFp: string;
  addr: string;
  helloRequired: boolean;
  /** 本机确认时可勾选「允许操作」 */
  inputRequested: boolean;
  /** 仅请求操作权限（会话已建立，不是新的查看请求） */
  inputOnly?: boolean;
}
/** 受查看会话（横幅）。 */
export interface ViewedSession {
  sessionId: string;
  peerName: string;
  peerAddr: string;
  quality: number;
  monitor: number;
  startedMs: number;
  elapsedSec: number;
}
/** 受查看会话事件（`peer://screen-session`）。 */
export interface ScreenSessionEvent {
  action: 'started' | 'ended';
  sessionId: string;
  peerName: string;
  reason?: string | null;
}
export const peerScreenConsent = (consentId: string, allow: boolean, allowInput = false) =>
  call<boolean>('peer_screen_consent', { consentId, allow, allowInput });
export const peerScreenSessions = () => call<ViewedSession[]>('peer_screen_sessions');
export const peerScreenKick = (sessionId: string) => call<boolean>('peer_screen_kick', { sessionId });

/* ---------- 声音播放 ---------- */

export interface AudioStart { id: string; rate: number; channels: number; frameMs: number }
/** 音频帧（ipc Channel 推送）：Opus 包 base64。 */
export type AudioFrame =
  | { kind: 'data'; sessionId: string; seq: number; data: string }
  | { kind: 'error'; sessionId: string; detail: string };

/** 音频帧通道（Tauri ipc Channel；mock 下返回 null）。 */
export function audioFrameChannel(onFrame: (f: AudioFrame) => void): Channel<AudioFrame> | null {
  if (!isTauri) return null;
  const ch = new Channel<AudioFrame>();
  ch.onmessage = onFrame;
  return ch;
}
export const peerAudioStart = (
  screenSessionId: string,
  token: string | null,
  onFrame: Channel<AudioFrame> | null,
) => call<AudioStart>('peer_audio_start', { screenSessionId, token, onFrame });
export const peerAudioStop = (id: string) => call<boolean>('peer_audio_stop', { id });

/* ---------- 剪贴板同步 ---------- */

export interface ClipboardStart { id: string; text: string }
export const peerClipboardStart = (screenSessionId: string, token?: string | null) =>
  call<ClipboardStart>('peer_clipboard_start', { screenSessionId, token });
export const peerClipboardGet = (id: string) => call<string>('peer_clipboard_get', { id });
export const peerClipboardSet = (id: string, text: string) =>
  call<boolean>('peer_clipboard_set', { id, text });
export const peerClipboardStop = (id: string) => call<boolean>('peer_clipboard_stop', { id });
/** 本机剪贴板（查看端按钮用，后端直接读写，避免 WebView 权限差异）。 */
export const clipboardLocalGet = () => call<string>('peer_clipboard_local_get');
export const clipboardLocalSet = (text: string) =>
  call<boolean>('peer_clipboard_local_set', { text });

/* ---------- 审计 ---------- */

/** 一条局域网会话记录（只含元数据，不含内容）。 */
export interface AuditEntry {
  at: string;
  kind: 'screen' | 'control' | string;
  direction: 'incoming' | 'outgoing' | string;
  peerName: string;
  peerAddr: string;
  quality?: number | null;
  width?: number | null;
  height?: number | null;
  frames?: number | null;
  bytes?: number | null;
  seconds: number;
  reason?: string | null;
}
export const peerAuditRecent = (limit = 50) => call<AuditEntry[]>('peer_audit_recent', { limit });
export const peerAuditClear = () => call<boolean>('peer_audit_clear');

/* ---------- 远程桌面（操作） ---------- */
export const peerInputStart = (screenSessionId: string, token?: string | null) =>
  call<string>('peer_input_start', { screenSessionId, token });
export const peerInputSend = (inputId: string, event: Record<string, unknown>) =>
  call<boolean>('peer_input_send', { inputId, event });
export const peerInputStop = (inputId: string) => call<boolean>('peer_input_stop', { inputId });
/** 启动门禁查询：配置里是否开启 Windows Hello 验证 + 当前设备的可用性。 */
export type HelloAvailability = 'available' | 'notConfigured' | 'noDevice' | 'policy' | 'busy' | 'unsupported';
export interface HelloRequirement { required: boolean; availability: HelloAvailability }
/** 一次验证的结果（后端 `hello.rs` 的 HelloOutcome，serde camelCase）。 */
export type HelloOutcome =
  | 'verified' | 'canceled' | 'retriesExhausted'
  | 'notConfigured' | 'noDevice' | 'policy' | 'busy' | 'unsupported' | 'error';
export const helloRequirement = () => call<HelloRequirement>('hello_requirement');
export const verifyWindowsHello = (reason?: string) => call<HelloOutcome>('verify_windows_hello', { reason });
/** 清空会话日志（后端只删 *.log 会话文件，保留 app.log 诊断日志），返回删除数量。 */
export const clearSessionLogs = () => call<number>('clear_session_logs');

/* ---------- 启动就绪信号：首帧画好后显示窗口 ---------- */
export const uiReady = () => call<void>('ui_ready');

/* ---------- Windows 11 贴靠布局（src-tauri/src/snap.rs） ---------- */
/**
 * 告诉后端最大化按钮在哪：在按钮上方盖一个回答 `HTMAXBUTTON` 的原生子窗口，
 * Windows 11 才会在悬停时弹出贴靠布局浮层。传 CSS 像素矩形（视口坐标），
 * 宽或高 < 2 表示「现在没有按钮」（专注模式 / 最小化 / 布局未就绪）→ 隐藏覆盖层。
 * 非 Windows 平台是空实现，浏览器（mock）下不会走到这里。
 */
export const snapOverlay = (rect: SnapOverlayArgs) => call<void>('snap_overlay_rect', { ...rect });
