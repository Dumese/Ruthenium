// store 拆分（slice pattern）的共享基础设施。
//
// 设计约束：
// - 对外 API 不变：50 个消费方仍 `import { useAppStore } from '.../useAppStore'`，
//   `AppStore` 接口（字段与 action 签名）也保持原样——拆分只动实现的组织方式。
// - zustand 的 slice pattern：每个 slice 是 `StateCreator<AppStore, [], [], SliceShape>`，
//   通过 `get()` 跨域访问其它 slice（如 sftp slice 需要 session slice 的 active），
//   类型上仍是同一个完整 AppStore，不引入循环依赖。
// - 模块级可变状态（pwRetries / pendingProgress 等会话闸门与缓冲）集中放在 `guards.ts`：
//   它们是跨 slice 的隐式契约（connect / closeSession / onConnState 都要读写同一份），
//   分散到各 slice 会造成「看起来独立、实际共享」的陷阱；集中 + __resetSessionGuards
//   统一清理（vitest 隔离）与原实现语义一致。
import type { StateCreator } from 'zustand';
import type { AppStore } from '../types';

export type StoreSet = (partial: Partial<AppStore> | ((s: AppStore) => Partial<AppStore>), replace?: false) => void;
export type StoreGet = () => AppStore;
/** slice creator 的通用形态：set/get 已由 create 装配，slice 只声明自己的实现 */
export type SliceCreator<T> = StateCreator<AppStore, [], [], T>;
