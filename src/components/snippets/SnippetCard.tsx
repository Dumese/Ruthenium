// 片段卡片 + 卡片网格页
import { Chip } from '../ui/Chip';
import { Button } from '../ui/Button';
import { useAppStore } from '../../store/useAppStore';
import { T, useLang } from '../../i18n';
import type { Snippet } from '../../types/models';

/** 单张片段卡片（.dpcard） */
export function SnippetCard({ snip }: { snip: Snippet }) {
  useLang();
  const dispatch = useAppStore((s) => s.dispatch);
  const askConfirm = useAppStore((s) => s.askConfirm);
  return (
    <div className="dpcard">
      <h4>{snip.name}</h4>
      <p><Chip>{snip.tag}</Chip></p>
      <span className="mono">{snip.cmd}</span>
      <div className="dprow">
        <Button primary onClick={() => dispatch('snipinsert', snip.id)}>{T('snips.insert')}</Button>
        <Button icon="edit" title={T('c.edit')} onClick={() => dispatch('snipedit', snip.id)} />
        <Button icon="trash" title={T('c.delete')} onClick={() => askConfirm('snipdel', snip.id, T('c.delete'), T('toast.delFileWarn', { 0: snip.name }))} />
      </div>
    </div>
  );
}
