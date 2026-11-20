// mock 的唯一入口。
//
// 为什么要多这一层：`useAppStore` 需要 `IMPORT_SAMPLE` / `resetMock` 等，
// 若各处直接 `import ... from './mock'`，生产构建就无法用一个别名把 mock 整体换掉。
// 统一从这里转发后，`vite.config.ts` 只需把 `./mock` 指向 `mock.stub.ts`（生产构建），
// 演示主机、事件回放与样例配置都不会进入正式包。
import { mockEmit as emitMockEvent } from './mock';
import { useMock } from './env';

export {
  mockInvoke,
  mockEmit,
  mockListen,
  mockDisconnectSession,
  IMPORT_SAMPLE,
} from './mock';
// 仅 mock / E2E：暴露事件注入钩子（模拟后端事件，例如「对端请求控制本机」）。
// 正式包走真实 Tauri 事件，不存在该全局变量（不扩大攻击面）。
if (typeof window !== 'undefined' && useMock) {
  (window as unknown as { __mockEmit?: typeof emitMockEvent }).__mockEmit = emitMockEvent;
}