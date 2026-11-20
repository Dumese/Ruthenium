// Toast 宿主
import { useAppStore } from '../../store/useAppStore';
import { Toast } from '../ui/Toast';

export function ToastHost() {
  const toast = useAppStore((s) => s.toast);
  if (!toast) return null;
  return <Toast msg={toast} />;
}
