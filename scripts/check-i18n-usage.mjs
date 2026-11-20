// 校验「代码里用到的 i18n key」是否真的存在于词条中。
//
// 为什么需要：`T('x.y')` 的参数是字符串，TypeScript 抓不到拼错或不存在的 key；
// 缺 key 时界面会直接显示 "ci.localEmpty" 这种原始 key（本项目已出现过一次）。
//
// 用法：node scripts/check-i18n-usage.mjs   （exit 1 = 有缺失）
import fs from 'node:fs';
import path from 'node:path';

const ROOT = path.resolve(import.meta.dirname, '..');
const SRC = path.join(ROOT, 'src');
const LOCALES = path.join(SRC, 'i18n', 'locales');

const walk = (dir) =>
  fs.readdirSync(dir, { withFileTypes: true }).flatMap((e) => {
    const p = path.join(dir, e.name);
    return e.isDirectory() ? walk(p) : [p];
  });

const localeKeys = (lang) => {
  const text = fs.readFileSync(path.join(LOCALES, `${lang}.ts`), 'utf8');
  return new Set([...text.matchAll(/^\s*"([^"]+)":/gm)].map((m) => m[1]));
};

const zh = localeKeys('zh-CN');

// 1) 收集静态用法 T('a.b') / T("a.b")，以及动态前缀 T('a.' + x)
const staticUses = new Map(); // key -> file:line
const dynamicPrefixes = new Map(); // prefix -> file:line
for (const file of walk(SRC)) {
  if (!/\.(ts|tsx)$/.test(file) || file.includes(`${path.sep}locales${path.sep}`)) continue;
  if (/\.test\.(ts|tsx)$/.test(file)) continue; // 测试里的假 key 不算缺失
  const rel = path.relative(ROOT, file).replace(/\\/g, '/');
  const lines = fs.readFileSync(file, 'utf8').split('\n');
  lines.forEach((line, i) => {
    const where = `${rel}:${i + 1}`;
    for (const m of line.matchAll(/\bT\(\s*'([A-Za-z][\w.]*)'\s*[,)]/g)) {
      if (!staticUses.has(m[1])) staticUses.set(m[1], where);
    }
    // 动态前缀：T('ci.' + ...) 或 T(`enc.group${...}`)
    for (const m of line.matchAll(/\bT\(\s*'([A-Za-z][\w.]*)'\s*\+/g)) {
      if (!dynamicPrefixes.has(m[1])) dynamicPrefixes.set(m[1], where);
    }
    for (const m of line.matchAll(/\bT\(\s*`([A-Za-z][\w]*\.)/g)) {
      if (!dynamicPrefixes.has(m[1])) dynamicPrefixes.set(m[1], where);
    }
  });
}

const missing = [];
for (const [key, where] of staticUses) {
  if (zh.has(key)) continue;
  // 动态前缀覆盖（如 T('auth.' + k)）
  if ([...dynamicPrefixes.keys()].some((p) => key.startsWith(p))) continue;
  missing.push({ key, where });
}

console.log(`[i18n-usage] 动态前缀: ${[...dynamicPrefixes.keys()].join(", ") || "无"}`);
console.log(`[i18n-usage] 静态引用 ${staticUses.size} 个 key，动态前缀 ${dynamicPrefixes.size} 组，词条 ${zh.size} 条`);
if (missing.length) {
  console.error(`[i18n-usage] 缺失 ${missing.length} 条：`);
  for (const { key, where } of missing) console.error(`  ✗ ${key}  (${where})`);
  process.exit(1);
}
console.log('[i18n-usage] OK: 代码引用的 key 全部存在');
