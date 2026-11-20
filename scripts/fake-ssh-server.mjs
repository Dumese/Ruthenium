/**
 * 本地「假 SSH 服务器」（仅测试用）——让真机端到端测试在没有第三方服务器时也能
 * 覆盖「SSH 成功路径」：密码认证 + PTY/shell + exec(/proc 指标) + SFTP + direct-tcpip。
 *
 * 为什么需要它：真实 SSH 成功路径需要一个可登录的服务器。本脚本用 ssh2 起一个
 * 只监听 127.0.0.1 的受控服务器，行为对齐真实 SSH 协议的语义（而不是像 mock 那样
 * 绕过 IPC/协议栈）。
 *
 * 用法：
 *   node scripts/fake-ssh-server.mjs            # 打印 FAKE_SSH_READY port=xxxxx
 *   FAKE_SSH_USER / FAKE_SSH_PASSWORD 可覆盖默认值（e2e / e2e-secret）
 *   然后让 scripts/e2e-real-ssh.mjs 连接 127.0.0.1:<port>。
 */
import crypto from 'node:crypto';
import net from 'node:net';
import ssh2 from 'ssh2';

const { Server, utils } = ssh2;
// ssh2 的 sftp 常量在 utils.sftp 下（不是顶层导出）
const { OPEN_MODE, STATUS_CODE } = utils.sftp;
const USER = process.env.FAKE_SSH_USER || 'e2e';
const PASS = process.env.FAKE_SSH_PASSWORD || 'e2e-secret';

// ---------- 内存文件系统（SFTP 用） ----------
/** @type {Map<string, {dir:boolean, data?:Buffer, mode:number, mtime:number}>} */
const FS = new Map();
const now = () => Math.floor(Date.now() / 1000);
function resetFs() {
  FS.clear();
  FS.set('/', { dir: true, mode: 0o040755, mtime: now() });
  FS.set('/root', { dir: true, mode: 0o040755, mtime: now() });
  FS.set('/tmp', { dir: true, mode: 0o041777, mtime: now() });
}
resetFs();
const norm = (p) => {
  let s = String(p || '.').replace(/\\/g, '/');
  if (!s.startsWith('/')) s = s === '.' ? '/root' : '/' + s;
  s = s.replace(/\/+/g, '/');
  if (s.length > 1 && s.endsWith('/')) s = s.slice(0, -1);
  return s;
};
const parent = (p) => (p === '/' ? '/' : p.slice(0, p.lastIndexOf('/')) || '/');
const attrsOf = (p) => {
  const n = FS.get(norm(p));
  if (!n) return null;
  return {
    mode: n.mode, uid: 0, gid: 0, size: n.dir ? 0 : (n.data ? n.data.length : 0),
    atime: n.mtime, mtime: n.mtime,
  };
};

// 供 exec 返回的 /proc & df 文本（与 metrics::sampler 的解析格式一致）
const PROC_TEXT = [
  'cpu  120 0 80 800 40 0 0 0 0 0',
  'cpu0 60 0 40 400 20 0 0 0 0 0',
  'cpu1 60 0 40 400 20 0 0 0 0 0',
  'MemTotal:        2048000 kB',
  'MemFree:          512000 kB',
  'MemAvailable:    1024000 kB',
  'Inter-|   Receive                                                |  Transmit',
  ' face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed',
  '    lo:    1000       10    0    0    0     0          0         0     1000       10    0    0    0     0       0          0',
  '  eth0: 1000000     1000    0    0    0     0          0         0  2000000     2000    0    0    0     0       0          0',
  '   8       0 sda 1000 0 20000 100 200 0 10000 50 0 0 0',
  '0.10 0.20 0.30 1/100 12345',
  'Filesystem     1B-blocks       Used  Available Use% Mounted on',
  '/dev/sda1     10737418240 5368709120 5368709120  50% /',
].join('\n') + '\n';

const keys = utils.generateKeyPairSync('ed25519');
const server = new Server({ hostKeys: [keys.private] }, (client) => {
  client
    .on('authentication', (ctx) => {
      if (ctx.method === 'password' && ctx.username === USER && ctx.password === PASS) return ctx.accept();
      if (ctx.method === 'none') return ctx.reject(['password']);
      return ctx.reject(['password']);
    })
    .on('ready', () => {
      client.on('session', (accept) => {
        const session = accept();
        session.on('pty', (accept) => { if (accept) accept(); });
        session.on('window-change', (accept) => { if (accept) accept(); });

        // 交互式 shell：回显输入；命令 `echo X` 输出 `OUT:X`
        session.on('shell', (accept) => {
          const stream = accept();
          let line = '';
          const prompt = (s) => s.write(`${USER}@fake:~$ `);
          prompt(stream);
          stream.on('data', (buf) => {
            for (const ch of buf.toString('utf8')) {
              if (ch === '\r' || ch === '\n') {
                stream.write('\r\n');
                const cmd = line.trim();
                line = '';
                if (cmd === 'exit') { stream.exit(0); stream.end(); return; }
                const m = /^echo\s+(.*)$/.exec(cmd);
                if (m) stream.write(`OUT:${m[1]}\r\n`);
                else if (cmd) stream.write(`OUT:${cmd}\r\n`);
                prompt(stream);
              } else if (ch === '\u0003') {
                line = '';
                stream.write('^C\r\n');
                prompt(stream);
              } else if (ch >= ' ') {
                line += ch;
                stream.write(ch);
              }
            }
          }).on('end', () => { /* client closed */ });
        });

        // exec：只读指标命令返回伪 /proc 文本（验证 metrics 解析全链路）
        session.on('exec', (accept, reject, info) => {
          const stream = accept();
          const cmd = String(info.command || '');
          if (cmd.includes('/proc/stat')) stream.write(PROC_TEXT);
          else stream.write(`OUT:${cmd}\n`);
          stream.exit(0);
          stream.end();
        });

        // SFTP：内存文件系统（覆盖列目录/上传/下载/删除）
        session.on('sftp', (accept) => {
          const sftp = accept();
          let handleSeq = 0;
          /** @type {Map<number, {kind:'file'|'dir', path:string, data?:Buffer, pos?:number, sent?:boolean, write?:boolean}>} */
          const handles = new Map();
          const mkHandle = (info) => {
            const id = handleSeq++;
            handles.set(id, info);
            const h = Buffer.alloc(4);
            h.writeUInt32BE(id, 0);
            return h;
          };
          const hid = (h) => (Buffer.isBuffer(h) && h.length === 4 ? h.readUInt32BE(0) : -1);

          sftp
            .on('REALPATH', (reqid, p) => {
              const path = norm(p);
              sftp.name(reqid, [{ filename: path, longname: path, attrs: attrsOf(path) || { mode: 0o040755, uid: 0, gid: 0 } }]);
            })
            .on('OPENDIR', (reqid, p) => {
              const path = norm(p);
              const n = FS.get(path);
              if (!n || !n.dir) return sftp.status(reqid, STATUS_CODE.NO_SUCH_FILE);
              sftp.handle(reqid, mkHandle({ kind: 'dir', path, sent: false }));
            })
            .on('READDIR', (reqid, h) => {
              const st = handles.get(hid(h));
              if (!st) return sftp.status(reqid, STATUS_CODE.FAILURE);
              if (st.sent) return sftp.status(reqid, STATUS_CODE.EOF);
              st.sent = true;
              const entries = [...FS.entries()]
                .filter(([p]) => p !== st.path && parent(p) === st.path)
                .map(([p, n]) => ({
                  filename: p.slice(p.lastIndexOf('/') + 1),
                  longname: `${n.dir ? 'd' : '-'}rw-r--r-- 1 root root ${n.dir ? 0 : (n.data ? n.data.length : 0)} ${p}`,
                  attrs: { mode: n.mode, uid: 0, gid: 0, size: n.dir ? 0 : (n.data ? n.data.length : 0), atime: n.mtime, mtime: n.mtime },
                }));
              sftp.name(reqid, entries);
            })
            .on('LSTAT', (reqid, p) => {
              const a = attrsOf(p);
              if (!a) return sftp.status(reqid, STATUS_CODE.NO_SUCH_FILE);
              sftp.attrs(reqid, a);
            })
            .on('STAT', (reqid, p) => {
              const a = attrsOf(p);
              if (!a) return sftp.status(reqid, STATUS_CODE.NO_SUCH_FILE);
              sftp.attrs(reqid, a);
            })
            .on('FSTAT', (reqid, h) => {
              const st = handles.get(hid(h));
              if (!st) return sftp.status(reqid, STATUS_CODE.FAILURE);
              sftp.attrs(reqid, attrsOf(st.path) || { mode: 0o100644, uid: 0, gid: 0, size: 0 });
            })
            .on('OPEN', (reqid, p, flags) => {
              const path = norm(p);
              const write = !!(flags & (OPEN_MODE.WRITE | OPEN_MODE.CREATE | OPEN_MODE.TRUNC));
              const existing = FS.get(path);
              if (!write && (!existing || existing.dir)) return sftp.status(reqid, STATUS_CODE.NO_SUCH_FILE);
              if (write) {
                const buf = existing && !existing.dir && !(flags & OPEN_MODE.TRUNC) ? existing.data : Buffer.alloc(0);
                FS.set(path, { dir: false, data: buf, mode: 0o100644, mtime: now() });
              }
              sftp.handle(reqid, mkHandle({ kind: 'file', path, write, data: FS.get(path).data, pos: 0 }));
            })
            .on('READ', (reqid, h, offset, length) => {
              const st = handles.get(hid(h));
              if (!st) return sftp.status(reqid, STATUS_CODE.FAILURE);
              const file = FS.get(st.path);
              const data = (file && file.data) || Buffer.alloc(0);
              if (offset >= data.length) return sftp.status(reqid, STATUS_CODE.EOF);
              sftp.data(reqid, data.subarray(offset, Math.min(data.length, offset + length)));
            })
            .on('WRITE', (reqid, h, offset, data) => {
              const st = handles.get(hid(h));
              if (!st) return sftp.status(reqid, STATUS_CODE.FAILURE);
              const file = FS.get(st.path) || { dir: false, data: Buffer.alloc(0), mode: 0o100644, mtime: now() };
              const need = offset + data.length;
              let buf = file.data || Buffer.alloc(0);
              if (buf.length < need) {
                const next = Buffer.alloc(need);
                buf.copy(next, 0);
                buf = next;
              }
              data.copy(buf, offset);
              file.data = buf;
              file.mtime = now();
              FS.set(st.path, file);
              sftp.status(reqid, STATUS_CODE.OK);
            })
            .on('CLOSE', (reqid, h) => {
              handles.delete(hid(h));
              sftp.status(reqid, STATUS_CODE.OK);
            })
            .on('REMOVE', (reqid, p) => {
              const path = norm(p);
              const n = FS.get(path);
              if (!n || n.dir) return sftp.status(reqid, STATUS_CODE.NO_SUCH_FILE);
              FS.delete(path);
              sftp.status(reqid, STATUS_CODE.OK);
            })
            .on('MKDIR', (reqid, p) => {
              const path = norm(p);
              if (FS.has(path)) return sftp.status(reqid, STATUS_CODE.FAILURE);
              FS.set(path, { dir: true, mode: 0o040755, mtime: now() });
              sftp.status(reqid, STATUS_CODE.OK);
            })
            .on('RENAME', (reqid, from, to) => {
              const a = norm(from), b = norm(to);
              const n = FS.get(a);
              if (!n) return sftp.status(reqid, STATUS_CODE.NO_SUCH_FILE);
              FS.delete(a);
              FS.set(b, n);
              sftp.status(reqid, STATUS_CODE.OK);
            })
            .on('SETSTAT', (reqid) => sftp.status(reqid, STATUS_CODE.OK))
            .on('FSETSTAT', (reqid) => sftp.status(reqid, STATUS_CODE.OK))
            .on('EXTENDED', (reqid) => sftp.status(reqid, STATUS_CODE.OP_UNSUPPORTED));
        });
      });
    })
    .on('tcpip', (accept, reject, info) => {
      // -L 转发的远端目标：真的建立 TCP 连接（测试里指向本假服务器自身，读到 SSH 版本串）
      const stream = accept();
      const destHost = info.destIP && info.destIP !== 'localhost' ? info.destIP : '127.0.0.1';
      const sock = net.connect({ host: destHost === '0.0.0.0' ? '127.0.0.1' : destHost, port: info.destPort });
      sock.on('connect', () => {
        sock.pipe(stream).pipe(sock);
      });
      sock.on('error', () => { try { stream.close(); } catch { /* ignore */ } });
    })
    .on('error', () => { /* 客户端异常不影响后续连接 */ });
});

server.listen(Number(process.env.FAKE_SSH_PORT || 0), '127.0.0.1', function () {
  console.log(`FAKE_SSH_READY port=${this.address().port} user=${USER}`);
  // 供测试读取：把密码写到 stdout 第二行（本地测试专用，不落盘）
  console.log(`FAKE_SSH_PASSWORD=${PASS}`);
});

process.on('SIGTERM', () => { server.close(); process.exit(0); });
process.on('SIGINT', () => { server.close(); process.exit(0); });
