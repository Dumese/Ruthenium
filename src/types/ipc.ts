// 每个 command 的入参/返回类型
import type { Host, Snippet, TunnelRule } from './models';

export interface ImportSshConfigArgs { text: string }
export interface CommitImportArgs { hosts: Host[] }
/** 导出/导入只认对话框签发的一次性 token（10 分钟过期），不接受任意路径 */
export interface ExportBackupArgs { saveToken: string }
export interface ImportBackupArgs { openToken: string }
/** 原生对话框选中的备份路径：`token` 供 export/import 命令消费，`path` 仅用于展示 */
export interface PickedPath { token: string; path: string }
export interface UpsertHostArgs { host: Host }
export interface DeleteHostArgs { id: string }
export interface SetPasswordArgs { hostId: string; password: string }
export interface HasPasswordArgs { hostId: string }
export interface DeletePasswordArgs { hostId: string }
/** 密码只在 host.auth==='password' 时使用：只活于本次调用栈，不落盘、不写日志 */
/** attemptId：前端"正在连接的标签 id"，关闭该标签时用 cancel_connect 中断握手。 */
/** onData：tauri-apps/api/core 的 Channel<TermDataEvent>；Tauri 环境由 commands.ts
 *  自动注入，后端终端数据走官方流式 IPC（有序、按调用方隔离），不再占用全局事件总线。 */
export interface ConnectArgs { hostId: string; cols: number; rows: number; kind?: 'term' | 'sftp'; password?: string; attemptId?: string; onData?: unknown }
/** 快速连接：host 是临时 Host 对象，后端按同一套认证/指纹流程建会话但不写 config */
export interface ConnectTransientArgs { host: Host; cols: number; rows: number; kind?: 'term' | 'sftp'; password?: string; attemptId?: string; onData?: unknown }
/** 本地终端（ConPTY）：无 hostId，cols/rows 为初始 PTY 尺寸 */
export interface ConnectLocalArgs { cols: number; rows: number; onData?: unknown }
/** data 是 base64（STANDARD），而不是 number[]/Uint8Array：终端字节走 JSON 数字数组
 *  时 64 KiB 会膨胀成 ~230 KB 文本，是终端输出卡顿的主因（见 scripts/perf-real-ui.mjs）。 */
export interface WriteTerminalArgs { sessionId: string; data: string }
export interface ResizeTerminalArgs { sessionId: string; cols: number; rows: number }
export interface CloseSessionArgs { sessionId: string }
export interface ReconnectArgs { sessionId: string; password?: string; attemptId?: string; onData?: unknown }
/** 取消在途连接：attemptId 与 connect/connect_transient/reconnect 传入的一致 */
export interface CancelConnectArgs { attemptId: string }
export interface TrustHostKeyArgs { sessionId: string; fingerprint: string }
export interface RejectHostKeyArgs { sessionId: string }
export interface LocalListArgs { path: string }
export interface LocalMkdirArgs { path: string; name: string }
export interface LocalDeleteArgs { path: string }
export interface LocalRenameArgs { path: string; newName: string }
export interface SftpListArgs { sessionId: string; path: string }
export interface SftpRenameArgs { sessionId: string; path: string; newName: string }
export interface SftpUploadArgs { sessionId: string; localPath: string; remotePath: string }
export interface SftpDownloadArgs { sessionId: string; remotePath: string; localPath: string }
export interface SftpMkdirArgs { sessionId: string; path: string }
export interface SftpDeleteArgs { sessionId: string; path: string }
/** 新增/重命名主机分组（folders，name 为唯一键） */
export interface UpsertFolderArgs { name: string; color?: string }
/** 重命名分组：成员主机的 group 同步更新 */
export interface RenameFolderArgs { oldName: string; newName: string }
/** 删除分组：成员主机移入剩余第一个分组（主机不删除），返回移动数 */
export interface DeleteFolderArgs { name: string }
export interface SftpCancelArgs { transferId: string }
export interface UpsertSnippetArgs { snippet: Snippet }
export interface DeleteSnippetArgs { id: string }
export interface UpsertTunnelArgs { tunnel: TunnelRule }
export interface DeleteTunnelArgs { id: string }
export interface ToggleTunnelArgs { id: string; on: boolean }
export interface StartMetricsArgs { sessionId: string }
export interface StopMetricsArgs { sessionId: string }
export interface SampleMetricsArgs { sessionId: string }
export interface SetSessionEncodingArgs { sessionId: string; encoding: string }
/** 标题栏最大化按钮的原生覆盖层矩形（**CSS 像素**，视口坐标；宽高 < 2 表示隐藏）。
 *  Windows 11 贴靠布局用，见 src-tauri/src/snap.rs。 */
export interface SnapOverlayArgs { x: number; y: number; width: number; height: number }

// connect / reconnect / sftp_upload / sftp_download 均返回**裸字符串**
// （sessionId / transferId，Rust 侧类型是 AppResult<String>）。
// 不能定义成 { sessionId } / { id } 对象：与后端契约不符时会解构出 undefined。
export type ConnectResult = string;
// commit_import 后端直接返回 usize（实际新增数）
export type CommitImportResult = number;
export type IdResult = string;
