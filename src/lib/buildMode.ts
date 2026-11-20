// 构建模式判定（演示 / QA 入口的可见性）。
//
// 覆盖表、伪造指纹演示等入口只在 dev（vite dev）与 e2e（vite build --mode e2e）构建保留，
// 生产构建必须剔除（发布包不得包含演示数据/演示入口）。
//
// 注意：`vite build` 默认把 NODE_ENV 设为 production，因此即使 `--mode e2e`，
// `import.meta.env.DEV` 也是 **false** —— 必须同时判断 MODE，否则 E2E/QA 构建里
// 覆盖表也会被剔除，依赖它的断言随之失败。
export const isDevOrE2E: boolean =
  import.meta.env.DEV || import.meta.env.MODE === 'e2e';
