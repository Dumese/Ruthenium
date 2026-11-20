// UI 测试一键编排：e2e 构建 → vite preview → headless 浏览器 → 依次执行脚本 → 清理。
//
// 为什么需要这一层（各脚本单跑的三个坑）：
// 1. 构建模式：脚本驱动的配对弹窗 / SFTP / 指纹流程依赖 `./mock`。生产构建（`npm run
//    build`）会把 mock 换成 noop 桩，直接对 dist 跑脚本会假失败。必须先
//    `vite build --mode e2e` 产出保留 mock 的 bundle。本脚本强制 e2e 构建，不依赖调用方记得。
// 2. 进程生命周期：preview 与浏览器必须与脚本同进程树启动。编排器把三者放在同一次
//    调用里拉起，脚本结束后统一收割（杀进程树），不留孤儿进程占用 4173 / 9333。
// 3. 浏览器发现：优先 Chrome / Edge 的标准安装路径，找不到时按 CHROME_BIN 环境变量兜底。
//
// 用法：
//   npm run test:ui                 # 跑全部浏览器脚本
//   npm run test:ui -- ui-regress   # 只跑指定脚本（不带 .mjs 后缀也可）
//
// 环境变量：
//   UI_BROWSER_BIN  指定浏览器可执行文件路径（默认自动探测 Chrome → Edge）
//   UI_SKIP_BUILD   设为 1 跳过 e2e 构建（要求 dist 已是 e2e 产物）
//   UI_KEEP_ALIVE   设为 1 时测试完成后不杀 preview（调试用；浏览器仍会关闭）
import { spawn, spawnSync } from 'node:child_process';
import { existsSync, mkdirSync, readFileSync } from 'node:fs';
import { createServer } from 'node:net';
import os from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const PREVIEW_PORT = 4173;
const CDP_PORT = 9333;
const PREVIEW_URL = `http://localhost:${PREVIEW_PORT}/`;

/** 依次执行的浏览器脚本（与 README「无人值守 UI 检查」表格一致） */
const SCRIPTS = [
  'ui-regress.mjs',
  'responsive-matrix.mjs',
  'ui-sweep.mjs',
  'stress-sweep.mjs',
  'i18n-residue.mjs',
  'find-narrow.mjs',
  'e2e-ui.mjs',
];

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const log = (msg) => console.log(`[test:ui] ${msg}`);

/** 端口是否有监听者（复用前探测） */
function portBusy(port) {
  return new Promise((res) => {
    const s = createServer();
    s.once('error', () => res(true));
    s.once('listening', () => s.close(() => res(false)));
    s.listen(port, '127.0.0.1');
  });
}

/** 按 Chrome → Edge 顺序探测浏览器；CHROME_BIN 优先 */
function findBrowser() {
  const candidates = [
    process.env.UI_BROWSER_BIN,
    'C:/Program Files/Google/Chrome/Application/chrome.exe',
    'C:/Program Files (x86)/Google/Chrome/Application/chrome.exe',
    'C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe',
    'C:/Program Files/Microsoft/Edge/Application/msedge.exe',
  ].filter(Boolean);
  for (const p of candidates) {
    if (existsSync(p)) return p;
  }
  return null;
}

/** e2e 构建（仅此模式保留 mock）。跳过条件：UI_SKIP_BUILD=1 */
function buildE2e() {
  if (process.env.UI_SKIP_BUILD === '1') {
    log('UI_SKIP_BUILD=1，跳过 e2e 构建（假定 dist 已是 e2e 产物）');
    return;
  }
  log('e2e 构建（tsc --noEmit + vite build --mode e2e）…');
  const r = spawnSync('npx', ['vite', 'build', '--mode', 'e2e'], {
    cwd: ROOT,
    stdio: 'inherit',
    shell: process.platform === 'win32',
  });
  if (r.status !== 0) {
    console.error('[test:ui] e2e 构建失败，中止（未启动 preview / 浏览器）');
    process.exit(r.status ?? 1);
  }
}

/** 杀整棵进程树（Windows: taskkill /T；其它: 负 SIGTERM） */
function killTree(child) {
  if (!child || child.exitCode !== null || child.signalCode !== null) return;
  if (process.platform === 'win32') {
    spawnSync('taskkill', ['/pid', String(child.pid), '/T', '/F'], { stdio: 'ignore' });
  } else {
    try {
      process.kill(-child.pid, 'SIGTERM');
    } catch {
      child.kill('SIGTERM');
    }
  }
}

async function waitFor(url, deadlineMs, what) {
  const start = Date.now();
  while (Date.now() - start < deadlineMs) {
    const ok = await fetch(url)
      .then((r) => r.ok)
      .catch(() => false);
    if (ok) return true;
    await sleep(300);
  }
  throw new Error(`${what} 未在 ${Math.round(deadlineMs / 1000)}s 内就绪：${url}`);
}

async function main() {
  const only = process.argv.slice(2).map((s) => s.replace(/\.mjs$/, ''));
  const scripts = only.length
    ? SCRIPTS.filter((s) => only.some((o) => s.startsWith(o)))
    : SCRIPTS;
  if (!scripts.length) {
    console.error(`[test:ui] 没有匹配的脚本：${only.join(', ')}（可选：${SCRIPTS.join(', ')}）`);
    process.exit(1);
  }

  const browserBin = findBrowser();
  if (!browserBin) {
    console.error('[test:ui] 未找到 Chrome / Edge；可用 UI_BROWSER_BIN 指定浏览器路径');
    process.exit(1);
  }
  log(`浏览器：${browserBin}`);

  // preview 已被占用：多半是上次运行残留。拒绝复用——dist 可能是另一个模式的产物。
  if (await portBusy(PREVIEW_PORT)) {
    console.error(
      `[test:ui] 端口 ${PREVIEW_PORT} 已被占用。请先停掉旧的 vite preview` +
        `（或确认其服务的是 e2e 产物后设 UI_SKIP_BUILD=1 自行编排）。`
    );
    process.exit(1);
  }
  if (await portBusy(CDP_PORT)) {
    console.error(`[test:ui] 端口 ${CDP_PORT} 已被占用。请先关掉旧的 headless 浏览器。`);
    process.exit(1);
  }

  buildE2e();

  const preview = spawn('npx', ['vite', 'preview', '--port', String(PREVIEW_PORT), '--strictPort'], {
    cwd: ROOT,
    stdio: 'ignore',
    shell: process.platform === 'win32',
  });
  const profileDir = join(os.tmpdir(), `ruthenium-ui-test-${Date.now()}`);
  mkdirSync(profileDir, { recursive: true });
  const browser = spawn(
    browserBin,
    [
      '--headless=new',
      `--remote-debugging-address=127.0.0.1`,
      `--remote-debugging-port=${CDP_PORT}`,
      `--user-data-dir=${profileDir}`,
      '--no-first-run',
      '--disable-gpu',
      '--no-proxy-server',
      'about:blank',
    ],
    { stdio: 'ignore' }
  );

  const results = [];
  let hardFail = false;
  try {
    await waitFor(PREVIEW_URL, 30_000, 'vite preview');
    await waitFor(`http://127.0.0.1:${CDP_PORT}/json/version`, 30_000, '浏览器 CDP');
    log(`preview(${PREVIEW_PORT}) + CDP(${CDP_PORT}) 就绪，开始执行 ${scripts.length} 个脚本`);

    for (const s of scripts) {
      log(`▶ ${s}`);
      const r = spawnSync(process.execPath, [join('scripts', s)], {
        cwd: ROOT,
        stdio: 'inherit',
        env: process.env,
        timeout: 10 * 60_000,
      });
      const ok = r.status === 0;
      results.push({ script: s, ok, code: r.status });
      if (!ok) {
        hardFail = true;
        log(`✗ ${s} 失败（exit=${r.status}${r.signal ? `, signal=${r.signal}` : ''}）`);
      }
    }
  } catch (e) {
    console.error(`[test:ui] ${e.message}`);
    hardFail = true;
  } finally {
    killTree(browser);
    killTree(preview);
    if (process.env.UI_KEEP_ALIVE === '1' && !hardFail) {
      log('UI_KEEP_ALIVE=1：preview 保留（请手动清理）');
    }
  }

  console.log('\n===== test:ui 汇总 =====');
  for (const r of results) console.log(`${r.ok ? 'PASS' : 'FAIL'}  ${r.script}`);
  const failed = results.filter((r) => !r.ok).length;
  console.log(`\n${results.length - failed}/${results.length} 通过`);
  process.exit(hardFail ? 1 : 0);
}

main().catch((e) => {
  console.error('[test:ui] 未捕获异常：', e);
  process.exit(1);
});
