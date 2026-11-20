import { Icon } from './Icon';

/** 面包屑（.crumb）。sep 可选（本地 Windows 用 \，远程 POSIX 用 /）；
 *  acc 保留原始分隔符拼出的路径；根按钮的跳转目标由 `rootPath` 决定，
 *  `rootLabel` 只影响显示（本地根是 ''「此电脑」，远端根是 '/'）。 */
export function Breadcrumb({ parts, onGo, sep = '/', rootLabel, rootPath }: {
  parts: string[]; onGo: (path: string) => void; sep?: string; rootLabel?: string; rootPath?: string;
}) {
  let acc = '';
  return (
    <div className="crumb">
      <button onClick={() => onGo(rootPath ?? '/')}>
        {rootLabel != null ? rootLabel : <Icon n="home" s={12} />}
      </button>
      {parts.map((p, i) => {
        acc = acc ? acc + sep + p : p;
        const path = acc;
        return (
          <span key={i} style={{ display: 'contents' }}>
            <span className="cx">{sep}</span>
            <button onClick={() => onGo(path)}>{p}</button>
          </span>
        );
      })}
    </div>
  );
}
