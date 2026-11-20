//! 本地终端会话（Ctrl+T）：portable-pty + ConPTY 驱动真实本地 shell，
//! 向上复用 SSH 会话的 `SessionHandle` / `term://data` / `term://exit` /
//! `conn://state` 管线。
//!
//! 与 SSH 会话的差异只在传输层：这里没有 russh handle，也不需要隧道/SFTP/
//! 远端指标；因此使用独立的 `LocalManager` 会话表，避免把 `russh` 塞进
//! `LiveSession` 之外的本地对象。
use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use portable_pty::{native_pty_system, Child, ChildKiller, CommandBuilder, MasterPty, PtySize};
use tauri::AppHandle;
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;

use crate::encoding::{decode_chunk, SessionEncoding};
use crate::error::{AppError, AppResult};
use crate::events::{self, ConnStatePayload, TermDataPayload, TermExitPayload};
use crate::models::{Session, SessionHandle, SessionKind};
use crate::session_log::SessionLogger;

/// 本地会话没有 host：IPC 的 `hostId` 固定为空字符串。
pub const LOCAL_HOST_ID: &str = "";

/// 本地 PTY 单次读取块大小：一次读取一屏量级的数据，避免频繁小包。
const PTY_READ_BUF_SIZE: usize = 8 * 1024;
/// 关闭 / 失败清理时等待子进程退出的上限（命令返回路径上不能卡死）。
const CHILD_EXIT_WAIT: Duration = Duration::from_secs(2);
/// 等待子进程退出时的轮询间隔。
const CHILD_EXIT_POLL: Duration = Duration::from_millis(20);

/// reader 的事件出口。生产环境写 Tauri 事件；单测用 channel sink 收集，
/// 避免为了测试构造 AppHandle。
/// `Sync`：以 `Arc<dyn LocalSink>` 在 reader / watcher 线程间共享。
pub trait LocalSink: Send + Sync + 'static {
    /// 上报一次解码后的终端输出。
    fn data(&self, session_id: &str, data: Vec<u8>);
    /// `reason` 直接进 `conn://state.reason`。本地 PTY 的断线只有 `lost`；
    /// 用户主动关闭由命令层 emit `closed_by_user`，reader 不补发。
    fn disconnected(&self, session_id: &str, reason: &str);
    /// 上报 PTY 退出（`term://exit`），`reason` 为退出原因文案。
    fn exit(&self, session_id: &str, reason: &str);
}

/// 生产 sink：与 SSH reader 完全一致的事件形状。
pub struct AppEventSink {
    app: AppHandle,
    /// 终端数据通道。`Some` 时 `data` 走 Channel（官方流式 IPC，按调用方隔离），
    /// `None` 回退全局 `term://data` 事件（与 SSH 侧 `deliver_term_data` 同一策略）。
    data_channel: Option<tauri::ipc::Channel<TermDataPayload>>,
}

impl AppEventSink {
    /// 用当前应用句柄构造事件出口（数据走全局 `term://data` 事件）。
    pub fn new(app: AppHandle) -> Self {
        Self {
            app,
            data_channel: None,
        }
    }

    /// 带终端数据通道构造（数据优先走 Channel）。
    pub fn with_data_channel(
        app: AppHandle,
        data_channel: Option<tauri::ipc::Channel<TermDataPayload>>,
    ) -> Self {
        Self { app, data_channel }
    }
}

impl LocalSink for AppEventSink {
    fn data(&self, session_id: &str, data: Vec<u8>) {
        let payload = TermDataPayload {
            session_id: session_id.to_string(),
            // IPC 边界统一 base64；LocalSink 内部仍按 Vec<u8> 传递，测试 sink 不受影响。
            data: events::b64(&data),
        };
        if let Some(ch) = &self.data_channel {
            if let Err(e) = ch.send(payload) {
                applog!("[local] channel 发送终端数据失败（可能 webview 已重载）: {e}");
            }
            return;
        }
        events::emit(&self.app, events::TERM_DATA, payload);
    }

    fn disconnected(&self, session_id: &str, reason: &str) {
        events::emit(
            &self.app,
            events::CONN_STATE,
            ConnStatePayload {
                session_id: session_id.to_string(),
                state: "disconnected".to_string(),
                latency_ms: None,
                reason: Some(reason.to_string()),
            },
        );
    }

    fn exit(&self, session_id: &str, reason: &str) {
        events::emit(
            &self.app,
            events::TERM_EXIT,
            TermExitPayload {
                session_id: session_id.to_string(),
                reason: reason.to_string(),
            },
        );
    }
}

/// 退出原因：由最先观察到退出的一方写入（CAS，首写生效），
/// watcher 线程据此决定事件文案（lost / closed by user）。
#[derive(Debug, Clone, Copy, PartialEq)]
enum ExitCause {
    /// 尚未退出
    None,
    /// 用户主动关闭（close 命令）
    ByUser,
    /// shell 自行退出（EOF / 正常返回）
    Natural,
    /// PTY 读取错误（管道破裂等）
    PtyBreak,
}

fn cause_u8(c: ExitCause) -> u8 {
    match c {
        ExitCause::None => 0,
        ExitCause::ByUser => 1,
        ExitCause::Natural => 2,
        ExitCause::PtyBreak => 3,
    }
}

/// 首写生效：只有当前值仍是 `from` 时才写入 `to`。
fn cas_cause(slot: &AtomicU8, from: ExitCause, to: ExitCause) -> bool {
    slot.compare_exchange(
        cause_u8(from),
        cause_u8(to),
        Ordering::SeqCst,
        Ordering::SeqCst,
    )
    .is_ok()
}

/// load 当前原因。
fn load_cause(slot: &AtomicU8) -> ExitCause {
    match slot.load(Ordering::SeqCst) {
        1 => ExitCause::ByUser,
        2 => ExitCause::Natural,
        3 => ExitCause::PtyBreak,
        _ => ExitCause::None,
    }
}

/// 一个本地会话：SessionHandle（IPC 共用字段）+ PTY master（resize/writer）
/// + 子进程的 kill 句柄（close 时用；`wait` 由 watcher 线程独占，见 spawn_watcher）。
struct LocalSession {
    inner: SessionHandle,
    connected: Arc<AtomicBool>,
    master: Box<dyn MasterPty + Send>,
    killer: Box<dyn ChildKiller + Send>,
    /// 退出原因槽（与 watcher / reader 共享）：close 时标记 ByUser。
    cause: Arc<AtomicU8>,
}

/// 本地会话表。SSH 会话表在 `SshManager`，两者由 commands 层做双查。
///
/// 表放进 `Arc`——reader 线程（shell 自然退出 / PTY 断开）持有克隆，
/// 退出时把条目摘掉，否则已死会话永远留在表里（句柄泄漏 + list_sessions 出僵尸）。
#[derive(Default)]
pub struct LocalManager {
    sessions: Arc<Mutex<HashMap<String, LocalSession>>>,
}

impl LocalManager {
    /// 创建空的本地会话表。
    pub fn new() -> Self {
        Self::default()
    }

    /// 创建本地 PTY + 交互式 shell，并启动 reader/writer 线程。
    /// 失败时保证不留下存活子进程，也**不产生会话日志文件**
    /// （`make_logger` 只在 PTY/reader/writer 全部就绪后才调用）。
    ///
    /// 日志：PTY reader 在**解码之前** tee 原始字节到会话日志。
    /// `init_command`：PTY 就绪后写入 master 的一行命令（空串不发；
    /// 本地会话没有宿主字段，生产入口传 None，机制与 SSH 保持一致）。
    pub async fn connect_with_sink(
        &self,
        cols: u16,
        rows: u16,
        sink: Box<dyn LocalSink>,
        make_logger: Option<Box<dyn FnOnce() -> Option<SessionLogger> + Send>>,
        init_command: Option<String>,
    ) -> AppResult<String> {
        let session_id = uuid::Uuid::new_v4().to_string();
        let cols = cols.max(1);
        let rows = rows.max(1);

        let (master, child) = open_local_pty(cols, rows)?;
        let reader = match master.try_clone_reader() {
            Ok(r) => r,
            Err(e) => {
                kill_and_wait(child);
                return Err(local_error(format!("克隆 PTY reader 失败: {e}")));
            }
        };
        let pty_writer = match master.take_writer() {
            Ok(w) => w,
            Err(e) => {
                kill_and_wait(child);
                return Err(local_error(format!("获取 PTY writer 失败: {e}")));
            }
        };

        let (inner, writer_rx) = crate::ssh::make_session_handle(
            session_id.clone(),
            LOCAL_HOST_ID.to_string(),
            SessionKind::Local,
        );
        let connected = Arc::new(AtomicBool::new(true));
        let cancel = inner.cancel.clone();
        let cause = Arc::new(AtomicU8::new(cause_u8(ExitCause::None)));
        let sink: Arc<dyn LocalSink> = sink.into();
        // Child 的 wait 由 watcher 独占；kill 句柄克隆三份：reader（EOF/错误时唤醒
        // watcher）、会话表（close 用）、init 失败路径。
        let killer_for_reader = child.clone_killer();
        let killer_for_close = child.clone_killer();
        let killer_for_failure = child.clone_killer();

        // writer 先启动：随后 reader 启动失败时，drop inner.writer 会让
        // writer 泵自行退出。
        if let Err(e) = spawn_writer(session_id.clone(), writer_rx, pty_writer, cancel.clone()) {
            kill_and_wait(child);
            return Err(local_error(format!("启动本地终端 writer 失败: {e}")));
        }

        // 日志文件在 PTY + reader/writer 就绪后才创建（失败不留空文件）。
        let logger = make_logger.and_then(|make| make());
        let log_path = logger.as_ref().map(|l| l.path().to_path_buf());
        if let Err(e) = spawn_reader(LocalSpawnArgs {
            session_id: session_id.clone(),
            reader,
            writer: inner.writer.clone(),
            encoding: inner.encoding.clone(),
            cancel: cancel.clone(),
            sink: Arc::clone(&sink),
            logger,
            killer: killer_for_reader,
            cause: Arc::clone(&cause),
        }) {
            // reader 线程没起来：刚建的日志文件（若有）一并清掉；会话尚未入表。
            if let Some(p) = log_path {
                let _ = std::fs::remove_file(p);
            }
            kill_and_wait(child);
            return Err(local_error(format!("启动本地终端 reader 失败: {e}")));
        }

        // watcher 独占 Child::wait：Windows ConPTY 下 shell 自然退出时
        // master 的阻塞 read **不会**返回 EOF，reader 感知不到退出——只有 wait 能。
        // watcher 在 shell 退出后统一负责事件上报与会话摘除（close() 的用户关闭除外）。
        spawn_watcher(WatcherArgs {
            session_id: session_id.clone(),
            child,
            cancel: cancel.clone(),
            connected: connected.clone(),
            cause: Arc::clone(&cause),
            sink: Arc::clone(&sink),
            sessions: Arc::clone(&self.sessions),
        });

        // PTY 已建立，返回前写入初始化命令（与 SSH 分支同一时机）。
        if let Some(cmd) = init_command {
            if !cmd.is_empty() {
                let mut bytes = cmd.into_bytes();
                bytes.push(b'\n');
                if inner.writer.send(bytes).await.is_err() {
                    // watcher 已在跑：标记用户关闭并 kill，watcher 发现表里无条目后静默退出
                    cas_cause(&cause, ExitCause::None, ExitCause::ByUser);
                    cancel.cancel();
                    let mut killer = killer_for_failure;
                    let _ = killer.kill();
                    return Err(local_error("写入初始化命令失败：会话已关闭".to_string()));
                }
            }
        }

        self.sessions.lock().await.insert(
            session_id.clone(),
            LocalSession {
                inner,
                connected,
                master,
                killer: killer_for_close,
                cause: Arc::clone(&cause),
            },
        );

        Ok(session_id)
    }

    /// 会话是否仍在本地会话表中（不判断 PTY 是否还活着）。
    pub async fn has(&self, session_id: &str) -> bool {
        self.sessions.lock().await.contains_key(session_id)
    }

    /// 写终端输入：只取 sender，锁不跨越 await。
    pub async fn write(&self, session_id: &str, data: Vec<u8>) -> AppResult<()> {
        let writer = {
            self.sessions
                .lock()
                .await
                .get(session_id)
                .map(|s| s.inner.writer.clone())
        }
        .ok_or_else(|| AppError::ENotFound(format!("会话 {session_id} 不存在")))?;

        writer
            .send(data)
            .await
            .map_err(|_| AppError::ESshChannel("本地终端写入失败：会话已关闭".to_string()))
    }

    /// 取会话级编码状态，供 `set_session_encoding` 整体替换 Decoder。
    pub async fn encoding(&self, session_id: &str) -> AppResult<Arc<Mutex<SessionEncoding>>> {
        self.sessions
            .lock()
            .await
            .get(session_id)
            .map(|s| s.inner.encoding.clone())
            .ok_or_else(|| AppError::ENotFound(format!("会话 {session_id} 不存在")))
    }

    /// 调整 PTY 窗口尺寸；0 行列按 1 处理，避免把非法尺寸传给 ConPTY。
    pub async fn resize(&self, session_id: &str, cols: u16, rows: u16) -> AppResult<()> {
        let sessions = self.sessions.lock().await;
        let session = sessions
            .get(session_id)
            .ok_or_else(|| AppError::ENotFound(format!("会话 {session_id} 不存在")))?;
        session
            .master
            .resize(PtySize {
                rows: rows.max(1),
                cols: cols.max(1),
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| AppError::ESshChannel(format!("本地终端窗口变更失败: {e}")))
    }

    /// 关闭本地会话：标记 ByUser → kill 子进程 → 释放 PTY master。
    /// （ConPTY 在 master 释放时关闭伪控制台，控制台内其余客户端进程随之终止。）
    /// 子进程的 wait 由 watcher 线程独占：kill 后 watcher 苏醒，看到表里已无本条
    /// 会话即静默退出（事件由命令层的 closed_by_user 上报，watcher 不补发 lost）。
    /// 从表中移除后再次关闭返回 E_NOT_FOUND，与前端清理契约一致。
    pub async fn close(&self, session_id: &str) -> AppResult<()> {
        let session = self
            .sessions
            .lock()
            .await
            .remove(session_id)
            .ok_or_else(|| AppError::ENotFound(format!("会话 {session_id} 不存在")))?;

        let LocalSession {
            inner,
            connected,
            master,
            mut killer,
            cause,
        } = session;

        cas_cause(&cause, ExitCause::None, ExitCause::ByUser);
        inner.cancel.cancel();
        connected.store(false, Ordering::Relaxed);
        let _ = killer.kill();
        drop(master); // 解除 reader 的阻塞读（破裂管道 / EOF）
        drop(inner);
        Ok(())
    }

    /// list_sessions 的本地部分：hostId 为空，kind 为 local。
    pub async fn list(&self) -> Vec<Session> {
        let snapshots: Vec<(String, bool, i64, Arc<Mutex<SessionEncoding>>)> = {
            let sessions = self.sessions.lock().await;
            sessions
                .values()
                .map(|s| {
                    (
                        s.inner.id.clone(),
                        s.connected.load(Ordering::Relaxed),
                        s.inner.started_at,
                        s.inner.encoding.clone(),
                    )
                })
                .collect()
        };

        let mut out = Vec::with_capacity(snapshots.len());
        for (id, connected, started_at, encoding) in snapshots {
            let encoding = encoding.lock().await.name.clone();
            out.push(Session {
                id,
                host_id: LOCAL_HOST_ID.to_string(),
                kind: SessionKind::Local,
                connected,
                started_at,
                encoding,
            });
        }
        out
    }
}

fn local_error(detail: String) -> AppError {
    AppError::ESshChannel(format!("本地终端: {detail}"))
}

/// 本地终端（Ctrl+T）的 shell：优先用户默认（COMSPEC），再退回 Windows PowerShell。
fn shell_candidates() -> Vec<OsString> {
    #[cfg(windows)]
    {
        let mut out = Vec::new();
        if let Some(comspec) = std::env::var_os("COMSPEC") {
            // 某些环境会带引号，创建命令行前先去掉。
            let text = comspec.to_string_lossy();
            let trimmed = text.trim().trim_matches('"');
            if !trimmed.is_empty() {
                out.push(OsString::from(trimmed));
            }
        }
        // cmd.exe 不可用时的回退；仍交给 portable-pty 在 PATH 中查找。
        out.push(OsString::from("powershell.exe"));
        out
    }

    #[cfg(unix)]
    {
        let mut out = Vec::new();
        if let Some(shell) = std::env::var_os("SHELL") {
            if !shell.is_empty() {
                out.push(shell);
            }
        }
        out.push(OsString::from("/bin/sh"));
        out
    }
}

/// 打开 PTY 并 spawn shell。Windows 上 portable-pty 自动走 ConPTY。
/// 候选 shell 依次尝试；全部失败时返回 E_SSH_CHANNEL（不 panic）。
///
/// 对等终端控制复用同一实现（服务端被控端也需要一模一样的 ConPTY 语义）。
pub(crate) fn open_local_pty(
    cols: u16,
    rows: u16,
) -> AppResult<(Box<dyn MasterPty + Send>, Box<dyn Child + Send + Sync>)> {
    open_pty_with(cols, rows, shell_candidates())
}

/// 对端控制用的 shell 顺序：pwsh → powershell → cmd。
///
/// 控制会话优先 PowerShell 是有意的：脚本能力一致、`[Console]::WindowWidth`
/// 之类可观测窗口尺寸（cmd 下无法验证 resize）。本地终端仍尊重用户默认（COMSPEC）。
pub(crate) fn control_shell_candidates() -> Vec<OsString> {
    #[cfg(windows)]
    {
        let mut out = vec![OsString::from("pwsh.exe"), OsString::from("powershell.exe")];
        if let Some(comspec) = std::env::var_os("COMSPEC") {
            let text = comspec.to_string_lossy();
            let trimmed = text.trim().trim_matches('"');
            if !trimmed.is_empty() {
                out.push(OsString::from(trimmed));
            }
        }
        out
    }
    #[cfg(not(windows))]
    {
        shell_candidates()
    }
}

/// 用指定候选列表打开 PTY（本地终端与对端控制共用的实际实现）。
pub(crate) fn open_pty_with(
    cols: u16,
    rows: u16,
    candidates: Vec<OsString>,
) -> AppResult<(Box<dyn MasterPty + Send>, Box<dyn Child + Send + Sync>)> {
    let size = PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    };
    let mut failures = Vec::new();

    for program in candidates {
        let pty_system = native_pty_system();
        let pair = match pty_system.openpty(size) {
            Ok(pair) => pair,
            Err(e) => {
                failures.push(format!("openpty: {e}"));
                break;
            }
        };

        let cmd = CommandBuilder::new(&program);
        match pair.slave.spawn_command(cmd) {
            Ok(child) => {
                // slave 必须在 spawn 后释放，否则 Windows 上会多持有 PTY 句柄。
                drop(pair.slave);
                return Ok((pair.master, child));
            }
            Err(e) => {
                applog!(
                    "[local] shell {} 启动失败，尝试下一个候选: {e}",
                    program.to_string_lossy()
                );
                failures.push(format!("{}: {e}", program.to_string_lossy()));
                drop(pair);
            }
        }
    }

    Err(local_error(format!(
        "无法启动本地 shell（{}）",
        if failures.is_empty() {
            "无可用的 shell".to_string()
        } else {
            failures.join("; ")
        }
    )))
}

/// `spawn_reader` 的启动参数：把会话上下文与 reader/writer 句柄收拢成一个结构体，
/// 避免 8 个位置参数（clippy::too_many_arguments）。
struct LocalSpawnArgs {
    session_id: String,
    reader: Box<dyn Read + Send>,
    writer: mpsc::Sender<Vec<u8>>,
    encoding: Arc<Mutex<SessionEncoding>>,
    cancel: CancellationToken,
    /// 数据出口（Arc 与 watcher 共享：reader 只用 data，事件由 watcher 发）。
    sink: Arc<dyn LocalSink>,
    logger: Option<SessionLogger>,
    /// EOF/读取错误时 kill 子进程，唤醒 watcher 完成事件上报与摘表。
    killer: Box<dyn ChildKiller + Send>,
    /// 退出原因槽（与 watcher / close 共享）。
    cause: Arc<AtomicU8>,
}

/// 启动阻塞式 PTY reader 线程；`Ok(())` 仅表示线程创建成功。
///
/// 职责边界：reader 只负责**数据泵**（读字节 → 解码 → sink.data）。
/// 退出事件 / 摘表 / 断线级联统一由 watcher（独占 `Child::wait`）负责——
/// Windows ConPTY 下 shell 自然退出时 master 的阻塞读**不会**返回 EOF，
/// reader 感知不到退出；reader 的 EOF/错误路径只负责 kill 子进程唤醒 watcher。
fn spawn_reader(args: LocalSpawnArgs) -> std::io::Result<()> {
    let LocalSpawnArgs {
        session_id,
        mut reader,
        writer,
        encoding,
        cancel,
        sink,
        logger,
        mut killer,
        cause,
    } = args;
    std::thread::Builder::new()
        .name(format!("ruthenium-pty-reader-{session_id}"))
        .spawn(move || {
            let mut buf = vec![0u8; PTY_READ_BUF_SIZE];
            // Windows ConPTY 在 PSEUDOCONSOLE_INHERIT_CURSOR 下启动时会先向
            // 终端发 `ESC[6n`（查询光标位置），并等待应答后才把 shell 挂上去。
            // 若前端监听尚未就绪（connect_local 返回前）这条查询会丢失，
            // shell 就停在空白界面。这里由 reader 对启动查询做一次透明应答，
            // 并从输出中剥离该序列，避免前端再回应一次。
            let mut startup_probe: Vec<u8> = Vec::new();
            let mut startup_done = false;

            loop {
                if cancel.is_cancelled() {
                    break;
                }
                match reader.read(&mut buf) {
                    Ok(0) => {
                        // EOF：master 写端已全部关闭（shell 退出）。标记自然退出并
                        // kill（此时多为已死进程，kill 只是确保 watcher 苏醒）。
                        cas_cause(&cause, ExitCause::None, ExitCause::Natural);
                        let _ = killer.kill();
                        break;
                    }
                    Ok(n) => {
                        let mut payload: &[u8] = &buf[..n];
                        if !startup_done {
                            startup_probe.extend_from_slice(payload);
                            if startup_probe.starts_with(b"\x1b[6n") {
                                let _ = writer.blocking_send(b"\x1b[1;1R".to_vec());
                                startup_probe.drain(..4);
                                startup_done = true;
                            } else if !b"\x1b[6n".starts_with(startup_probe.as_slice()) {
                                // 首个输出不是 ConPTY 的启动查询，按普通数据透传。
                                startup_done = true;
                            }
                            payload = if startup_done { &startup_probe } else { &[] };
                        }

                        if !payload.is_empty() {
                            // 日志记录解码前的原始 PTY 字节（ANSI 原样），与
                            // term://data 的解码输出互不影响。
                            if let Some(log) = logger.as_ref() {
                                log.append(payload);
                            }
                            // reader 线程不在 tokio runtime 中，blocking_lock 安全；
                            // Decoder 跨块保留在会话级 encoding 里（与 SSH 一致）。
                            let decoded = {
                                let mut enc = encoding.blocking_lock();
                                decode_chunk(&mut enc, payload)
                            };
                            if !decoded.is_empty() {
                                sink.data(&session_id, decoded);
                            }
                        }
                        if startup_done {
                            startup_probe.clear();
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(e) => {
                        // Windows 关闭 ConPTY 时常见 ERROR_BROKEN_PIPE(109)；
                        // 任何读取错误都按流结束处理。
                        if e.raw_os_error() != Some(109) {
                            applog!("[local] 读取本地 PTY 失败: {e}");
                        }
                        if !cancel.is_cancelled() {
                            cas_cause(&cause, ExitCause::None, ExitCause::PtyBreak);
                            let _ = killer.kill();
                        }
                        break;
                    }
                }
            }
            // 流结束——解码器收尾，把残留的半截多字节序列按 U+FFFD 输出。
            {
                let mut enc = encoding.blocking_lock();
                let tail = crate::encoding::finish_decode(&mut enc);
                if !tail.is_empty() {
                    sink.data(&session_id, tail);
                }
            }
            // 让 writer 泵退出（close/watcher 路径也会 cancel，这里兜底）
            cancel.cancel();
        })
        .map(|_| ())
}

/// `spawn_watcher` 的参数。
struct WatcherArgs {
    session_id: String,
    child: Box<dyn Child + Send + Sync>,
    cancel: CancellationToken,
    connected: Arc<AtomicBool>,
    cause: Arc<AtomicU8>,
    sink: Arc<dyn LocalSink>,
    sessions: Arc<Mutex<HashMap<String, LocalSession>>>,
}

/// 启动子进程 watcher 线程：独占 `Child::wait`。
///
/// 为什么需要它：Windows ConPTY 下 shell 自然退出（用户敲 `exit`）时，master 的
/// 阻塞 read 不会返回——reader 感知不到，会话永远留在表里（句柄泄漏 + 僵尸标签）。
/// watcher 阻塞在 `wait` 上，shell 一退出（自然退出 / 被 kill）就苏醒：
/// - 表里仍有本条会话（非用户关闭路径）→ 标记断开、发出 lost/exit 事件、摘表；
/// - 表里已无本条会话（close() 已处理）→ 静默退出。
fn spawn_watcher(args: WatcherArgs) {
    let WatcherArgs {
        session_id,
        child,
        cancel,
        connected,
        cause,
        sink,
        sessions,
    } = args;
    let _ = std::thread::Builder::new()
        .name(format!("ruthenium-pty-watcher-{session_id}"))
        .spawn(move || {
            // 阻塞到 shell 退出（自然 / 被 kill）。ignore 返回值：原因由 cause 槽决定。
            let mut child = child;
            let _ = child.wait();
            drop(child);

            // 建连路径先启动 watcher、后把会话插入表：shell 可能在插入前
            // 立即退出。给插入留一个很短的窗口，避免留下 connected 僵尸标签；
            // close() 早已移除的会话则最多多等 500ms 后静默退出。
            let mut session = sessions.blocking_lock().remove(&session_id);
            if session.is_none() {
                for _ in 0..50 {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    session = sessions.blocking_lock().remove(&session_id);
                    if session.is_some() {
                        break;
                    }
                }
            }
            let Some(session) = session else {
                return;
            };
            // ConPTY 下 reader 仍阻塞在 read：原因槽若仍是 None（reader 没感知到），
            // 按自然退出处理；否则尊重首写者的判定（PtyBreak / ByUser）。
            cas_cause(&cause, ExitCause::None, ExitCause::Natural);
            let final_cause = load_cause(&cause);

            connected.store(false, Ordering::Relaxed);
            cancel.cancel(); // 停 writer 泵 + reader 的循环检查

            if final_cause != ExitCause::ByUser {
                // 非用户关闭（shell 退出 / PTY 破裂）按 lost 上报，前端据此
                // 展示断线横幅 / 触发自动重连；用户关闭由命令层 emit closed_by_user。
                sink.disconnected(&session_id, events::CONN_REASON_LOST);
                sink.exit(&session_id, "local shell exited");
            }
            // 摘下的会话资源：释放 master（解除 reader 阻塞）、kill 句柄随 drop 丢弃。
            drop(session);
        });
}

/// writer 泵：tokio 端收到 SessionHandle.writer 的字节，交给专用阻塞线程
/// 写入 PTY master（ConPTY 的写端是同步句柄，不能阻塞 tokio worker）。
fn spawn_writer(
    session_id: String,
    mut writer_rx: mpsc::Receiver<Vec<u8>>,
    mut pty_writer: Box<dyn Write + Send>,
    cancel: CancellationToken,
) -> std::io::Result<()> {
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let bridge_cancel = cancel.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = bridge_cancel.cancelled() => break,
                data = writer_rx.recv() => match data {
                    Some(data) => {
                        if tx.send(data).is_err() {
                            break;
                        }
                    }
                    None => break,
                },
            }
        }
    });

    std::thread::Builder::new()
        .name(format!("ruthenium-pty-writer-{session_id}"))
        .spawn(move || {
            while let Ok(data) = rx.recv() {
                if cancel.is_cancelled() {
                    break;
                }
                if pty_writer.write_all(&data).is_err() {
                    break;
                }
                let _ = pty_writer.flush();
            }
            // 线程退出时 drop writer，向 shell 发送 EOF。
        })
        .map(|_| ())
}

/// 失败清理：kill 后不无限等待（命令返回路径上不能卡死）。
fn kill_and_wait(mut child: Box<dyn Child + Send + Sync>) {
    let _ = child.kill();
    let deadline = std::time::Instant::now() + CHILD_EXIT_WAIT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => break,
            Ok(None) if std::time::Instant::now() >= deadline => break,
            Ok(None) => std::thread::sleep(CHILD_EXIT_POLL),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用 sink：把 PTY 输出/退出事件转发到 tokio channel。
    struct ChannelSink(tokio::sync::mpsc::UnboundedSender<TestEvent>);

    #[derive(Debug)]
    enum TestEvent {
        Data(Vec<u8>),
        Disconnected,
        Exit,
    }

    impl LocalSink for ChannelSink {
        fn data(&self, _session_id: &str, data: Vec<u8>) {
            let _ = self.0.send(TestEvent::Data(data));
        }

        fn disconnected(&self, _session_id: &str, _reason: &str) {
            let _ = self.0.send(TestEvent::Disconnected);
        }

        fn exit(&self, _session_id: &str, _reason: &str) {
            let _ = self.0.send(TestEvent::Exit);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn local_shell_echo_close_and_double_close() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let manager = LocalManager::new();
        let session_id = manager
            .connect_with_sink(100, 30, Box::new(ChannelSink(tx)), None, None)
            .await
            .expect("本地会话创建失败");

        // list_sessions：本地会话 hostId 为空、kind=local。
        let listed = manager.list().await;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, session_id);
        assert_eq!(listed[0].host_id, "");
        assert_eq!(listed[0].kind, SessionKind::Local);
        assert!(listed[0].connected);
        assert_eq!(listed[0].encoding, "UTF-8");
        // IPC 序列化契约：kind=local、hostId 为空字符串。
        let json = serde_json::to_value(&listed[0]).expect("Session 应可序列化");
        assert_eq!(json["kind"], "local");
        assert_eq!(json["hostId"], "");

        // 写入真实 shell，等待回显/执行结果。
        manager
            .write(&session_id, b"echo RUTHENIUM_LOCAL_OK\r".to_vec())
            .await
            .expect("写入本地终端失败");

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut collected = Vec::new();
        let mut found = false;
        while tokio::time::Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Some(TestEvent::Data(data))) => {
                    collected.extend_from_slice(&data);
                    if String::from_utf8_lossy(&collected).contains("RUTHENIUM_LOCAL_OK") {
                        found = true;
                        break;
                    }
                }
                Ok(Some(TestEvent::Exit)) | Ok(None) => break,
                Ok(Some(TestEvent::Disconnected)) => {}
                Err(_) => break,
            }
        }
        let text = String::from_utf8_lossy(&collected);
        applog!("[local test] PTY 输出: {text:?}");
        assert!(
            found,
            "10s 内未从本地 PTY 读到 RUTHENIUM_LOCAL_OK，实际输出: {text:?}"
        );

        // resize 走 master.resize，不重建会话。
        manager
            .resize(&session_id, 90, 20)
            .await
            .expect("本地 resize 失败");

        // 关闭：kill 子进程 + 释放 PTY；再次关闭返回 E_NOT_FOUND。
        manager.close(&session_id).await.expect("关闭本地会话失败");
        assert!(!manager.has(&session_id).await);
        let err = manager
            .close(&session_id)
            .await
            .expect_err("重复关闭应失败");
        assert!(
            matches!(err, AppError::ENotFound(_)),
            "重复关闭应返回 E_NOT_FOUND，实际: {err:?}"
        );
    }

    /// shell 自行 `exit` 后，reader 必须把会话从表中摘除——
    /// 否则 list_sessions 持续返回僵尸会话，且前端"重连"会累积泄漏 ConPTY/子进程句柄。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn local_shell_exit_reaps_session_from_table() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let manager = LocalManager::new();
        let session_id = manager
            .connect_with_sink(100, 30, Box::new(ChannelSink(tx)), None, None)
            .await
            .expect("本地会话创建失败");

        // 让 shell 自己退出（模拟用户在终端里敲 exit）
        manager
            .write(&session_id, b"exit\r\n".to_vec())
            .await
            .expect("写入失败");

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if tokio::time::Instant::now() >= deadline {
                panic!("10s 内未收到 PTY Exit 事件");
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(remaining, rx.recv()).await {
                // Exit 事件之后 reader 还要同步摘表，继续走下方的轮询等待
                Ok(Some(TestEvent::Exit)) | Ok(None) => break,
                Ok(Some(_)) => {}
                Err(_) => panic!("10s 内未收到 PTY Exit 事件"),
            }
        }
        // reader 摘表在 Exit 事件之后同步执行，稍等它落地
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        while manager.has(&session_id).await {
            if tokio::time::Instant::now() >= deadline {
                panic!("shell 退出后会话仍留在表里（未 reap）");
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(
            manager.list().await.is_empty(),
            "list_sessions 不得返回已退出的会话"
        );
    }
}
