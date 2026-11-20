import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';

// Tauri expects a fixed port; fail if that port is not available
export default defineConfig(({ mode }) => {
  // 发布构建必须排除 mock 数据（演示主机、事件回放）。
  // 生产构建把 `./mock` 换成空替身；开发与 E2E 构建（--mode e2e）保留真实 mock。
  const useMock = mode !== 'production';
  // 版本的唯一权威源是 src-tauri/tauri.conf.json（安装包文件名、Rust get_app_info
  // 的兜底都来自它）。构建期注入 __APP_VERSION__，设置「关于」与 mock 的
  // get_app_info 都读这个常量，界面不得硬编码版本号。
  const appVersion = (JSON.parse(readFileSync(resolve(__dirname, 'src-tauri/tauri.conf.json'), 'utf8')) as { version: string }).version;
  return {
    define: { __APP_VERSION__: JSON.stringify(appVersion) },
    plugins: [react()],
    clearScreen: false,
    resolve: {
      alias: useMock
        ? []
        : [{ find: /^\.\/mock$/, replacement: resolve(__dirname, 'src/ipc/mock.stub.ts') }],
    },
    server: {
      port: 1420,
      strictPort: true,
      // 别监视 Rust 侧：src-tauri/target 是 cargo 的构建产物，`cargo build` 期间文件被锁，
      // chokidar 抛 EBUSY 会直接把 dev server 进程带崩（启动 dev 后另开窗口 `cargo build` 必现）。
      watch: { ignored: ['**/src-tauri/**'] },
    },
    envPrefix: ['VITE_', 'TAURI_'],
    build: {
      target: 'chrome105',
      minify: 'esbuild',
      sourcemap: false,
      rollupOptions: {
        output: {
          // 只切一块长期不变的 vendor（React/ReactDOM/scheduler）：它们的版本跟随依赖升级，
          // 业务代码每次发版都变。分开后桌面端更新时 vendor 命中旧缓存、只需重新传输/解析业务包；
          // 首屏两个 chunk 可并行下载（本地协议下请求开销低，未再细分）。
          // 其余依赖（zustand/@tauri-apps/api，体积小）与业务代码留在主包，避免过度切分。
          manualChunks: {
            'vendor-react': ['react', 'react-dom', 'react-dom/client', 'react/jsx-runtime', 'scheduler'],
          },
        },
      },
    },
  };
});
