// 启动门禁：开启了 Windows Hello 验证时，主界面在验证通过前**不挂载**。
//
// 为什么放在这里而不是 App 内部：App 挂载即触发 store.init()（loadConfig +
// 会话对账）。门禁挡在 App 外面，未验证时配置根本不会被请求，主机列表、
// 标签、终端都不会出现在 DOM 里——比「先加载再盖一层遮罩」泄露面更小。
//
// 结果分支与提示（对应后端 hello::HelloOutcome）：
//   verified            → 解锁，正常启动
//   canceled            → 保持锁定，可重试或退出
//   retriesExhausted    → 保持锁定，可重试或退出
//   error               → 保持锁定（失败即不放行），可重试或退出
//   notConfigured / noDevice / policy / busy / unsupported
//                       → 明确告知环境不可用；主按钮是「退出应用」，
//                         「跳过本次」需要二次确认（不落盘，仅本次生效）
//
// 已知边界（刻意写明，避免误当成完整防护）：这是前端门禁，挡的是「别人坐到
// 这台开着的电脑前直接看主机列表」；有本机代码执行权限的人可以直接读
// %APPDATA%\Ruthenium\hosts.toml（明文），本功能不改变这一点。

import { useCallback, useEffect, useRef, useState, type ReactNode } from 'react';
import { getCurrentWindow } from '@tauri-apps/api/window';
import { helloRequirement, verifyWindowsHello, type HelloAvailability, type HelloOutcome } from '../../ipc/commands';
import { appShown } from '../../lib/appReady';
import { isTauri } from '../../ipc/env';
import { helloUnavailableText } from '../../lib/helloText';
import { T, useLang } from '../../i18n';
import { Button } from '../ui/Button';
import { BrandMark } from '../ui/BrandMark';

type Phase = 'checking' | 'verifying' | 'locked' | 'unlocked';
/** 锁定原因：决定文案与可用的操作。 */
type Trouble = Exclude<HelloOutcome, 'verified'> | null;

/** 环境级不可用（重试没有意义，只能退出或显式跳过本次）。 */
function isEnvironmentTrouble(t: Trouble): boolean {
  return t === 'notConfigured' || t === 'noDevice' || t === 'policy' || t === 'busy' || t === 'unsupported';
}

/** StrictMode 双挂载 / 快速重载时只发起一轮自动验证（系统验证框不能叠两个）。 */
let autoGateStarted = false;

/** 等窗口真正可见再弹系统验证框；ui_ready 异常时最多等 2.5s 后照常发起。 */
function waitForShown(): Promise<void> {
  return Promise.race([appShown, new Promise<void>((r) => setTimeout(r, 2500))]);
}

function troubleText(t: Trouble): string {
  switch (t) {
    case 'canceled': return T('hello.canceled');
    case 'retriesExhausted': return T('hello.retries');
    case 'error': return T('hello.error');
    case 'notConfigured':
    case 'noDevice':
    case 'policy':
    case 'busy':
    case 'unsupported':
      return T('hello.unsupported');
    default: return T('hello.desc');
  }
}

export function HelloGate({ children }: { children: ReactNode }) {
  useLang();
  const [phase, setPhase] = useState<Phase>(() => (autoGateStarted ? 'unlocked' : 'checking'));
  const [trouble, setTrouble] = useState<Trouble>(null);
  const [availability, setAvailability] = useState<HelloAvailability>('available');
  const [confirmSkip, setConfirmSkip] = useState(false);
  const busy = useRef(false);

  /** 发起一次验证并把结果映射到界面状态。 */
  const verifyNow = useCallback(async () => {
    if (busy.current) return;
    busy.current = true;
    setPhase('verifying');
    let outcome: HelloOutcome = 'error';
    try {
      outcome = await verifyWindowsHello(T('hello.dialogReason'));
    } catch {
      // 命令本身失败（后端异常）也按 error 处理：失败即不放行
      outcome = 'error';
    }
    busy.current = false;
    if (outcome === 'verified') {
      setTrouble(null);
      setPhase('unlocked');
      return;
    }
    setTrouble(outcome);
    setPhase('locked');
  }, []);

  // 启动检查：读配置里的开关（只读、无副作用）。不需要验证时直接放行——
  // 浏览器 / mock 环境 hello_requirement 返回 required:false，走同一条路径。
  useEffect(() => {
    if (autoGateStarted) return;
    autoGateStarted = true;
    void (async () => {
      try {
        const req = await helloRequirement();
        setAvailability(req.availability);
        if (!req.required) {
          setPhase('unlocked');
          return;
        }
        await waitForShown();
        await verifyNow();
      } catch {
        // 门禁查询失败（例如旧后端）时按「未开启」处理：不能把应用锁死在自己的门禁里
        setPhase('unlocked');
      }
    })();
  }, [verifyNow]);

  const quit = useCallback(() => {
    void (async () => {
      try {
        if (isTauri) await getCurrentWindow().close();
        else window.close();
      } catch {
        window.close();
      }
    })();
  }, []);

  if (phase === 'unlocked') return <>{children}</>;

  const checking = phase === 'checking';
  const environment = isEnvironmentTrouble(trouble);
  const retryable = trouble === null || trouble === 'canceled' || trouble === 'retriesExhausted' || trouble === 'error';

  return (
    <div className="hellolock" role="dialog" aria-modal="true" aria-label={T('hello.title')}>
      <div className="hlbox">
        <div className="hlbrand">
          <BrandMark size={38} plate />
          <span>Ruthenium</span>
        </div>
        <h2 className="hltitle">{T('hello.title')}</h2>
        <p className="hldesc">{checking || phase === 'verifying' ? T('hello.verifying') : troubleText(trouble)}</p>
        {environment && <p className="hlsub">{helloUnavailableText(availability)}</p>}
        {trouble === 'error' && <p className="hlsub">{T('hello.errorDetail')}</p>}
        {availability !== 'available' && trouble === null && <p className="hlsub">{helloUnavailableText(availability)}</p>}

        {/* 检查阶段（读配置 + 等窗口可见）不显示按钮：避免「重新验证」闪一下 */}
        {!checking && (
          <div className="hlacts">
            {retryable && (
              <Button primary icon="shield" onClick={() => void verifyNow()} disabled={phase === 'verifying'}>
                {phase === 'verifying' ? T('hello.verifying') : T('hello.retry')}
              </Button>
            )}
            {environment && (
              <>
                <Button primary icon="x" onClick={quit}>{T('hello.quit')}</Button>
                {!confirmSkip && (
                  <Button onClick={() => setConfirmSkip(true)}>{T('hello.skipOnce')}</Button>
                )}
              </>
            )}
            {!environment && <Button icon="x" onClick={quit}>{T('hello.quit')}</Button>}
          </div>
        )}

        {environment && confirmSkip && (
          <div className="hlskip">
            <p className="hlwarn">{T('hello.skipWarn')}</p>
            <div className="hlacts">
              <Button onClick={() => { setTrouble(null); setPhase('unlocked'); }}>
                {T('hello.continueUnverified')}
              </Button>
              <Button onClick={() => setConfirmSkip(false)}>{T('hello.cancelSkip')}</Button>
            </div>
          </div>
        )}
      </div>
    </div>
  );
}
