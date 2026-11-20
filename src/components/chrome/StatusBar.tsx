// 状态栏（Windows 版）：
// 本地模式 · 会话级连接状态 · 转发计数 · 会话编码 chip · 连接信息入口。
// 不常驻显示 %APPDATA%\Ruthenium\hosts.toml。
import { Icon } from '../ui/Icon';
import { Dot } from '../ui/Dot';
import { useAppStore } from '../../store/useAppStore';
import { hostById } from '../../store/selectors';
import { T, useLang } from '../../i18n';
import { hhmm } from '../../lib/format';

export function StatusBar() {
  useLang();
  const dispatch = useAppStore((s) => s.dispatch);
  // 必须订阅 sessions/tunnels：直接调用 selector 读 getState() 不会在状态变化时
  // 触发重渲染，切换转发规则后状态栏计数会不刷新。
  const sessions = useAppStore((s) => s.sessions);
  const active = useAppStore((s) => s.active);
  // 计数用「标量 selector」：传输进度每 150ms 一次、转发状态也会变，
  // 直接订阅 transfers / tunnels 数组会让整个状态栏跟着每帧重渲染；
  // 这里只取数字，数组内容变化但计数不变时不重渲染。
  const runningTransfers = useAppStore((s) => {
    let n = 0;
    for (const t of s.transfers) if (t.state === 'running') n += 1;
    return n;
  });
  const on = useAppStore((s) => {
    // 状态栏显示**真的在监听**的转发数（tunnel://state 运行态），
    // 不用配置意图 on——重连/未连接时应为 0，否则会谎报"有转发在跑"。
    let n = 0;
    for (const v of Object.values(s.tunnelState)) if (v.active) n += 1;
    return n;
  });
  const s = sessions.find((x) => x.id === active) ?? null;
  const host = s && s.hostId ? hostById(s.hostId) : null;
  const title = s ? (host?.name ?? s.title.split(' · ')[0]) : '';
  return (
    <footer className="statusbar">
      <span className="st"><Dot state="on" size={6} />{T('app.mode')}</span>
      {s && s.kind !== 'sftp' && (
        <>
          {s.hostId
            ? <span>{title}:{host?.port ?? 22}</span>
            : <span>{T('app.localTerm')}</span>}
          {s.status === 'connecting' && <span>{T('side.connecting')}</span>}
          {s.status === 'disconnected' && <span>{T('side.disconnected')}</span>}
          {s.status === 'connected' && s.hostId && (
            <>
              {/* 只显示有真实来源的信息：不固定渲染未经验证的算法名 /「known_hosts 已校验」/ 延迟
                  （后端 latencyMs 可能为 None），伪造状态会误导用户。 */}
              <span data-opt="1">{T('st.sessionSince', { 0: hhmm(s.startedAt) })}</span>
            </>
          )}
          {s.status === 'connected' && !s.hostId && (
            <span data-opt="1">{T('st.sessionSince', { 0: hhmm(s.startedAt) })}</span>
          )}
        </>
      )}
      {s && s.kind === 'sftp' && (
        <>
          <span>{s.status === 'connecting' ? T('side.connecting') : s.status === 'disconnected' ? T('side.disconnected') : T('st.sftpConnected')}</span>
          <span>{title}</span>
          <span>{T('st.queue', { 0: runningTransfers })}</span>
        </>
      )}
      {!s && <span>{T('st.noSession')}</span>}
      <span className="sp" />
      {on > 0 && <span className="st"><Icon n="network" s={11} /> {T('st.forwards', { 0: on })}</span>}
      {/* 会话日志记录点：日志开启且会话仍在线时在编码 chip 旁显示 ● */}
      {s && s.log && s.status !== 'disconnected' && <span className="recdot" data-log="1" title={T('st.recording')}><Dot state="warn" size={7} /></span>}
      {/* 编码 chip：仅活动会话时显示（SFTP 会话也显示），VSCode 式 */}
      {s && (
        <button className="encbtn" title={T('enc.chipTitle')} onClick={() => dispatch('encpick')}>
          {s.encoding}
          <Icon n="down" s={10} />
        </button>
      )}
      {/* 连接信息入口 */}
      <button title={T('st.connInfoTitle')} onClick={() => dispatch('conninfo')}>
        <Icon n="info" s={11} /> {T('st.connInfo')}
      </button>
    </footer>
  );
}
