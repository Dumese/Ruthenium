// 单侧文件面板：面包屑 + 文件列表。单击选中、双击进目录。
// 文件行的拖拽用 **Pointer 自绘**（SftpView.onRowPointerDown 启动，window 接管 move/up）：
// Tauri 的 dragDropEnabled 会屏蔽 WebView2 内的 HTML5 DnD（dragstart 永不触发），
// 因此行上不再挂 draggable / DragEvent——打包端与浏览器端行为由此保持一致。
import { useEffect, useState } from 'react';
import type { PointerEvent as ReactPointerEvent } from 'react';
import { Icon } from '../ui/Icon';
import { Breadcrumb } from '../ui/Breadcrumb';
import { IconButton } from '../ui/IconButton';
import { useAppStore } from '../../store/useAppStore';
import { selectSftpSessionId } from '../../store/selectors';
import { localList, sftpList } from '../../ipc/commands';
import { fmtSize, fmtMode } from '../../lib/format';
import { T, useLang } from '../../i18n';
import type { FileEntry } from '../../types/models';

type Side = 'local' | 'remote';

function childPath(side: Side, base: string, name: string): string {
  if (side === 'local') {
    if (!base) return name.endsWith('\\') ? name : name + '\\';
    return base.endsWith('\\') ? base + name : base + '\\' + name;
  }
  return (base === '/' ? '' : base) + '/' + name;
}

export function FilePane({ side, title, path, selected, rev, dragging, dropActive, dirOver,
  onRowPointerDown, clickSuppressed }: {
  side: Side; title: string; path: string; selected: string | null; rev: number;
  dragging?: string | null; dropActive?: boolean; dirOver?: string | null;
  /** 文件行按下即进入指针拖拽跟踪（SftpView 负责 move/up/落点判定） */
  onRowPointerDown?: (side: Side, e: ReactPointerEvent, name: string, isDir: boolean) => void;
  /** 拖拽刚结束时吞掉尾随 click（否则松手会被当成“单击选中”） */
  clickSuppressed?: () => boolean;
}) {
  useLang();
  const [entries, setEntries] = useState<FileEntry[]>([]);
  const [error, setError] = useState<string | null>(null);
  const selectFile = useAppStore((s) => s.selectFile);
  const openDir = useAppStore((s) => s.openDir);
  const setSftpPath = useAppStore((s) => s.setSftpPath);
  const upDir = useAppStore((s) => s.upDir);
  const refreshSftp = useAppStore((s) => s.refreshSftp);
  const dispatch = useAppStore((s) => s.dispatch);
  const sessions = useAppStore((s) => s.sessions);
  const active = useAppStore((s) => s.active);
  // SFTP 会话解析统一走 selectSftpSessionId（active 优先 → 第一个 sftp → 第一个 term），
  // 与列表加载 / 写操作 / 标题三处保持同一规则，避免多主机并存时指错连接。
  const sftpSessionId = selectSftpSessionId({ sessions, active }) ?? '';
  const sep = side === 'local' ? '\\' : '/';
  // 本地路径按 \ 分段；根（''）显示「此电脑」，盘符为第一段
  const parts = path.split(/[\\/]/).filter(Boolean);

  useEffect(() => {
    let dead = false;
    setError(null);
    const load = async () => {
      // 远端路径尚未确定（首次进入、等待 home/cwd 解析）时不要请求，否则会读一个空路径
      if (side === 'remote' && !path) { setEntries([]); return; }
      try {
        const list = side === 'local'
          ? await localList(path)
          : await sftpList(sftpSessionId, path);
        if (!dead) setEntries(list);
      } catch (e) {
        if (!dead) { setEntries([]); setError((e as { detail?: string })?.detail ?? T('err.readFail')); }
      }
    };
    void load();
    return () => { dead = true; };
    // 依赖只保留真正影响请求的参数：把整个 sessions 数组放进依赖，
    // 任何会话的状态变化（连接中/断开/重连）都会触发一次目录重取。
    // sftpSessionId 已经表达了「用哪条连接」，换会话时它自己会变。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [side, path, rev, sftpSessionId]);

  return (
    <div
      className={'pane' + (dropActive ? ' dragover' : '')}
      data-side={side}
      data-ctx="pane"
    >
      <div className="panehead">
        <Icon n={side === 'local' ? 'monitor' : 'network'} s={13} />
        <span style={{ fontSize: 11, fontWeight: 550, color: 'var(--text)', minWidth: 0, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>{title}</span>
        {/* 远端绝对路径的根分隔由 Breadcrumb 自己渲染，这里只给本地盘符路径补分隔符 */}
        {side === 'local' && <span style={{ color: 'var(--t4)' }}>{sep}</span>}
        <Breadcrumb
          rootLabel={side === 'local' && !parts.length ? T('sftp.thisPc') : undefined}
          /* 本地栏根是 ''（盘符列表「此电脑」），远端栏根是 '/'。 */
          rootPath={side === 'local' ? '' : '/'}
          parts={parts}
          sep={sep}
          onGo={(p) => setSftpPath(side, p)}
        />
        <IconButton icon="arrow" title={T('sftp.up')} size={13}
          onClick={() => upDir(side)} className="rot180" />
        <IconButton icon="refresh" title={T('sftp.refresh')} size={13} onClick={() => refreshSftp(true)} />
        <IconButton icon="plus" title={T('sftp.newFolder')} size={14} onClick={() => dispatch('fnew', side)} />
      </div>
      <div className="filelist">
        {error && <div className="emptyhint">{error}</div>}
        {!error && !entries.length && <div className="emptyhint">{T('sftp.emptyDir')}</div>}
        {entries.map((e) => {
          const target = e.isDir ? childPath(side, path, e.name) : undefined;
          return (
            <div
              key={e.name}
              className={'frow' + (e.isDir ? ' dir' : '') + (selected === e.name ? ' sel' : '')
                + (dragging === e.name ? ' dragging' : '') + (e.isDir && dirOver === e.name ? ' dir-over' : '')}
              data-name={e.name}
              data-dir-path={target}
              data-ctx="frow"
              data-arg={side + '|' + e.name}
              onPointerDown={(ev) => onRowPointerDown?.(side, ev, e.name, e.isDir)}
              onClick={() => {
                // 拖拽尾随的 click 不算“单击选中”（useHostDrag 同款处理）
                if (clickSuppressed?.()) return;
                selectFile(side, e.name);
              }}
              onDoubleClick={() => {
                if (e.isDir) openDir(side, e.name);
                else dispatch('fopen', side + '|' + e.name);
              }}
            >
              <span className="fi"><Icon n={e.isDir ? 'folder' : 'file'} s={14} /></span>
              <span className="fn">{e.name}{e.isDir ? sep : ''}</span>
              {/* 权限位（spec：SFTP 需展示权限）：远程为 POSIX 位，Windows 本地无该概念显示 — */}
              <span className="fp" title={e.mode == null ? '' : '0o' + (e.mode & 0o777).toString(8)}>{fmtMode(e.mode, e.isDir)}</span>
              <span className="fs">{e.isDir ? '—' : fmtSize(e.size)}</span>
              <span className="fd">{e.isDir ? T('sftp.folder') : ''}</span>
            </div>
          );
        })}
      </div>
    </div>
  );
}
