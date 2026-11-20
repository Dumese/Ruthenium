// 本机确认弹窗的 30 秒倒计时（与后端 CONSENT_TIMEOUT 对齐）。
//
// 为什么需要：应用最小化时只有应用内模态，用户很容易错过；倒计时让“还有多久
// 自动拒绝”可见，到点主动收尾，避免弹窗一直挂着、队列里的下一个请求被堵住。
// 后端超时也会发 `peer://denied`（带 consentId），两条路径都安全。
import { useEffect, useRef, useState } from 'react';
import { getCurrentWindow } from '@tauri-apps/api/window';
import { isTauri } from '../../ipc/env';

export function useConsentCountdown(
  consentId: string | undefined,
  seconds: number,
  onExpire: () => void,
): number {
  const [left, setLeft] = useState(seconds);
  const expireRef = useRef(onExpire);
  expireRef.current = onExpire;

  useEffect(() => {
    if (!consentId) return;
    setLeft(seconds);
    const startedAt = Date.now();
    const timer = setInterval(() => {
      const remain = Math.max(0, seconds - Math.floor((Date.now() - startedAt) / 1000));
      setLeft(remain);
      if (remain <= 0) {
        clearInterval(timer);
        expireRef.current();
      }
    }, 250);
    return () => clearInterval(timer);
  }, [consentId, seconds]);

  // 弹窗出现时窗口不在前台：闪任务栏（Windows）/ 弹跳 Dock（macOS）。
  useEffect(() => {
    if (!consentId || !isTauri) return;
    if (document.hasFocus()) return;
    // 1 = critical；失败（未授权/浏览器环境）静默忽略
    void getCurrentWindow().requestUserAttention(1).catch(() => { /* ignore */ });
  }, [consentId]);

  return left;
}
