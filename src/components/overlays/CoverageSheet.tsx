// 覆盖表：30 行全部真实可达（宽版 560px Sheet；goto 含 conninfo/lang/encoding/auth/ctxmenu）
import { Sheet } from './Sheet';
import { Button } from '../ui/Button';
import { useAppStore } from '../../store/useAppStore';
import { T, useLang } from '../../i18n';

// [功能 key, 组件 key, 入口 key, goto]
const COV: [string, string, string, string][] = [
  ['cov.r1f', 'cov.r1c', 'cov.r1e', 'hosts'],
  ['cov.r2f', 'cov.r2c', 'cov.r2e', 'groupsearch'],
  ['cov.r3f', 'cov.r3c', 'cov.r3e', 'hostform'],
  ['cov.r4f', 'cov.r4c', 'cov.r4e', 'hostdetail'],
  ['cov.r5f', 'cov.r5c', 'cov.r5e', 'hostdel'],
  ['cov.r6f', 'cov.r6c', 'cov.r6e', 'import'],
  ['cov.r7f', 'cov.r7c', 'cov.r7e', 'hostkey'],
  ['cov.r8f', 'cov.r8c', 'cov.r8e', 'connect'],
  ['cov.r9f', 'cov.r9c', 'cov.r9e', 'disconnect'],
  ['cov.r10f', 'cov.r10c', 'cov.r10e', 'empty'],
  ['cov.r11f', 'cov.r11c', 'cov.r11e', 'terminal'],
  ['cov.r12f', 'cov.r12c', 'cov.r12e', 'snippets'],
  ['cov.r13f', 'cov.r13c', 'cov.r13e', 'snipinsert'],
  ['cov.r14f', 'cov.r14c', 'cov.r14e', 'sftp'],
  ['cov.r15f', 'cov.r15c', 'cov.r15e', 'transfer'],
  ['cov.r16f', 'cov.r16c', 'cov.r16e', 'rules'],
  ['cov.r17f', 'cov.r17c', 'cov.r17e', 'ruletoggle'],
  ['cov.r18f', 'cov.r18c', 'cov.r18e', 'palette'],
  ['cov.r19f', 'cov.r19c', 'cov.r19e', 'settings'],
  ['cov.r20f', 'cov.r20c', 'cov.r20e', 'theme'],
  ['cov.r21f', 'cov.r21c', 'cov.r21e', 'backup'],
  ['cov.r22f', 'cov.r22c', 'cov.r22e', 'focus'],
  ['cov.r23f', 'cov.r23c', 'cov.r23e', 'sidebar'],
  ['cov.r24f', 'cov.r24c', 'cov.r24e', 'xnav'],
  ['cov.r25f', 'cov.r25c', 'cov.r25e', 'status'],
  ['cov.r26f', 'cov.r26c', 'cov.r26e', 'conninfo'],
  ['cov.r27f', 'cov.r27c', 'cov.r27e', 'lang'],
  ['cov.r28f', 'cov.r28c', 'cov.r28e', 'encoding'],
  ['cov.r29f', 'cov.r29c', 'cov.r29e', 'auth'],
  ['cov.r30f', 'cov.r30c', 'cov.r30e', 'ctxmenu'],
];

export function CoverageSheet() {
  useLang();
  const dispatch = useAppStore((s) => s.dispatch);
  return (
    <Sheet
      wide
      title={T('cov.title')}
      foot={
        <>
          <span className="sp" />
          <Button primary onClick={() => dispatch('closeov')}>{T('c.done')}</Button>
        </>
      }
    >
      <table className="covtable">
        <thead><tr><th>{T('cov.thFeature')}</th><th>{T('cov.thComp')}</th><th>{T('cov.thEntry')}</th><th></th></tr></thead>
        <tbody>
          {COV.map((r) => (
            <tr key={r[3]}>
              <td>{T(r[0])}</td><td>{T(r[1])}</td><td>{T(r[2])}</td>
              <td style={{ textAlign: 'right' }}>
                <button className="go" onClick={() => dispatch('goto', r[3])}>{T('cov.go')}</button>
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </Sheet>
  );
}
