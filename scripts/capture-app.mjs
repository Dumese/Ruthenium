// 抓取真实应用（WebView2 CDP）截图：node scripts/capture-app.mjs [输出路径]
import os from 'node:os';
import path from 'node:path';
const APP_CDP = process.env.APP_CDP || 'http://127.0.0.1:9444';
// 默认输出到系统临时目录，避免换机后因硬编码路径而失败
const out = process.argv[2] || path.join(os.tmpdir(), 'real_app.png');

async function connect() {
  for (let i = 0; i < 80; i++) {
    try {
      const r = await fetch(APP_CDP + '/json/version');
      if (r.ok) return (await r.json()).webSocketDebuggerUrl;
    } catch {
      /* retry */
    }
    await new Promise((r) => setTimeout(r, 250));
  }
  throw new Error('CDP not ready（应用是否带 --remote-debugging-port 启动？）');
}

function client(u) {
  const ws = new WebSocket(u);
  let id = 0;
  const pending = new Map();
  ws.addEventListener('message', (ev) => {
    const m = JSON.parse(ev.data);
    if (m.id && pending.has(m.id)) {
      const { resolve, reject } = pending.get(m.id);
      pending.delete(m.id);
      m.error ? reject(new Error(JSON.stringify(m.error))) : resolve(m.result);
    }
  });
  const ready = new Promise((res, rej) => {
    ws.addEventListener('open', res);
    ws.addEventListener('error', rej);
  });
  const send = (method, params = {}, sessionId) =>
    new Promise((resolve, reject) => {
      const mid = ++id;
      pending.set(mid, { resolve, reject });
      ws.send(JSON.stringify({ id: mid, method, params, sessionId }));
    });
  return { ws, ready, send };
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const main = async () => {
  const c = client(await connect());
  await c.ready;
  const { targetInfos } = await c.send('Target.getTargets');
  const page = targetInfos.find((t) => t.type === 'page');
  const { sessionId } = await c.send('Target.attachToTarget', { targetId: page.targetId, flatten: true });
  await c.send('Page.enable', {}, sessionId);
  await sleep(500);
  const { data } = await c.send('Page.captureScreenshot', { format: 'png' }, sessionId);
  const fs = await import('node:fs');
  fs.writeFileSync(out, Buffer.from(data, 'base64'));
  console.log('screenshot ->', out);
  c.ws.close();
  process.exit(0);
};

main().catch((e) => {
  console.error('CAPTURE_FAILED', e?.message || e);
  process.exit(1);
});
