// 表单 slice：主机 / 片段 / 转发规则表单、SSH 配置导入、备份导出导入、
// 主机删除与分组移动、片段插入与转发开关。
import { defaultGroup } from '../../lib/defaults';
import { normalizeHostTags, normalizeHostTag } from '../../lib/tags';
import { sshCommand } from '../../lib/sshConfigParser';
import { pickSavePath } from '../../lib/dialog';
import { setLang as i18nSetLang, T, terr } from '../../i18n';
import { isTauri } from '../../ipc/env';
import { IMPORT_SAMPLE } from '../../ipc/mockBridge';
import { uid } from '../../lib/ids';
import { applyGpuMode, pasteToTerminal } from '../../components/terminal/useXterm';
import * as api from '../../ipc/commands';
import type { Config, Host, ParsedHost, Preferences, Snippet, TunnelRule } from '../../types/models';
import type { AppStore } from '../types';
import type { SliceCreator } from './shared';
import { normalizePrefs } from './prefsSlice';
import { pwRetries, hostFromForm, resolveFormPort } from './guards';

/** 保存进行中：按钮双击时不能再生成第二个 uid/第二次 keyring 写入。 */
let formSaving = false;

export interface FormSlice {
  form: AppStore['form'];
  importText: string | null; importParsed: ParsedHost[]; importSelected: Record<number, boolean>;

  // 表单
  openHostForm(host?: Host): void;
  openSnipForm(snip?: Snippet): void;
  openRuleForm(rule?: TunnelRule, hostId?: string): void;
  updateForm(key: string, value: unknown): void;
  saveForm(): Promise<void>;
  /** 表单「保存密码」：立即写 keyring；成功后状态切已保存并清空输入 */
  saveFormPassword(): Promise<void>;

  // 主机
  deleteHostById(id: string): Promise<void>;
  /** 拖拽主机到其它分组：乐观更新界面，落盘失败回滚并提示 */
  moveHostToGroup(hostId: string, group: string): Promise<void>;
  copySsh(id: string): void;

  // 导入
  openImport(): void; setImportText(t: string): void;
  parseImportText(): Promise<void>; toggleImportRow(i: number): void;
  importBack(): void; importSelectedRows(): Promise<void>;

  // 备份
  exportBackupJson(): void;
  /** 桌面版：原生文件选择器拿到路径后走 IPC 整体导入 */
  importBackupFile(path: string): void;
  /** 把一份完整 Config 一次性应用到 UI 状态（导入备份后全量刷新） */
  applyImportedConfig(cfg: Config): void;
  applyBackup(json: string): void;

  // 片段 / 转发
  insertSnippet(id: string): Promise<void>;
  deleteSnippetById(id: string): Promise<void>;
  deleteRuleById(id: string): Promise<void>;
  toggleRule(id: string): Promise<void>;
}

export const createFormSlice: SliceCreator<FormSlice> = (set, get) => ({
  form: null,
  importText: null, importParsed: [], importSelected: {},

  /* ---------- 表单 ---------- */
  openHostForm(host) {
    set({
      form: host
        ? {
          _t: 'host', id: host.id, name: host.name, addr: host.addr, user: host.user, port: String(host.port),
          group: host.group, tag: host.tag, keyRef: host.keyRef, auth: host.auth ?? 'key',
          known: host.known, agent: host.agent, note: host.note, saved: false,
          initCommand: host.initCommand ?? '', log: !!host.log,
        }
        : {
          // `known` 表示"指纹已信任（known_hosts 有记录）"，新建主机必须从未信任开始，
          // 否则刚建的主机在详情页就显示"已信任"，且 dev/mock 链路会跳过指纹挑战。
          _t: 'host', name: '', addr: '', user: '', port: '22', group: defaultGroup(get().folders),
          tag: 'dev', keyRef: '~\\.ssh\\id_ed25519', auth: 'key', known: false, agent: true, note: '', saved: false,
          // 新主机「记录会话日志」默认跟随全局默认开关
          initCommand: '', log: !!get().logSessions,
        },
      overlay: { kind: 'sheet', id: 'hostform' },
    });
    // 「已保存密码」是派生态：只问 has_password，绝不回显密码内容
    if (host?.id) {
      void api.hasPassword(host.id).then((saved) => {
        const f = get().form;
        if (f?._t === 'host' && f.id === host.id && !f.pw) set({ form: { ...f, saved } });
      }).catch(() => { /* keyring 不可用：按未保存处理 */ });
    }
  },
  openSnipForm(snip) {
    set({ form: snip ? { _t: 'snip', id: snip.id, name: snip.name, cmd: snip.cmd, tag: snip.tag } : { _t: 'snip', name: '', cmd: '', tag: '诊断' }, overlay: { kind: 'sheet', id: 'small' } });
  },
  openRuleForm(rule, hostId) {
    set({
      form: rule
        ? { _t: 'rule', id: rule.id, name: rule.name, local: rule.local, remote: rule.remote, kind: rule.kind, host: rule.host, on: rule.on }
        : { _t: 'rule', name: '', local: '127.0.0.1:', remote: '', kind: 'local', host: hostId ?? get().hosts[0]?.id ?? '', on: false },
      overlay: { kind: 'sheet', id: 'small' },
    });
  },
  updateForm(key, value) {
    const f = get().form;
    if (f) set({ form: { ...f, [key]: value } });
  },
  async saveForm() {
    const f = get().form;
    if (!f || formSaving) return;
    if (!f.name || !String(f.name).trim()) { get().showToast(T('toast.needName')); return; }
    formSaving = true;
    try {
      if (f._t === 'snip') {
        await api.upsertSnippet({ id: f.id ?? uid(), name: f.name, cmd: f.cmd ?? '', tag: f.tag ?? '诊断' });
        get().showToast(T('toast.snipSaved'));
      } else if (f._t === 'rule') {
        // 编辑时保留原有 on：转发规则表单没有启用开关，固定写 false
        // 会“只改个名字就把已启用规则静默关掉”。
        const existing = f.id ? get().tunnels.find((t) => t.id === f.id) : undefined;
        await api.upsertTunnel({
          id: f.id ?? uid(), name: f.name, kind: f.kind ?? 'local',
          local: f.local ?? '', remote: f.remote ?? '', host: f.host ?? get().hosts[0]?.id ?? '',
          on: f.on ?? existing?.on ?? false,
        });
        get().showToast(T('toast.ruleSaved'));
      } else {
        // 认证方式随主机落盘；key_ref 只在 key 时有值，否则置空串
        const auth = f.auth ?? 'key';
        // 端口必须显式校验——空串按 22，非法值提示后中止保存。
        // 绝不把非法值静默写成 0（0 是本地终端哨兵，会让"远程命令"落在本机）。
        const port = resolveFormPort(f);
        if (port === null) { get().showToast(T('toast.portInvalid')); return; }
        const host = hostFromForm(f, defaultGroup(get().folders), port);
        const saved = await api.upsertHost(host);
        // 后端可能回填 id：新建主机必须先落盘拿到 id，才能写 keyring
        const hostId = (saved as Host | undefined)?.id ?? host.id;
        const created = !f.id;
        // 密码只走 keyring：先落盘生成 id，再写 keyring；失败就报错并保持「未保存」
        if (auth === 'password') {
          const pw = String(f.pw ?? '');
          if (pw) {
            try {
              await api.setPassword(hostId, pw);
            } catch (e) {
              const withId = { ...f, id: hostId, saved: false, pw };
              set({ form: withId });
              get().showToast(T('err.saveFailed', { 0: terr((e as { code?: string })?.code, (e as { detail?: string })?.detail) }));
              return;
            }
          } else if (f.pwCleared && !created) {
            try {
              await api.deletePassword(hostId);
            } catch (e) {
              set({ form: { ...f, id: hostId, saved: true, pwCleared: false } });
              get().showToast(T('err.saveFailed', { 0: terr((e as { code?: string })?.code, (e as { detail?: string })?.detail) }));
              return;
            }
          }
          pwRetries.delete(hostId);
        }
        get().showToast(created ? T('toast.hostAdded') : T('toast.hostUpdated'));
      }
      set({ form: null, overlay: null });
      await get().reload();
    } catch (e) {
      get().showToast(T('err.saveFailed', { 0: terr((e as { code?: string })?.code, (e as { detail?: string })?.detail) }));
    } finally {
      formSaving = false;
    }
  },
  /**
   * 表单里的「保存密码」：立即写 keyring，成功后状态切「已保存密码」并清空输入。
   * 新建主机还没有 host id：按「先落盘生成 id，再写 keyring」的顺序先 upsert 一次。
   * 失败时不改状态（保持未保存），按现有错误 Toast 处理。
   */
  async saveFormPassword() {
    const f = get().form;
    if (!f || formSaving || f._t !== 'host' || (f.auth ?? 'key') !== 'password') return;
    const pw = String(f.pw ?? '');
    if (!pw) return;
    formSaving = true;
    try {
    let hostId = f.id;
    if (!hostId) {
      if (!f.name || !String(f.name).trim()) { get().showToast(T('toast.needName')); return; }
      try {
        const port = resolveFormPort(f);
        if (port === null) { get().showToast(T('toast.portInvalid')); return; }
        const host = hostFromForm(f, defaultGroup(get().folders), port);
        const saved = await api.upsertHost(host);
        hostId = (saved as Host | undefined)?.id ?? host.id;
      } catch (e) {
        get().showToast(T('err.saveFailed', { 0: terr((e as { code?: string })?.code, (e as { detail?: string })?.detail) }));
        return;
      }
    }
    try {
      await api.setPassword(hostId, pw);
      const cur = get().form;
      if (!cur || cur._t !== 'host' || (cur.auth ?? 'key') !== 'password') return;
      set({ form: { ...cur, id: hostId, saved: true, pw: '', pwCleared: false } });
      pwRetries.delete(hostId);
      // 新建主机：密码按钮也会把主机落盘，侧栏/表格立即能看到
      if (!f.id) await get().reload();
    } catch (e) {
      get().showToast(T('err.saveFailed', { 0: terr((e as { code?: string })?.code, (e as { detail?: string })?.detail) }));
    }
    } finally {
      formSaving = false;
    }
  },

  /* ---------- 主机 ---------- */
  async moveHostToGroup(hostId, group) {
    const host = get().hosts.find((h) => h.id === hostId);
    if (!host || host.group === group) return;   // 同组拖放是 no-op：不写盘、不提示
    const prev = host.group;
    // 乐观更新：先让侧栏立刻反映结果；目标分组若折叠则展开，让用户看到落点
    set({
      hosts: get().hosts.map((h) => (h.id === hostId ? { ...h, group } : h)),
      closedGroups: { ...get().closedGroups, [group]: false },
    });
    try {
      await api.upsertHost({ ...host, group });
      get().showToast(T('toast.hostMoved', { 0: host.name, 1: group }));
    } catch (e) {
      // 落盘失败（只读配置 / 磁盘满）：回滚，避免界面与 hosts.toml 不一致
      set({ hosts: get().hosts.map((h) => (h.id === hostId ? { ...h, group: prev } : h)) });
      get().showToast(T('err.saveFailed', { 0: terr((e as { code?: string })?.code, (e as { detail?: string })?.detail) }));
    }
  },
  async deleteHostById(id) {
    pwRetries.delete(id);
    // 落盘失败必须中止并提示——否则界面显示已删除、hosts.toml 未变，
    // 重启/重新 loadConfig 后主机会"复活"（同 moveHostToGroup / saveForm 的处理）。
    try {
      await api.deleteHost(id);
    } catch (e) {
      get().showToast(T('err.deleteFailed', { 0: terr((e as { code?: string })?.code, (e as { detail?: string })?.detail) }));
      return;
    }
    // 主机删除成功后再清钥匙串（后端也会 best-effort 清一次）。
    // 顺序不能反：先删密码再删主机，delete_host 失败时密码已被误删（主机仍在却要重新输入）。
    try { await api.deletePassword(id); } catch { /* keyring 不可用不阻断删除主机 */ }
    const sessions = get().sessions.filter((x) => x.hostId === id);
    for (const s of sessions) {
      // 统一走 closeSession：screen 需要 peerCloseScreen，终端需要级联派生 SFTP。
      await get().closeSession(s.id);
    }
    const rest = get().sessions.filter((x) => x.hostId !== id);
    const active = rest.some((x) => x.id === get().active) ? get().active : (rest.length ? rest[0].id : null);
    const ov = get().overlay;
    set({
      hosts: get().hosts.filter((x) => x.id !== id), sessions: rest, active,
      selectedHost: get().selectedHost === id ? null : get().selectedHost,
      // 删除时若正停在该主机的详情/编辑 Sheet，一并收起（否则面板对着已删除的 id 渲染空壳）。
      overlay: ov?.kind === 'menu'
        ? null
        : ov?.kind === 'sheet' && (ov.id === 'hostform' || ov.id === 'hostdetail') && ov.arg === id
          ? null
          : ov,
      form: get().form?.id === id ? null : get().form,
    });
    get().showToast(T('toast.hostDeleted'));
  },
  copySsh(id) {
    const h = get().hosts.find((x) => x.id === id);
    if (!h) return;
    const cmd = sshCommand(h);
    void navigator.clipboard?.writeText(cmd).catch(() => { /* ignore */ });
    get().showToast(T('toast.copied', { 0: cmd }));
  },

  /* ---------- 导入 ---------- */
  openImport() { set({ importParsed: [], importSelected: {}, importText: null, overlay: { kind: 'sheet', id: 'import' } }); },
  setImportText(t) { set({ importText: t }); },
  async parseImportText() {
    const s = get();
    const raw = s.importText ?? IMPORT_SAMPLE;
    try {
      const parsed = await api.importSshConfig(raw);
      const sel: Record<number, boolean> = {};
      // 与后端一致：重复按 addr + port 比对（config.rs parse_ssh_config / commit_hosts_into）
      parsed.forEach((x, i) => { sel[i] = !x.duplicate && !s.hosts.some((h) => h.addr === x.addr && h.port === x.port); });
      set({ importParsed: parsed, importSelected: sel });
    } catch (e) {
      get().showToast(T('err.parseFailed', { 0: terr((e as { code?: string })?.code, (e as { detail?: string })?.detail) }));
    }
  },
  toggleImportRow(i) { set({ importSelected: { ...get().importSelected, [i]: !get().importSelected[i] } }); },
  importBack() { set({ importParsed: [] }); },
  async importSelectedRows() {
    const s = get();
    const hosts: Host[] = s.importParsed
      // 与后端 commit_import 一致：重复按 addr + port 跳过
      .filter((x, i) => s.importSelected[i] && !s.hosts.some((h) => h.addr === x.addr && h.port === x.port))
      .map((x) => ({
        id: uid(), name: x.name, addr: x.addr, user: x.user || 'root', port: x.port || 22,
        group: x.group || defaultGroup(get().folders), tag: x.tag || 'dev', keyRef: x.keyRef || '~\\.ssh\\id_ed25519',
        auth: 'key' as const, known: false, agent: true, note: x.note || '从 %USERPROFILE%\\.ssh\\config 导入',
        initCommand: '', log: false,
      }));
    try {
      const r = await api.commitImport(hosts);
      set({ overlay: null });
      await get().reload();
      get().showToast(r ? T('toast.imported', { 0: r }) : T('toast.importNone'));
    } catch (e) {
      get().showToast(T('toast.importFailed', { 0: terr((e as { code?: string })?.code, (e as { detail?: string })?.detail) }));
    }
  },

  /* ---------- 备份（完整配置整体导出 / 整体替换，不含秘密与日志） ---------- */
  exportBackupJson() {
    if (isTauri) {
      // 桌面版：原生「另存为」→ export_backup(save_token)（token 一次性消费，
      // path 只用于展示，WebView 无法让后端写任意路径）
      void (async () => {
        try {
          const picked = await pickSavePath('ruthenium-backup.json', T('bk.saveTitle'));
          if (!picked) return; // 用户取消
          await api.exportBackup(picked.token);
          // 明确告知导出到了哪个文件：只提示「已导出」时用户不知道文件在哪，
          // 容易以为功能没生效。
          get().showToast(T('toast.exportedTo', { 0: picked.path }));
        } catch (e) {
          get().showToast(T('err.opFailed', {
            0: T('bk.export'),
            1: terr((e as { code?: string })?.code, (e as { detail?: string })?.detail ?? (e as Error)?.message),
          }));
        }
      })();
      return;
    }
    // 纯浏览器 / mock（E2E）：没有原生保存对话框，降级为 Blob 下载，保持端到端可测。
    // 不包含 keyRef（私钥只引用路径，不入备份）、不包含密码、会话日志与 known_hosts。
    const s = get();
    const data = {
      version: 1, exportedAt: new Date().toISOString(),
      folders: s.folders,
      hosts: s.hosts, snippets: s.snippets, tunnels: s.tunnels,
      preferences: {
        lang: s.lang, theme: s.theme, nav: s.nav, fontSize: s.fontSize, lineHeight: s.lineHeight, font: s.font, cursor: s.cursor,
        showStatus: s.showStatus, showDots: s.showDots, showLineNumbers: s.showLineNumbers,
        confirmClose: s.confirmClose, restoreTabs: s.restoreTabs, sidebarWidth: s.sidebarWidth,
        gpuMode: s.gpuMode, autoReconnect: s.autoReconnect, logSessions: s.logSessions,
        keepaliveSecs: s.keepaliveSecs, requireWindowsHello: s.requireWindowsHello,
        lanVisible: s.lanVisible, lanDeviceName: s.lanDeviceName, lanPort: s.lanPort, peerShareRoot: s.peerShareRoot,
        lanStrictPairing: s.lanStrictPairing,
      },
    };
    const blob = new Blob([JSON.stringify(data, null, 2)], { type: 'application/json' });
    const a = document.createElement('a');
    a.href = URL.createObjectURL(blob);
    a.download = 'ruthenium-backup.json';
    document.body.appendChild(a); a.click(); a.remove();
    setTimeout(() => URL.revokeObjectURL(a.href), 1500);
    get().showToast(T('toast.exported'));
  },
  importBackupFile(token) {
    if (!isTauri) return;
    void (async () => {
      try {
        // 后端返回整体替换后的完整 Config：用它一次刷新 hosts/folders/snippets/tunnels/preferences
        // （token 由「选择备份文件」对话框签发，一次性消费）
        const cfg = await api.importBackup(token);
        get().applyImportedConfig(cfg);
        get().showToast(T('toast.restored', { 0: (cfg.hosts ?? []).length }));
      } catch (e) {
        get().showToast(T('toast.importFailed', {
          0: terr((e as { code?: string })?.code, (e as { detail?: string })?.detail ?? (e as Error)?.message),
        }));
      }
    })();
  },
  applyImportedConfig(cfg) {
    const prefs = normalizePrefs(cfg.preferences);
    const hosts = normalizeHostTags(Array.isArray(cfg.hosts) ? cfg.hosts : []);
    const selected = get().selectedHost;
    // 整体替换后旧会话/隧道仍指向旧主机配置；先逐个走统一关闭流程
    // （screen 需要 peerCloseScreen，终端需要级联派生 SFTP）。
    for (const s of [...get().sessions]) void get().closeSession(s.id);
    // 同步运行态偏好（设备名/严格配对），并让局域网开关立即生效。
    void api.savePreferences(prefs).catch(() => { /* ignore */ });
    void api.peerSetVisible(prefs.lanVisible).catch(() => { /* 端口不可用等，设置页会显示 */ });
    set({
      folders: cfg.folders ?? [],
      hosts,
      snippets: Array.isArray(cfg.snippets) ? cfg.snippets : [],
      tunnels: Array.isArray(cfg.tunnels) ? cfg.tunnels : [],
      ...prefs,
      selectedHost: selected && hosts.some((h) => h.id === selected) ? selected : null,
    });
    applyGpuMode(prefs.gpuMode);
    i18nSetLang(prefs.lang);
    // 整体替换后，仍在打开的主机表单 / 详情可能引用已不存在的主机：收起
    const o = get().overlay;
    if (o?.kind === 'sheet' && (o.id === 'hostform' || o.id === 'hostdetail')) set({ overlay: null, form: null });
  },
  applyBackup(json) {
    // 浏览器 / mock（E2E）降级：原生对话框不可用，从用户选择的文件文本恢复完整配置。
    try {
      const d = JSON.parse(json) as {
        hosts?: Host[]; snippets?: Snippet[]; tunnels?: TunnelRule[]; preferences?: Partial<Preferences>;
        folders?: { name: string; color: string }[];
      };
      if (!Array.isArray(d.hosts)) throw new Error(T('toast.badFormat'));
      const hosts: Host[] = d.hosts.map((h, i) => {
        const auth = h.auth ?? 'key';
        return {
          id: h.id || `bk-host-${i}`,
          name: String(h.name ?? ''),
          addr: String(h.addr ?? ''),
          user: String(h.user ?? ''),
          port: h.port === 0 ? 0 : (Number(h.port) || 22),
          group: String(h.group ?? ''),
          tag: normalizeHostTag(h.tag),
          auth,
          // 后端 import_backup 相同语义：备份不含私钥路径，导入后一律留空
          keyRef: '',
          known: !!h.known,
          agent: h.agent ?? true,
          note: String(h.note ?? ''),
          initCommand: '',
          log: false,
        };
      });
      const snippets: Snippet[] = (Array.isArray(d.snippets) ? d.snippets : []).map((n, i) => ({
        id: n.id || `bk-snip-${i}`,
        name: String(n.name ?? ''),
        cmd: String(n.cmd ?? ''),
        tag: String(n.tag ?? ''),
      })).filter((n) => n.name.trim() !== '');
      const tunnels: TunnelRule[] = (Array.isArray(d.tunnels) ? d.tunnels : []).map((r, i) => ({
        id: r.id || `bk-rule-${i}`,
        name: String(r.name ?? ''),
        kind: r.kind === 'remote' ? ('remote' as const) : ('local' as const),
        local: String(r.local ?? ''),
        remote: String(r.remote ?? ''),
        host: String(r.host ?? ''),
        on: !!r.on,
      })).filter((r) => r.name.trim() !== '');
      // 分组：新备份带 folders（含颜色/空分组）；备份没有该字段时用 host.group 兜底，
      // 与后端 parse_backup_config 保持同一规则。
      const folders = Array.isArray(d.folders) && d.folders.length
        ? d.folders
          .map((f) => ({ name: String(f?.name ?? '').trim(), color: String(f?.color ?? '') }))
          .filter((f) => f.name)
        : (() => {
          const seen: string[] = [];
          for (const h of hosts) {
            const g = (h.group ?? '').trim();
            if (g && !seen.includes(g)) seen.push(g);
          }
          return seen.map((name) => ({ name, color: '' }));
        })();
      const cfg: Config = {
        version: 1,
        preferences: normalizePrefs(d.preferences),
        folders,
        hosts,
        snippets,
        tunnels,
      };
      get().applyImportedConfig(cfg);
      get().showToast(T('toast.restored', { 0: hosts.length }));
    } catch (e) {
      get().showToast(T('toast.importFailed', { 0: (e as Error).message }));
    }
  },

  /* ---------- 片段 / 转发 ---------- */
  async insertSnippet(id) {
    const n = get().snippets.find((x) => x.id === id);
    if (!n) return;
    const s = await get().ensureTerm();
    set({ overlay: null });
    // 没有活动终端：ensureTerm 已提示「先连接或建本地终端」，不要再盖成「已写入」
    if (!s) return;
    pasteToTerminal(s.id, n.cmd);
    get().showToast(T('toast.draftReady'));
  },
  async deleteSnippetById(id) {
    // 删失败要提示并保留本地状态，不能"提示已删除但磁盘还在"。
    try {
      await api.deleteSnippet(id);
    } catch (e) {
      get().showToast(T('err.deleteFailed', { 0: terr((e as { code?: string })?.code, (e as { detail?: string })?.detail) }));
      return;
    }
    set({ snippets: get().snippets.filter((x) => x.id !== id) });
    get().showToast(T('toast.snipDeleted'));
  },
  async deleteRuleById(id) {
    // 同上：删失败要提示并保留本地状态。
    try {
      await api.deleteTunnel(id);
    } catch (e) {
      get().showToast(T('err.deleteFailed', { 0: terr((e as { code?: string })?.code, (e as { detail?: string })?.detail) }));
      return;
    }
    // 删掉规则时同步丢弃运行态，避免 tunnelState 里留下幽灵条目。
    const nextState = { ...get().tunnelState };
    delete nextState[id];
    set({ tunnels: get().tunnels.filter((x) => x.id !== id), tunnelState: nextState });
    get().showToast(T('toast.ruleDeleted'));
  },
  async toggleRule(id) {
    const r = get().tunnels.find((x) => x.id === id);
    if (!r) return;
    const target = !r.on;
    // 先落到目标值：快速双击不会都读到旧的 r.on 而发出两次 true。
    set({ tunnels: get().tunnels.map((x) => (x.id === id ? { ...x, on: target } : x)) });
    try {
      await api.toggleTunnel(id, target);
      get().showToast(T(target ? 'toast.ruleOn' : 'toast.ruleOff', { 0: r.name }));
    } catch (e) {
      set({ tunnels: get().tunnels.map((x) => (x.id === id ? { ...x, on: !target } : x)) });
      // 用错误自身的 code 翻译——E_NOT_FOUND（规则已删）/ 写盘失败等
      // 各有正确文案，统一报成“端口绑定失败”会误导排查。
      get().showToast(terr((e as { code?: string })?.code, (e as { detail?: string })?.detail));
    }
  },
});
