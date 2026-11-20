// 全局状态（zustand）——slice 组合入口。
// xterm 实例、ResizeObserver、unlisten 句柄一律不进 store（见 terminal/useXterm.ts）。
//
// 拆分结构（slice pattern，zustand 官方推荐）：
//   types.ts      AppStore 完整接口（对外契约，一字未改）
//   guards.ts     模块级可变状态（连接闸门 / 进度缓冲 / 计时器）与共享纯函数
//   slices/
//     prefsSlice     偏好 / 语言 / 视图设置
//     peerSlice      局域网对等层（发现/配对/互传/控制/屏幕）
//     overlaySlice   覆盖层 / 确认输入弹窗 / 右键菜单动作
//     sessionSlice   连接闸门 / 会话生命周期 / 重连 / 指纹 / 密码（核心）
//     formSlice      表单 / 主机 / 导入 / 备份 / 片段与转发
//     sftpSlice      SFTP 双栏状态与文件操作 / 传输队列
//     actionsSlice   runNamed / dispatch / goto 三张动作路由表
//
// 对外 API 不变：消费方仍 `import { useAppStore } from '.../useAppStore'`；
// AppStore 字段与 action 签名与拆分前完全一致（类型由 types.ts 保证）。
// slice 之间通过 `get()` 跨域调用（如 sftp 需要 session 的 active），类型上
// 仍是同一个 AppStore——这是 zustand slice pattern 的标准跨 slice 协作方式。
import { create } from 'zustand';
import type { AppStore } from './types';
import { createPrefsSlice } from './slices/prefsSlice';
import { createPeerSlice } from './slices/peerSlice';
import { createOverlaySlice } from './slices/overlaySlice';
import { createSessionSlice } from './slices/sessionSlice';
import { createFormSlice } from './slices/formSlice';
import { createSftpSlice } from './slices/sftpSlice';
import { createActionsSlice } from './slices/actionsSlice';
import {
  pwRetries, cancelledConnects, earlyConnStates, quietSessions,
  reconnectingSessions, pendingKinds, pendingProgress, pendingProgressTimers,
  retryTimers, reconnectFailCode, autoReconnecting, retryGen,
} from './slices/guards';

export const useAppStore = create<AppStore>()((...a) => ({
  ...createPrefsSlice(...a),
  ...createPeerSlice(...a),
  ...createOverlaySlice(...a),
  ...createSessionSlice(...a),
  ...createFormSlice(...a),
  ...createSftpSlice(...a),
  ...createActionsSlice(...a),
}));

export type { Preferences, Folder } from './types';
export { normalizePrefs, DEFAULT_PREFS } from './slices/prefsSlice';

/** 测试隔离——重置本模块的全部模块级可变状态。
 *  正式运行不调用；vitest 的 beforeEach 使用（只重置 zustand state 不够，
 *  cancelledConnects / pwRetries 等会跨用例泄漏，产生顺序相关的假绿/假红）。 */
export function __resetSessionGuards(): void {
  pwRetries.clear();
  cancelledConnects.clear();
  earlyConnStates.clear();
  quietSessions.clear();
  reconnectingSessions.clear();
  pendingKinds.clear();
  pendingProgress.clear();
  pendingProgressTimers.forEach((t) => clearTimeout(t));
  pendingProgressTimers.clear();
  retryTimers.forEach((t) => clearInterval(t));
  retryTimers.clear();
  reconnectFailCode.clear();
  autoReconnecting.clear();
  retryGen.clear();
}
