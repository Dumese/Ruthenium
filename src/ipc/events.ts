// listen/emit 订阅封装 + 取消订阅
import { isTauri } from './env';
import { mockListen } from './mockBridge';

type UnlistenFn = () => void;

export async function listen<T>(event: string, handler: (payload: T) => void): Promise<UnlistenFn> {
  if (isTauri) {
    const { listen: tauriListen } = await import('@tauri-apps/api/event');
    return tauriListen<T>(event, (e) => handler(e.payload));
  }
  return mockListen(event, handler as (p: unknown) => void);
}
