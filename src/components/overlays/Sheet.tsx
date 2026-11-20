// 右侧滑入 Sheet 外壳：scrim 点击 / Esc 关闭、焦点圈定（.sheet；wide=560px 覆盖表宽版）
import type { ReactNode } from 'react';
import { Icon } from '../ui/Icon';
import { IconButton } from '../ui/IconButton';
import { useAppStore } from '../../store/useAppStore';
import { useFocusTrap } from '../../hooks/useFocusTrap';
import { T, useLang } from '../../i18n';

export function Sheet({ title, sub, children, foot, wide }: {
  title: string; sub?: string; children: ReactNode; foot?: ReactNode; wide?: boolean;
}) {
  useLang();
  const closeOverlay = useAppStore((s) => s.closeOverlay);
  useFocusTrap(true);
  return (
    <>
      <div className="scrim" onClick={closeOverlay} />
      <div className={'sheet' + (wide ? ' wide' : '')}>
        <div className="sheethead">
          <div className="sh">
            <h3>{title}</h3>
            {sub && <p>{sub}</p>}
          </div>
          <IconButton icon="x" title={T('c.close')} onClick={closeOverlay} />
        </div>
        <div className="sheetbody">{children}</div>
        {foot && <div className="sheetfoot">{foot}</div>}
      </div>
    </>
  );
}

/** 居中 Modal 外壳（.modal）。点遮罩（外部）即取消。
 *  非 overlay 驱动的确认弹窗（配对/控制/屏幕）必须传 onClose，否则会误关底层的 Sheet。 */
export function Modal({ children, onClose }: { children: ReactNode; onClose?: () => void }) {
  useFocusTrap(true);
  const closeOverlay = useAppStore((s) => s.closeOverlay);
  return (
    <>
      <div className="scrim" onClick={onClose ?? closeOverlay} />
      <div className="modal" role="dialog" aria-modal="true">{children}</div>
    </>
  );
}

/** 模态图标（.modalbody .mi） */
export function ModalIcon({ icon, danger }: { icon: string; danger?: boolean }) {
  return (
    <div className="mi" style={danger ? { background: 'rgba(217,44,32,.1)', color: 'var(--red)' } : undefined}>
      <Icon n={icon} s={24} />
    </div>
  );
}
