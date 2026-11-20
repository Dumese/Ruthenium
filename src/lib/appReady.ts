// 主窗口是否已经显示（main.tsx 在 ui_ready 成功后 resolve）。
//
// HelloGate 用它保证 Windows 的系统验证框弹在**可见**窗口之上：窗口还隐藏时
// 发起 `UserConsentVerifier`，对话框可能落到其它窗口后面，用户会以为「点了没反应」。
// 只用于时序协调，不承载任何业务状态。
let resolveShown: (() => void) | null = null;

export const appShown = new Promise<void>((resolve) => {
  resolveShown = resolve;
});

export function markAppShown(): void {
  resolveShown?.();
  resolveShown = null;
}
