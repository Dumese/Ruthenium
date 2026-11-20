// 构建期由 vite.config.ts 注入：读 src-tauri/tauri.conf.json 的 version 字段。
// 设置「关于」与 mock get_app_info 的版本号都读它——不要在这里或组件里手写版本字符串。
declare const __APP_VERSION__: string;
