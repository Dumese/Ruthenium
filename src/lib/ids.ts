// sessionId / transferId / 临时对象 id 生成
export function uid(): string {
  if (typeof crypto !== 'undefined' && crypto.randomUUID) return crypto.randomUUID();
  return Math.random().toString(36).slice(2, 9) + Date.now().toString(36);
}
