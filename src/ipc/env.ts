// Tauri 运行环境检测：无 Rust 后端时全部 IPC 调用回退到 mock
export const isTauri: boolean = typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window;
export const useMock: boolean = !isTauri || import.meta.env.VITE_USE_MOCK === '1';
