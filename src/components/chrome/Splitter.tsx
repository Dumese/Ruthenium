// 侧栏分隔条：拖拽 210–360px，宽度持久化。
// 拖拽期间的合帧与局部写入在 useResizable 内完成（高刷适配，见该文件注释）。
import { useAppStore } from '../../store/useAppStore';
import { useResizable } from '../../hooks/useResizable';

export function Splitter() {
  const setWidth = useAppStore((s) => s.setSidebarWidth);
  const onMouseDown = useResizable(setWidth);
  return <div className="splitter" onMouseDown={onMouseDown} />;
}
