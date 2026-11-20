// 构建前校验：四份 locale key 差集必须为空
import fs from 'node:fs';
import path from 'node:path';
import url from 'node:url';

const dir = path.join(path.dirname(url.fileURLToPath(import.meta.url)), '..', 'src', 'i18n', 'locales');
const langs = ['zh-CN', 'zh-TW', 'en', 'ja'];
const load = (l) => fs.readFileSync(path.join(dir, l + '.ts'), 'utf8');
const keys = (s) => Array.from(s.matchAll(/^\s*"([^"]+)":/gm)).map((m) => m[1]).sort();

const base = keys(load('zh-CN'));
let fail = 0;
for (const l of langs) {
  const ks = keys(load(l));
  const missing = base.filter((k) => !ks.includes(k));
  const extra = ks.filter((k) => !base.includes(k));
  const dup = ks.filter((k, i) => i > 0 && ks[i - 1] === k);
  if (missing.length || extra.length || dup.length) {
    fail = 1;
    console.error(`[check-i18n] ${l}: missing=${missing.length} extra=${extra.length} dup=${dup.length}`);
    console.error('  missing:', missing.slice(0, 12));
    console.error('  extra:', extra.slice(0, 12));
    console.error('  dup:', dup.slice(0, 12));
  }
}
if (fail) process.exit(1);
console.log(`[check-i18n] OK: ${langs.join(' / ')} 各 ${base.length} 条 key 完全对齐`);
