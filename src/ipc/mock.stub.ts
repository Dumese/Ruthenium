// 生产构建替身：发布构建必须排除 mock 数据。
// vite.config.ts 在 mode==='production' 时把 `./mock` 别名指到这里，
// 于是演示主机、事件回放、样例配置与 E2E 辅助数据都不会进入正式包。
// 仅在开发（vite dev）与 E2E 构建（--mode e2e）时才会用到真实的 mock.ts。

export function mockInvoke(): Promise<never> {
  return Promise.reject(new Error('mock 未包含在正式构建中'));
}

export function mockEmit(): void {
  /* no-op */
}

export function mockListen(): () => void {
  return () => {
    /* no-op */
  };
}

export function mockDisconnectSession(): void {
  /* no-op */
}

/** 生产环境不会用到样例导入文本；给调用方一个安全空串。 */
export const IMPORT_SAMPLE = '';
