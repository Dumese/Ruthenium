// 设置 Sheet：语言与地区 + 外观 / 导航与布局 / 行为 / 关于（字体 pill 五项，pill 按各自 mono 字体渲染）
import { useEffect, useMemo, useState } from 'react';
import { useShallow } from 'zustand/react/shallow';
import { Sheet } from '../overlays/Sheet';
import { Field } from '../ui/Field';
import { Button } from '../ui/Button';
import { Pill } from '../ui/Pill';
import { Switch } from '../ui/Switch';
import { BrandMark } from '../ui/BrandMark';
import { useAppStore } from '../../store/useAppStore';
import { T, useLang, LANGS } from '../../i18n';
import { TERMFONTS } from '../../lib/termfonts';
import { gpuAvailable, isLowEndDevice } from '../../lib/gpu';
import { isDevOrE2E } from '../../lib/buildMode';
import { helloRequirement, type HelloAvailability } from '../../ipc/commands';
import { helloUnavailableText } from '../../lib/helloText';
import { pickShareFolder } from '../../lib/dialog';
import { peerCloseWindow, peerOpenWindow, peerSetVisible, peerStatus, type PeerStatus } from '../../ipc/commands';
import { clipboardLocalSet, peerEncoderDiagnostics } from '../../ipc/commands';
import type { Lang } from '../../types/models';

/**
 * 不要用 `useAppStore()` 订阅整个 store：传输进度（150ms/帧）、
 * 指标采样（2s/帧）、Toast 等任何状态变化都会把整张设置面板重渲染一遍。
 * 这里只挑面板真正读的字段 + 稳定的 action（useShallow 做浅比较）。
 */
function useSettingsState() {
  return useAppStore(useShallow((st) => ({
    lang: st.lang, theme: st.theme, fontSize: st.fontSize, lineHeight: st.lineHeight,
    font: st.font, cursor: st.cursor, showLineNumbers: st.showLineNumbers, nav: st.nav,
    showStatus: st.showStatus, confirmClose: st.confirmClose, showDots: st.showDots,
    autoReconnect: st.autoReconnect, logSessions: st.logSessions, gpuMode: st.gpuMode,
    requireWindowsHello: st.requireWindowsHello,
    lanVisible: st.lanVisible, lanStrictPairing: st.lanStrictPairing, lanDeviceName: st.lanDeviceName, lanPort: st.lanPort, peerShareRoot: st.peerShareRoot,
    setLanDeviceName: st.setLanDeviceName, setLanPort: st.setLanPort, setPeerShareRoot: st.setPeerShareRoot,
    appLogsDir: st.appLogsDir,
    setLang: st.setLang, setTheme: st.setTheme, setFontSize: st.setFontSize, setLineHeight: st.setLineHeight,
    setFont: st.setFont, setCursor: st.setCursor, setToggle: st.setToggle, setNav: st.setNav,
    setGpuMode: st.setGpuMode,
    openLogFolder: st.openLogFolder, requestClearLogs: st.requestClearLogs,
  })));
}

function SetRow({ title, sub, checked, onToggle, disabled, act, arg, setting }: {
  title: string;
  sub?: string;
  checked: boolean;
  onToggle: () => void;
  disabled?: boolean;
  act?: string;
  arg?: string;
  setting?: string;
}) {
  return (
    <div className="setrow" data-setting={setting}>
      <div className="sr">
        <div className="srt">{title}</div>
        {sub && <div className="srs">{sub}</div>}
      </div>
      <Switch on={checked} onToggle={onToggle} label={title} disabled={disabled} act={act} arg={arg} />
    </div>
  );
}

export function SettingsSheet() {
  useLang();
  const s = useSettingsState();
  const dispatch = useAppStore((st) => st.dispatch);
  const showToast = useAppStore((st) => st.showToast);
  const [diagBusy, setDiagBusy] = useState(false);
  const overlay = useAppStore((st) => st.overlay);
  const highlightLang = overlay?.kind === 'sheet' && overlay.id === 'settings' && overlay.arg === 'lang';
  // 编码器诊断：只复制到剪贴板，不落盘、不上报；用户主动点才执行。
  const copyEncoderDiagnostics = async () => {
    if (diagBusy) return;
    setDiagBusy(true);
    try {
      const diag = await peerEncoderDiagnostics();
      await clipboardLocalSet(JSON.stringify(diag, null, 2));
      showToast(T('set.encoderDiagCopied'));
    } catch (e) {
      const detail = (e as { detail?: string })?.detail ?? (e as Error)?.message ?? T('err.unknown');
      showToast(T('set.encoderDiagFail', { 0: detail }));
    } finally {
      setDiagBusy(false);
    }
  };
  // 只用一个开关表达 system/off；WebGL 探测不可用时整行禁用并给原因。
  // 低配设备（≤4GB 内存或 ≤2 核）自动走 DOM 渲染器：WebGL 基线 376MB / 每标签 +28MB，
  // DOM 仅 141MB / +2.7MB，且限速下 DOM 的输入延迟更低。
  const lowEnd = useMemo(isLowEndDevice, []);
  const gpuOk = useMemo(gpuAvailable, []);
  const gpuOn = s.gpuMode !== 'off' && !lowEnd;
  const gpuSub = lowEnd ? T('set.gpuLowEnd') : !gpuOk ? T('set.gpuNA') : gpuOn ? undefined : T('set.gpuOffHint');
  // Windows Hello 可用性：只在设置打开时查一次（WinRT 调用 ~毫秒级）。
  // 不可用时（无硬件 / 未配置 PIN / 策略禁用 / 被占用）开关直接禁用并说明原因——
  // 不允许「开了也验证不了」的配置（需求：使用前必须确认系统支持）。
  const [helloAvail, setHelloAvail] = useState<HelloAvailability | null>(null);
  useEffect(() => {
    void helloRequirement().then((r) => setHelloAvail(r.availability)).catch(() => setHelloAvail(null));
  }, []);
  const helloReady = helloAvail === 'available';
  // 只有「不可用」时才需要副标题（告诉用户具体缺什么、去哪配）；
  // 可用时不加常驻说明——标题本身已说明用途，设置页保持无副标题的版式约束。
  const helloSub = helloAvail !== null && helloAvail !== 'available'
    ? helloUnavailableText(helloAvail)
    : undefined;
  // 局域网对等层状态：设置页打开时轮询（5s），窗口倒计时/监听状态随之刷新
  const [lan, setLan] = useState<PeerStatus | null>(null);
  useEffect(() => {
    let alive = true;
    const load = () => {
      void peerStatus().then((st) => { if (alive) setLan(st); }).catch(() => { /* 旧后端/浏览器 */ });
    };
    load();
    const timer = setInterval(load, 5000);
    return () => { alive = false; clearInterval(timer); };
  }, []);
  const lanFail = (e: unknown) => {
    const detail = (e as { detail?: string })?.detail ?? String(e);
    useAppStore.getState().showToast(T('toast.lanFail', { 0: detail }));
  };
  const toggleLan = async () => {
    const next = !s.lanVisible;
    // 只更新本地状态；持久化交给 peer_set_visible（避免 savePrefs 与
    // peer_set_visible 两次写盘相互覆盖）。
    useAppStore.setState({ lanVisible: next });
    try {
      setLan(await peerSetVisible(next));
    } catch (e) {
      useAppStore.setState({ lanVisible: !next });
      lanFail(e);
    }
  };
  const openLanWindow = async () => {
    try { setLan(await peerOpenWindow()); } catch (e) { lanFail(e); }
  };
  const closeLanWindow = async () => {
    try { setLan(await peerCloseWindow()); } catch (e) { lanFail(e); }
  };
  const browseShare = async () => {
    try {
      const picked = await pickShareFolder(s.peerShareRoot, T('set.lanSharePickTitle'));
      if (picked) s.setPeerShareRoot(picked);
    } catch (e) {
      lanFail(e);
    }
  };
  const lanStatus = (() => {
    if (!lan || !lan.listening) return T('set.lanStatusOff');
    const port = lan.port ?? 0;
    if (lan.windowLeftSec != null) {
      const s = lan.windowLeftSec;
      const mmss = `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;
      return T('set.lanStatusWindow', { 0: port, 1: mmss });
    }
    return T('set.lanStatusOn', { 0: port });
  })();
  return (
    <Sheet
      title={T('set.title')}
      foot={
        <>
          <span className="sp" />
          <Button primary onClick={() => dispatch('closeov')}>{T('c.done')}</Button>
        </>
      }
    >
      <div
        className="sec"
        style={{
          borderTop: 0, paddingTop: 0,
          ...(highlightLang ? { outline: '1.5px solid var(--accent)', outlineOffset: 6, borderRadius: 8, padding: '8px', background: 'var(--accent2)' } : {}),
        }}
      >
        <div className="sectitle">{T('set.secLang')}</div>
        <Field label={T('set.lang')}>
          {/* 语言名按惯例用母语显示（简体中文 / 繁體中文 / 日本語 / English），
              不是"未翻译文案"：data-native 让 i18n 残检跳过这一组。 */}
          <div className="pillrow" data-native="1">
            {LANGS.map((l) => (
              <Pill key={l.id} on={s.lang === l.id} onClick={() => s.setLang(l.id as Lang)}>{l.label}</Pill>
            ))}
          </div>
        </Field>
        <p className="srs">{T('set.langNote')}</p>
      </div>
      <div className="sec">
        <div className="sectitle">{T('set.secAppearance')}</div>
        <Field label={T('set.theme')}>
          <div className="pillrow">
            <Pill on={s.theme === 'light'} onClick={() => s.setTheme('light')}>{T('set.light')}</Pill>
            <Pill on={s.theme === 'dark'} onClick={() => s.setTheme('dark')}>{T('set.dark')}</Pill>
            <Pill on={s.theme === 'system'} onClick={() => s.setTheme('system')}>{T('set.followSystem')}</Pill>
          </div>
        </Field>
        <Field label={T('set.fontSize', { 0: s.fontSize })}>
          <div className="rangerow">
            <input type="range" min={10} max={18} step={0.5} value={s.fontSize}
              onChange={(e) => s.setFontSize(Number(e.target.value))} />
            <span className="mono" data-v="fontsize" style={{ fontSize: 11, color: 'var(--t3)' }}>{s.fontSize}</span>
          </div>
        </Field>
        <Field label={T('set.lineHeight', { 0: s.lineHeight.toFixed(2) })}>
          <div className="rangerow">
            <input type="range" min={1} max={2} step={0.05} value={s.lineHeight}
              onChange={(e) => s.setLineHeight(Number(e.target.value))} />
            <span className="mono" data-v="lineheight" style={{ fontSize: 11, color: 'var(--t3)' }}>{s.lineHeight.toFixed(2)}</span>
          </div>
        </Field>
        <Field label={T('set.font')}>
          <div className="pillrow">
            {TERMFONTS.map((f) => (
              <Pill key={f.id} on={s.font === f.id} onClick={() => s.setFont(f.id)}
                style={{ fontFamily: f.mono + ',monospace' }}>{f.id}</Pill>
            ))}
          </div>
        </Field>
        <Field label={T('set.cursor')}>
          <div className="pillrow">
            <Pill on={s.cursor === 'block'} onClick={() => s.setCursor('block')}>{T('set.cursorBlock')}</Pill>
            <Pill on={s.cursor === 'bar'} onClick={() => s.setCursor('bar')}>{T('set.cursorBar')}</Pill>
          </div>
        </Field>
        <SetRow title={T('set.lineNo')} checked={s.showLineNumbers} onToggle={() => s.setToggle('showLineNumbers')} />
      </div>
      <div className="sec">
        <div className="sectitle">{T('set.secNav')}</div>
        <Field label={T('set.navStyle')}>
          <div className="pillrow">
            <Pill on={s.nav === 'classic'} onClick={() => s.setNav('classic')}>{T('set.navClassic')}</Pill>
            <Pill on={s.nav === 'x'} onClick={() => s.setNav('x')}>{T('set.navX')}</Pill>
          </div>
        </Field>
        <SetRow title={T('set.showStatus')} checked={s.showStatus} onToggle={() => s.setToggle('showStatus')} />
      </div>
      <div className="sec">
        <div className="sectitle">{T('set.secBehavior')}</div>
        <SetRow title={T('set.confirmClose')} checked={s.confirmClose} onToggle={() => s.setToggle('confirmClose')} />
        <SetRow title={T('set.showDot')} checked={s.showDots} onToggle={() => s.setToggle('showDots')} />
        {/* 断线自动重连（默认关）与会话日志全局默认 */}
        <SetRow title={T('set.autoReconnect')} checked={s.autoReconnect} onToggle={() => s.setToggle('autoReconnect')} setting="autoReconnect" />
        <SetRow title={T('set.logSessions')} checked={s.logSessions} onToggle={() => s.setToggle('logSessions')} setting="logSessions" />
        <SetRow
          title={T('set.winHello')}
          sub={helloSub}
          checked={s.requireWindowsHello}
          disabled={!helloReady && !s.requireWindowsHello}
          onToggle={() => s.setToggle('requireWindowsHello')}
          setting="requireWindowsHello"
        />
        <SetRow
          title={T('set.gpu')}
          sub={gpuSub}
          checked={gpuOk && gpuOn}
          disabled={!gpuOk}
          act="settoggle"
          arg="gpu"
          setting="gpu"
          onToggle={() => { if (gpuOk) s.setGpuMode(gpuOn ? 'off' : 'system'); }}
        />
      </div>
      <div className="sec">
        <div className="sectitle">{T('set.secLan')}</div>
        <SetRow
          title={T('set.lanVisible')}
          checked={s.lanVisible}
          onToggle={() => void toggleLan()}
          setting="lanVisible"
        />
        <SetRow
          title={T('set.lanStrictPairing')}
          sub={T('set.lanStrictPairingHint')}
          checked={s.lanStrictPairing}
          onToggle={() => s.setToggle('lanStrictPairing')}
          setting="lanStrictPairing"
        />
        <Field label={T('set.lanDeviceName')}>
          <input
            className="inp"
            value={s.lanDeviceName}
            placeholder={T('set.lanDeviceNamePh')}
            onChange={(e) => s.setLanDeviceName(e.target.value)}
          />
        </Field>
        <Field label={T('set.lanPort')}>
          <input
            className="inp"
            type="number"
            min={0}
            max={65535}
            value={String(s.lanPort)}
            onChange={(e) => s.setLanPort(Number(e.target.value) || 0)}
          />
        </Field>
        <Field label={T('set.lanShareRoot')}>
          <div className="pathpick">
            <input
              className="inp"
              data-lan-share-root
              value={s.peerShareRoot}
              placeholder={T('set.lanShareRootPh')}
              onChange={(e) => s.setPeerShareRoot(e.target.value)}
            />
            <Button icon="folder" onClick={() => void browseShare()}>
              <span data-lan-share-browse>{T('set.lanShareBrowse')}</span>
            </Button>
            {s.peerShareRoot !== '' && (
              <Button icon="x" onClick={() => s.setPeerShareRoot('')}>
                {T('set.lanShareClear')}
              </Button>
            )}
          </div>
        </Field>
        <div className="kv">
          <span>{T('set.lanFingerprint')}</span>
          <span className="mono" title={lan?.fingerprint ?? ''}>{lan?.fingerprint || '—'}</span>
        </div>
        <div className="kv">
          <span>{T('set.lanStatus')}</span>
          <span data-lan-status>{lanStatus}</span>
        </div>
        <div className="kv">
          <span>{T('set.lanPairPort')}</span>
          <span data-lan-pair-port>
            {lan?.pairPort
              ? String(lan.pairPort)
              : lan?.listening ? T('set.lanPairUnavailable') : '—'}
          </span>
        </div>
        <div className="setrow">
          <div className="sr"><div className="srt">{T('set.lanWindow')}</div></div>
          <div className="pillrow">
            <Button icon="link" onClick={() => void openLanWindow()}>{T('set.lanOpenWindow')}</Button>
            <Button icon="x" onClick={() => void closeLanWindow()}>{T('set.lanCloseWindow')}</Button>
          </div>
        </div>
      </div>
      <div className="sec">
        <div className="sectitle">{T('set.secLogs')}</div>
        {/* 会话日志是明文终端内容，必须讲清位置/风险，并给出打开与清空入口。
            目录与隐私说明用 kv 行（设置面板的设计语言是「无副标题」，不要用 .srs）。 */}
        <div className="kv">
          <span>{T('set.logPath')}</span>
          <span className="mono" title={s.appLogsDir}>{s.appLogsDir || '—'}</span>
        </div>
        <div className="kv"><span>{T('set.logPrivacy')}</span><span>{T('set.logPrivacyVal')}</span></div>
        <div className="setrow">
          <div className="sr">
            <div className="srt">{T('set.logActions')}</div>
          </div>
          <div className="pillrow">
            <Button icon="folder" onClick={() => void s.openLogFolder()}>{T('set.logOpen')}</Button>
            <Button icon="trash" onClick={() => s.requestClearLogs()}>{T('set.logClear')}</Button>
            <Button icon="copy" disabled={diagBusy} onClick={() => void copyEncoderDiagnostics()}>
              {T('set.encoderDiag')}
            </Button>
          </div>
        </div>
      </div>
      <div className="sec">
        <div className="sectitle">{T('set.secAbout')}</div>
        <div className="brandrow">
          <BrandMark size={34} plate />
          <div>
            <div className="bt">Ruthenium <span style={{ color: 'var(--t3)', fontWeight: 450, fontSize: 11 }}>{__APP_VERSION__}</span></div>
          </div>
        </div>
        <div className="kv"><span>{T('set.aboutMode')}</span><span>{T('set.aboutModeVal')}</span></div>
        <div className="kv"><span>{T('set.aboutConfig')}</span><span>{T('set.aboutConfigVal')}</span></div>
        <div className="kv"><span>{T('set.aboutCred')}</span><span>{T('set.aboutCredVal')}</span></div>
        {/* 覆盖表只在 dev/e2e 暴露（生产包内不应有演示/QA 入口） */}
        {isDevOrE2E && (
          <div className="kv"><span>{T('set.aboutCov')}</span><span><button className="covlink" onClick={() => dispatch('coverage')}>{T('set.openCov')}</button></span></div>
        )}
      </div>
    </Sheet>
  );
}
