// 新建/编辑主机表单 Sheet。校验 name/addr/port；私钥只存 Windows 路径。
// 认证方式三选一：key 才显示私钥路径；password 显示掩码输入 + 已保存/未保存状态行；
// agent 两者都不显示。密码明文只在本表单内存里，随切换/关闭/提交丢弃。
import { useState } from 'react';
import { defaultGroup } from '../../lib/defaults';
import { Sheet } from '../overlays/Sheet';
import { Field } from '../ui/Field';
import { Button } from '../ui/Button';
import { Switch } from '../ui/Switch';
import { Dot } from '../ui/Dot';
import { useAppStore } from '../../store/useAppStore';
import { foldersOf } from '../../store/selectors';
import { HOST_TAGS, normalizeHostTag } from '../../lib/tags';
import { T, useLang } from '../../i18n';
import { pickPrivateKey } from '../../lib/dialog';
import { isTauri } from '../../ipc/env';
import type { AuthMethod } from '../../types/models';

const AUTH: AuthMethod[] = ['key', 'password', 'agent'];

export function HostFormSheet() {
  useLang();
  const form = useAppStore((s) => s.form);
  const dispatch = useAppStore((s) => s.dispatch);
  const update = useAppStore((s) => s.updateForm);
  const showToast = useAppStore((s) => s.showToast);
  const saveFormPassword = useAppStore((s) => s.saveFormPassword);
  const folders = foldersOf();
  const edit = !!form?.id;
  const [group, setGroup] = useState(form?.group ?? defaultGroup(folders));
  const tagValue = normalizeHostTag(form?.tag);
  if (!form || form._t !== 'host') return null;
  const auth: AuthMethod = form.auth ?? 'key';
  const saved = !!form.saved;

  // 私钥只能从系统文件选择器选（不手输）：选中的主目录路径会被压成 `~\...`
  const browseKey = async () => {
    try {
      const picked = await pickPrivateKey(form.keyRef);
      if (picked) update('keyRef', picked);
      else if (!isTauri) showToast(T('hf.browseDesktopOnly'));
    } catch (e) {
      // 对话框打不开（权限/插件异常）不能静默——点了没反应是最难排查的形态
      showToast(T('err.opFailed', { 0: T('hf.key'), 1: (e as Error)?.message ?? String(e) }));
    }
  };
  // 切换认证方式：立即丢弃表单里的密码明文，并把 key_ref 语义留给保存时统一归一化
  const setAuth = (a: AuthMethod) => {
    if (a === auth) return;
    useAppStore.setState({ form: { ...form, auth: a, pw: '', pwCleared: false } });
  };
  // 「清除」只在表单内暂存：状态变未保存、清空输入框；点保存主机时才真正 delete_password
  const clearPw = () => {
    useAppStore.setState({ form: { ...form, pw: '', saved: false, pwCleared: !!form.id } });
  };

  return (
    <Sheet
      title={edit ? T('hf.edit') : T('hf.new')}
      foot={
        <>
          {edit && <Button icon="trash" onClick={() => dispatch('hostdel', form.id)}><span style={{ color: 'var(--red)' }}>{T('c.delete')}</span></Button>}
          <span className="sp" />
          <Button onClick={() => dispatch('closeov')}>{T('c.cancel')}</Button>
          <Button icon="check" primary onClick={() => dispatch('formsave')}>{T('hf.save')}</Button>
        </>
      }
    >
      <div className="grid2">
        <Field label={T('hf.name')}>
          <input className="inp" placeholder="web-03" value={form.name ?? ''} onChange={(e) => update('name', e.target.value)} />
        </Field>
        <Field label={T('hf.addr')}>
          <input className="inp" placeholder="203.0.113.10" value={form.addr ?? ''} onChange={(e) => update('addr', e.target.value)} />
        </Field>
      </div>
      <div className="grid2">
        <Field label={T('hf.user')}>
          <input className="inp" placeholder="deploy" value={form.user ?? ''} onChange={(e) => update('user', e.target.value)} />
        </Field>
        <Field label={T('hf.port')}>
          <input className="inp" placeholder="22" value={String(form.port ?? '22')} onChange={(e) => update('port', e.target.value)} />
        </Field>
      </div>
      <div className="grid2">
        <Field label={T('hf.group')}>
          <select className="inp" value={group} onChange={(e) => { setGroup(e.target.value); update('group', e.target.value); }}>
            {/* data-user：分组名是用户数据（i18n 残检据此跳过） */}
            {folders.map((g) => <option key={g.name} value={g.name} data-user="1">{g.name}</option>)}
          </select>
        </Field>
        <Field label={T('hf.tag')}>
          <select className="inp" data-tag="1" value={tagValue} onChange={(e) => update('tag', e.target.value)}>
            {HOST_TAGS.map((id) => <option key={id} value={id}>{T(`htag.${id}`)}</option>)}
            {/* 旧数据/手改的未知值：保留并显示原值，不悄悄改成默认项 */}
            {!(HOST_TAGS as readonly string[]).includes(tagValue) && <option value={tagValue} data-user="1">{tagValue}</option>}
          </select>
        </Field>
      </div>

      {/* 认证方式三选一（#authField 供覆盖表 goto 定位） */}
      <div className="field" id="authField" data-sec="auth">
        <label>{T('auth.label')}</label>
        <div className="pillrow" role="group" aria-label={T('auth.label')}>
          {AUTH.map((k) => (
            <button
              key={k}
              type="button"
              className={'pill' + (auth === k ? ' on' : '')}
              data-act="setauth"
              data-arg={k}
              aria-pressed={auth === k}
              onClick={() => setAuth(k)}
            >
              {T('auth.' + k)}
            </button>
          ))}
        </div>
      </div>

      {auth === 'key' && (
        <Field label={T('hf.key')}>
          <div className="pickrow">
            <input
              className="inp mono"
              value={form.keyRef ?? ''}
              placeholder={T('hf.keyPh')}
              readOnly={isTauri}
              onChange={(e) => update('keyRef', e.target.value)}
            />
            <Button icon="folder" title={T('hf.browseTitle')} onClick={() => void browseKey()}>
              {T('hf.browse')}
            </Button>
          </div>
        </Field>
      )}

      {auth === 'password' && (
        <Field label={T('auth.password')}>
          <input
            className="inp"
            type="password"
            data-act="field"
            data-arg="pw"
            value={form.pw ?? ''}
            placeholder={saved ? T('auth.pwSavedPh') : T('auth.pwPh')}
            autoComplete="new-password"
            spellCheck={false}
            onChange={(e) => update('pw', e.target.value)}
          />
          {/* 用户要求：输入密码的右下角提供「保存密码」，立即写 keyring；空输入禁用 */}
          <div className="pwsave">
            <button
              type="button"
              className="tbtn"
              data-act="savepw"
              disabled={!String(form.pw ?? '').trim()}
              onClick={() => void saveFormPassword()}
            >
              {T('auth.save')}
            </button>
          </div>
          <div className="authrow">
            <Dot state={saved ? 'on' : 'off'} />
            <span>{saved ? T('auth.saved') : T('auth.unsaved')}</span>
            <span className="sp" />
            {saved && <button type="button" className="tbtn" data-act="savepass" onClick={clearPw}>{T('auth.clear')}</button>}
          </div>
        </Field>
      )}

      <Field label={T('hf.note')}>
        <input className="inp" placeholder={T('hf.notePh')} value={form.note ?? ''} onChange={(e) => update('note', e.target.value)} />
      </Field>
      <div className="sec">
        <div className="sectitle">{T('hf.secConn')}</div>
        {/* 连接成功后自动执行的一行命令（PTY 就绪即写入，不等待提示符） */}
        <Field label={T('hf.initCmd')}>
          <input
            className="inp mono"
            data-act="field"
            data-arg="initCommand"
            value={form.initCommand ?? ''}
            placeholder={T('hf.initCmdPh')}
            spellCheck={false}
            onChange={(e) => update('initCommand', e.target.value)}
          />
        </Field>
        <div className="setrow">
          <div className="sr">
            <div className="srt">{T('hf.verify')}</div>
          </div>
          {/* 开关语义 = "首次连接校验指纹"，与 known（已信任）相反：
              开 = 需要校验 = known false。开关不能直接绑到 known 上，
              否则"开启校验"其实表示"已信任"，与文案相反。 */}
          <Switch on={!form.known} onToggle={() => update('known', !!form.known)} label={T('hf.verify')} />
        </div>
        <div className="setrow">
          <div className="sr">
            <div className="srt">{T('hf.agent')}</div>
          </div>
          <Switch on={!!form.agent} onToggle={() => update('agent', !form.agent)} label={T('hf.agent')} />
        </div>
        {/* 每主机「记录会话日志」开关（默认值来自设置里的全局默认） */}
        <div className="setrow" data-setting="hostlog">
          <div className="sr">
            <div className="srt">{T('hf.log')}</div>
          </div>
          <Switch on={!!form.log} onToggle={() => update('log', !form.log)} label={T('hf.log')} act="switchfield" arg="log" />
        </div>
      </div>
    </Sheet>
  );
}
