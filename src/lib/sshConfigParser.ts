// ~/.ssh/config 前端预览解析（后端 commands/config.rs parse_ssh_config 为权威实现）。
// 与后端对齐的行为点：
// - 一行多模式 `Host a b` 整组只产出一条（name 取第一个模式），其余模式丢弃；
// - 任一模式含 * / ? 时整组丢弃（不只是过滤 name === '*'）；
// - Match 行：flush 当前组，块内键值全部忽略，直到下一个 Host；
// - keyRef 默认空串（只有 IdentityFile 会写入）；group / tag / note 默认空串
//   （分组 / 标签 / 备注的落库默认由 store importSelectedRows 的回退值统一补齐）；
// - Port 仅在 0–65535 整数时生效，否则保持默认 22；addr 缺省回退为 name；
// - duplicate 按 addr + port 与已产出条目比对（store 预览与 mock 同规则）。
import type { ParsedHost } from '../types/models';

function blankHost(): ParsedHost {
  // 后端 tag / group / note 是自由字符串且默认空串；这里保留前端联合类型，空串
  // 由 importSelectedRows 回退 'dev'，与后端 commit_import（不补默认）的最终值一致
  return { name: '', addr: '', user: '', port: 22, keyRef: '', group: '', tag: '' as ParsedHost['tag'], note: '', duplicate: false };
}

export function parseSshConfig(text: string): ParsedHost[] {
  const out: ParsedHost[] = [];
  // 一行多模式共享后续键值；flush 时按后端同规则决定丢弃 / 兜底
  let patterns: string[] = [];
  let cur: ParsedHost | null = null;
  let inMatch = false;
  const flush = () => {
    const c = cur;
    cur = null;
    if (!c) return;
    // 后端：模式列表为空（裸 `Host` 行）或任一模式含 * / ? 时整组丢弃
    if (!patterns.length || patterns.some((p) => p.includes('*') || p.includes('?'))) { patterns = []; return; }
    const name = patterns[0];
    patterns = [];
    c.name = name;
    if (!c.addr) c.addr = name;
    if (out.some((e) => e.addr === c.addr && e.port === c.port)) c.duplicate = true;
    out.push(c);
  };
  String(text ?? '').split(/\r?\n/).forEach((line) => {
    const t = line.trim();
    if (!t || t.startsWith('#')) return;
    // 与后端 split_once(char::is_whitespace) 等价：无空白行整行是 keyword、value 为空串
    const sp = t.search(/\s/);
    const k = (sp < 0 ? t : t.slice(0, sp)).toLowerCase();
    const v = sp < 0 ? '' : t.slice(sp).trim();
    if (k === 'match') { flush(); inMatch = true; return; }
    if (k === 'host') { flush(); inMatch = false; patterns = v ? v.split(/\s+/) : []; cur = blankHost(); return; }
    if (inMatch || !cur) return;
    if (k === 'hostname') cur.addr = v;
    else if (k === 'user') cur.user = v;
    // 与后端 value.parse::<u16>() 等价：可选正号的纯十进制数字且 ≤ 65535，否则保持 22
    else if (k === 'port') { if (/^\+?\d+$/.test(v) && Number(v) <= 65535) cur.port = Number(v); }
    else if (k === 'identityfile') cur.keyRef = v;
  });
  flush();
  return out;
}

export function sshCommand(host: { user: string; addr: string; port: number }): string {
  return 'ssh ' + (host.port && host.port !== 22 ? '-p ' + host.port + ' ' : '') + host.user + '@' + host.addr;
}
