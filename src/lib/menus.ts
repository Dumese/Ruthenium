// 六类区域的右键菜单条目。
// 只有一份定义、一个渲染组件（overlays/ContextMenu.tsx）；区域里只挂 data-ctx 标记。
// 规则：不放与上下文无关的条目（本地主机不出现「打开 SFTP」）、不放永远不可用的条目；
// 禁用项只降透明度（disabled: true），渲染时不挂 data-act。
// 本模块是纯函数（状态由调用方传入），因此 store 也能反过来用它拼覆盖表的示例菜单。
import { T } from '../i18n';
import type { Host, MenuItem, Session } from '../types/models';

const div = (): MenuItem => ({ t: '', act: '', div: true });

/** 主机行 / 主机库表格行 */
export function hostMenu(host: Host): MenuItem[] {
  const isLocal = host.port === 0;
  return [
    { t: T('menu.connect'), i: 'play', act: 'connect', arg: host.id },
    ...(isLocal ? [] : [{ t: T('menu.openSftp'), i: 'folder', act: 'opensftp', arg: host.id } as MenuItem]),
    div(),
    { t: T('menu.detail'), i: 'info', act: 'hostdetail', arg: host.id },
    { t: T('menu.edit'), i: 'edit', act: 'hostedit', arg: host.id },
    { t: T('menu.copySsh'), i: 'copy', act: 'copyssh', arg: host.id },
    div(),
    { t: T('menu.delHost'), i: 'trash', act: 'hostdel', arg: host.id, danger: true },
  ];
}

/** 会话标签。transient=true 表示快速连接的临时会话：多一个「保存为主机」 */
export function tabMenu(sess: Session | undefined, sessions: Session[], transient = false): MenuItem[] {
  const id = sess?.id ?? '';
  const idx = sessions.findIndex((x) => x.id === id);
  // 只有普通 SSH 终端/SFTP 才能打开 host 的 SFTP；peer/screen 的入口在设备页。
  const hasRemote = !!sess && (sess.kind === 'term' || sess.kind === 'sftp') && !!sess.hostId;
  return [
    {
      t: T('menu.reconnect'), i: 'refresh', act: 'reconnect', arg: id,
      // 仅断线可点：连接中 / 已连接时重新连接没有意义
      disabled: !sess || sess.status !== 'disconnected' || sess.kind === 'screen',
    },
    ...(hasRemote ? [{ t: T('menu.openSftp'), i: 'folder', act: 'opensftp', arg: sess!.hostId } as MenuItem] : []),
    ...(transient ? [{ t: T('menu.saveHost'), i: 'box', act: 'savequickhost', arg: id } as MenuItem] : []),
    div(),
    { t: T('menu.closeTab'), i: 'x', act: 'tabclose', arg: id, k: 'Ctrl+Shift+W' },
    { t: T('menu.closeOthers'), i: 'layers', act: 'closeothers', arg: id, disabled: sessions.length < 2 },
    { t: T('menu.closeRight'), i: 'arrow', act: 'closeright', arg: id, disabled: idx < 0 || idx === sessions.length - 1 },
    div(),
    { t: T('menu.newConn'), i: 'terminal', act: 'newtab', k: 'Ctrl+T' },
    { t: T('menu.closeAll'), i: 'trash', act: 'closeall', danger: true },
  ];
}

/** SFTP 文件行：文件给上传/下载，目录给「打开」；删除作用于右键的那一侧 */
export function fileMenu(side: 'local' | 'remote', name: string, isDir: boolean): MenuItem[] {
  const arg = side + '|' + name;
  const first: MenuItem = isDir
    ? { t: T('menu.open'), i: 'folder', act: 'fopendir', arg }
    : side === 'local'
      ? { t: T('menu.uploadTo'), i: 'upload', act: 'upload', arg }
      : { t: T('menu.downloadTo'), i: 'download', act: 'download', arg };
  return [
    first,
    div(),
    { t: T('menu.rename'), i: 'edit', act: 'frename', arg },
    { t: T('menu.copyPath'), i: 'copy', act: 'copypath', arg },
    div(),
    { t: T('menu.delete'), i: 'trash', act: 'fdelete', arg, danger: true },
  ];
}

/** SFTP 面板空白：刷新 / 新建文件夹 / 按侧的上传或下载选中项 */
export function paneMenu(side: 'local' | 'remote', selLocal: string | null, selRemote: string | null): MenuItem[] {
  return [
    { t: T('menu.refresh'), i: 'refresh', act: 'frefresh', arg: side },
    { t: T('menu.newFolder'), i: 'plus', act: 'fnew', arg: side },
    div(),
    side === 'remote'
      ? { t: T('menu.uploadHere'), i: 'upload', act: 'upload', disabled: !selLocal }
      : { t: T('menu.downloadHere'), i: 'download', act: 'download', disabled: !selRemote },
  ];
}

/** 终端（xterm 区域） */
export function termMenu(sessionId: string | null, hasSelection: boolean): MenuItem[] {
  const sid = sessionId ?? '';
  return [
    // 无选中文字时「复制」禁用（不挂 data-act）
    { t: T('menu.copy'), i: 'copy', act: 'copyterm', disabled: !sid || !hasSelection },
    { t: T('menu.selectAll'), i: 'layers', act: 'termselall' },
    { t: T('menu.paste'), i: 'download', act: 'termpaste' },
    { t: T('menu.clearScreen'), i: 'trash', act: 'termclear', k: 'Ctrl+L' },
    div(),
    { t: T('menu.newConn'), i: 'terminal', act: 'newtab', k: 'Ctrl+T' },
    { t: T('menu.connInfo'), i: 'info', act: 'conninfo' },
    // 日志目录入口（与当前会话是否开启日志无关，目录是全局的）
    { t: T('menu.openLogDir'), i: 'folder', act: 'openlogdir' },
    div(),
    { t: T('menu.closeTab'), i: 'x', act: 'tabclose', arg: sid, k: 'Ctrl+Shift+W', disabled: !sid },
  ];
}

/** 侧栏空白。hostsTab=true 时（主机页）多一项「新增分组」 */
export function sideMenu(opts?: { hostsTab?: boolean }): MenuItem[] {
  return [
    { t: T('menu.newHost'), i: 'plus', act: 'hostform' },
    ...(opts?.hostsTab ? [{ t: T('menu.newGroup'), i: 'layers', act: 'newgroup' } as MenuItem] : []),
    { t: T('menu.importConfig'), i: 'download', act: 'import' },
    div(),
    { t: T('menu.newSnippet'), i: 'code', act: 'snipnew' },
    { t: T('menu.newRule'), i: 'network', act: 'rulenew' },
    div(),
    { t: T('menu.backup'), i: 'layers', act: 'backup' },
  ];
}

/** 分组行右键：重命名（组内主机一并改名）/ 删除（主机移入剩余第一个分组，不删主机） */
export function groupMenu(name: string): MenuItem[] {
  return [
    { t: T('menu.groupRename'), i: 'edit', act: 'grouprename', arg: name },
    div(),
    { t: T('menu.groupDelete'), i: 'trash', act: 'groupdel', arg: name, danger: true },
  ];
}

export { formatSidePath } from './paths';
