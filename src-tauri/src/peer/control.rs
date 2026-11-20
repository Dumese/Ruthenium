//! 对等终端控制：控制方（发起连接）与受控方（本机确认 + ConPTY）。
//!
//! 安全与交互：
//! - 控制方：钉扎校验对端身份 → `exec("ruthenium-info")` 预检 `grantControl` →
//!   可选的本机 Windows Hello 前置验证 → 打开 pty/shell；
//! - 受控方：`shell_request` 必须 **每次** 经本机确认（30s 未确认按拒绝）；开启
//!   `requireWindowsHello` 时确认后再过一次系统验证（阻塞调用放 spawn_blocking）；
//! - 受控会话登记在 [`ControlHub`]，前端显示常驻横幅并可「立即断开」；
//! - 审计：谁（设备名/指纹）、何时、来自哪个 IP、终端尺寸、持续时长、结束原因。
//!
//! 阻塞语义：受控端的确认弹窗发生在该连接的 SSH 消息循环里，所以**每条控制会话
//! 必须独立一条 SSH 连接**（不与其他用途复用）。

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};

use portable_pty::PtySize;
use serde::{Deserialize, Serialize};
use tauri::Emitter;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::models::SessionKind;
use crate::peer::client;
use crate::peer::peers::PeerRecord;
use crate::ssh::channel::{open_term_channel, TermChannelArgs};
use crate::ssh::{make_session_handle, new_session_id, LiveSession};
use crate::state::AppState;

/// 本机确认等待上限（超时按拒绝处理）。
pub const CONSENT_TIMEOUT: Duration = Duration::from_secs(30);
/// PTY 尺寸下限：0×0 会让 CreatePseudoConsole 返回 E_INVALIDARG。
const MIN_COLS: u16 = 20;
const MIN_ROWS: u16 = 5;
/// 上限：防御客户端发超大尺寸把 ConPTY 撑爆。
const MAX_COLS: u16 = 1000;
const MAX_ROWS: u16 = 600;

/// `pty_request` 尺寸收敛（clamp）。
pub fn clamp_size(cols: u16, rows: u16) -> (u16, u16) {
    (
        cols.clamp(MIN_COLS, MAX_COLS),
        rows.clamp(MIN_ROWS, MAX_ROWS),
    )
}

/// 受控端一条活动会话（横幅 / 审计 / 踢出）。
pub struct ControlSession {
    pub peer_name: String,
    pub peer_addr: String,
    pub cols: u16,
    pub rows: u16,
    pub started_at: String,
    pub started_ms: i64,
    pub started: Instant,
    /// 取消 → 数据泵退出并杀掉 ConPTY（“立即断开”）。
    pub cancel: CancellationToken,
}

/// 前端展示用的受控会话。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ControlSessionView {
    pub session_id: String,
    pub peer_name: String,
    pub peer_addr: String,
    pub cols: u16,
    pub rows: u16,
    pub started_at: String,
    pub started_ms: i64,
    pub elapsed_sec: u64,
}

/// 控制请求弹窗 payload（`peer://control-request`）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ControlRequest {
    pub consent_id: String,
    pub peer_name: String,
    pub peer_fp: String,
    pub addr: String,
    pub cols: u16,
    pub rows: u16,
    pub hello_required: bool,
}

/// 受控会话开始/结束（`peer://session`）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ControlSessionEvent {
    /// started / ended
    pub action: String,
    pub session_id: String,
    pub peer_name: String,
    /// 开始时间（epoch ms，前端据此本地计时）
    pub started_ms: Option<i64>,
    /// 来源地址 / 终端尺寸（started 事件带上，横幅无需再查后端）
    pub addr: Option<String>,
    pub cols: Option<u16>,
    pub rows: Option<u16>,
    /// 结束原因：kicked / closed / exited / failed
    pub reason: Option<String>,
}

/// 拒绝原因（`peer://denied`）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeniedEvent {
    pub peer_name: String,
    /// consent / hello
    pub reason: String,
    /// 对应的确认请求 id：前端据此关掉正确的弹窗（含超时分支）
    pub consent_id: Option<String>,
    /// `control` / `screen`（决定提示文案）
    pub kind: String,
}

/// 受控端状态：待确认请求 + 活动会话 + 确认框频控。
#[derive(Default)]
pub struct ControlHub {
    consents: Mutex<HashMap<String, oneshot::Sender<bool>>>,
    sessions: Mutex<HashMap<String, ControlSession>>,
    /// peerId → 最近一次弹确认框的时间（防同一设备连续刷确认框）
    prompts: Mutex<HashMap<String, Instant>>,
}

impl ControlHub {
    pub fn new() -> Self {
        Self::default()
    }

    /// 确认框频控：同一设备 5s 内只弹一次（拒绝后立刻重试不会刷屏）。
    pub async fn prompt_allowed(&self, peer_id: &str) -> bool {
        const COOLDOWN: Duration = Duration::from_secs(5);
        let mut map = self.prompts.lock().await;
        let now = Instant::now();
        map.retain(|_, t| now.duration_since(*t) < COOLDOWN);
        if map.contains_key(peer_id) {
            return false;
        }
        map.insert(peer_id.to_string(), now);
        true
    }

    /// 登记一次待确认请求（shell_request 内 await，30s 超时按拒绝）。
    pub async fn request_consent(&self, consent_id: String) -> oneshot::Receiver<bool> {
        let (tx, rx) = oneshot::channel();
        self.consents.lock().await.insert(consent_id, tx);
        rx
    }

    /// 前端点「允许 / 拒绝」；返回是否命中。
    pub async fn decide(&self, consent_id: &str, allow: bool) -> bool {
        match self.consents.lock().await.remove(consent_id) {
            Some(tx) => tx.send(allow).is_ok(),
            None => false,
        }
    }

    /// 超时/取消时清掉挂起条目（send 会失败，对端按拒绝处理）。
    pub async fn cancel_consent(&self, consent_id: &str) {
        self.consents.lock().await.remove(consent_id);
    }

    pub async fn register(&self, session_id: String, session: ControlSession) {
        self.sessions.lock().await.insert(session_id, session);
    }

    pub async fn finish(&self, session_id: &str) -> Option<ControlSession> {
        self.sessions.lock().await.remove(session_id)
    }

    pub async fn list(&self) -> Vec<ControlSessionView> {
        let now = Instant::now();
        self.sessions
            .lock()
            .await
            .iter()
            .map(|(id, s)| ControlSessionView {
                session_id: id.clone(),
                peer_name: s.peer_name.clone(),
                peer_addr: s.peer_addr.clone(),
                cols: s.cols,
                rows: s.rows,
                started_at: s.started_at.clone(),
                started_ms: s.started_ms,
                elapsed_sec: now.duration_since(s.started).as_secs(),
            })
            .collect()
    }

    /// 停止局域网监听时结束全部受控会话。
    pub async fn cancel_all(&self) {
        for (_, s) in self.sessions.lock().await.drain() {
            s.cancel.cancel();
        }
    }

    /// 「立即断开」：取消该会话的数据泵（ConPTY 随后被杀掉）。
    pub async fn kick(&self, session_id: &str) -> bool {
        match self.sessions.lock().await.get(session_id) {
            Some(s) => {
                s.cancel.cancel();
                true
            }
            None => false,
        }
    }
}

/// 控制方 `ruthenium-info` 的返回。
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PeerInfo {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub grant_control: bool,
    #[serde(default)]
    pub grant_files: bool,
    #[serde(default)]
    pub grant_config: bool,
    #[serde(default)]
    pub grant_screen: bool,
    #[serde(default)]
    pub grant_clipboard: bool,
    #[serde(default)]
    pub grant_audio: bool,
    /// 对端是否支持「查看并操作」的子系统名（不支持时只认 ruthenium-screen）
    #[serde(default)]
    pub screen_input: bool,
    #[serde(default)]
    pub require_windows_hello: bool,
}

/// 控制方：打开对端终端（返回前端会话 id）。
pub async fn open_terminal(
    state: &AppState,
    peer_id: &str,
    cols: u16,
    rows: u16,
    attempt_id: Option<String>,
) -> AppResult<String> {
    let peer = state
        .peer
        .peer_record(peer_id)
        .await
        .ok_or_else(|| AppError::EPeerNotPaired("该设备未配对".to_string()))?;

    // 1) 本机前置 Hello（与“启动验证”同一偏好）：未通过不发起任何连接。
    let (need_hello, log_sessions) = {
        let c = state.config.read().await;
        (
            c.preferences.require_windows_hello,
            c.preferences.log_sessions,
        )
    };
    if need_hello {
        let peer_name = peer.name.clone();
        let outcome = tauri::async_runtime::spawn_blocking(move || {
            crate::hello::verify(&format!("远程控制「{peer_name}」"))
        })
        .await
        .map_err(|e| AppError::EPeerHelloDenied(format!("Windows Hello 线程失败: {e}")))?;
        if outcome != crate::hello::HelloOutcome::Verified {
            return Err(AppError::EPeerHelloDenied(format!(
                "Windows Hello 验证未通过（{outcome:?}），已取消控制"
            )));
        }
    }

    // 2) 取消令牌（关标签可中断握手 / 确认等待）。
    let attempt = attempt_id.filter(|s| !s.trim().is_empty());
    let cancel = match attempt.as_deref() {
        Some(id) => Some(state.register_connect_attempt(id).await),
        None => None,
    };

    let result = open_terminal_inner(state, &peer, cols, rows, log_sessions, cancel.clone()).await;
    if let (Some(id), Some(token)) = (attempt.as_deref(), cancel.as_ref()) {
        state.finish_connect_attempt(id, token).await;
    }
    result
}

async fn open_terminal_inner(
    state: &AppState,
    peer: &PeerRecord,
    cols: u16,
    rows: u16,
    log_sessions: bool,
    cancel: Option<CancellationToken>,
) -> AppResult<String> {
    // 3) 钉扎校验 + 身份认证（复用文件互传/配置推送的连接实现）。
    let identity = state.peer.identity_handle().await?;
    let mut handle = client::connect(state, peer, "远程终端").await?;
    client::authenticate(&mut handle, &identity).await?;

    // 4) `ruthenium-info`：预检对端授权（授权可能在配对后被撤销）。
    let info = query_info(&handle, cancel.as_ref()).await?;
    state.peer.sync_peer_allows(&peer.id, &info).await;
    if !info.grant_control {
        return Err(AppError::EPeerNotAllowed(format!(
            "{} 未允许远程终端（可能已撤销授权）",
            peer.name
        )));
    }
    if info.require_windows_hello {
        applog!("[peer] 对端开启了 Windows Hello：确认时还会再验证一次");
    }
    if let Some(name) = info.name.as_deref() {
        applog!(
            "[peer] 对端能力查询: name={name} grantControl={}",
            info.grant_control
        );
    }

    // 5) 会话装配（与普通终端共用 reader/writer/日志/事件链路）。
    let (cols, rows) = clamp_size(cols, rows);
    let session_id = new_session_id();
    let (session_handle, mut writer_rx) =
        make_session_handle(session_id.clone(), peer.id.clone(), SessionKind::Peer);
    let logger = if log_sessions {
        let name = format!("peer-{}", sanitize(&peer.name));
        match crate::session_log::SessionLogger::open(&name) {
            Ok(log) => {
                applog!("[peer] 控制会话日志: {}", log.path().display());
                Some(log)
            }
            Err(e) => {
                applog!("[session-log] 创建控制会话日志失败（本次不记录）: {e}");
                None
            }
        }
    } else {
        None
    };
    let ctrl = open_term_channel(
        &mut handle,
        TermChannelArgs {
            session_id: session_id.clone(),
            cols,
            rows,
            app: Some(state.app.clone()),
            data_channel: None,
            cancel: session_handle.cancel.clone(),
            encoding: session_handle.encoding.clone(),
            logger,
            // 对端可能弹确认：必须等 pty/shell 的 Success/Failure
            wait_replies: true,
            wait_cancel: cancel.clone(),
        },
    )
    .await?;

    // 对端确认期间用户可能已经关闭标签；不要建出一个没有标签可关闭的
    // 控制会话。
    if cancel.as_ref().is_some_and(|token| token.is_cancelled()) {
        session_handle.cancel.cancel();
        return Err(AppError::ECancelled("连接已取消".to_string()));
    }

    // writer 泵：xterm 输入 → SSH channel
    let ctrl2 = ctrl.clone();
    tokio::spawn(async move {
        while let Some(data) = writer_rx.recv().await {
            if ctrl2
                .send(crate::ssh::channel::ChannelCmd::Data(data))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    let (addr, port) = state
        .peer
        .peer_addr(&peer.id)
        .await
        .and_then(|a| {
            a.rsplit_once(':')
                .map(|(h, p)| (h.to_string(), p.parse::<u16>().unwrap_or(0)))
        })
        .unwrap_or((String::new(), 0));
    let live = Arc::new(LiveSession {
        inner: session_handle,
        connected: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        russh: Arc::new(tokio::sync::Mutex::new(handle)),
        ctrl,
        owns_connection: true,
        addr,
        port,
        user: "ruthenium".to_string(),
        cols: std::sync::atomic::AtomicU16::new(cols),
        rows: std::sync::atomic::AtomicU16::new(rows),
    });
    state.ssh.insert(live).await;
    crate::events::emit(
        &state.app,
        crate::events::CONN_STATE,
        crate::events::ConnStatePayload {
            session_id: session_id.clone(),
            state: "connected".to_string(),
            latency_ms: None,
            reason: None,
        },
    );
    applog!("[peer] 控制会话开始: {}（{}×{}）", peer.name, cols, rows);
    Ok(session_id)
}

/// 控制方：按需刷新「对方允许本机」的授权快照（连接一次 `ruthenium-info`）。
///
/// 授权在对方实时判定，但设备页展示的快照只在屏幕/终端/声音/剪贴板连接时更新；
/// 文件互传/配置推送不会触发，因此打开设备页时主动刷一次。
pub async fn refresh_peer_grants(state: &AppState, peer_id: &str) -> AppResult<Option<PeerRecord>> {
    let Some(peer) = state.peer.peer_record(peer_id).await else {
        return Ok(None);
    };
    let identity = state.peer.identity_handle().await?;
    let mut handle = client::connect(state, &peer, "授权刷新").await?;
    client::authenticate(&mut handle, &identity).await?;
    let info = query_info(&handle, None).await?;
    state.peer.sync_peer_allows(&peer.id, &info).await;
    Ok(state.peer.peer_record(peer_id).await)
}

/// 控制方：`exec("ruthenium-info")` 并解析 JSON（3s 超时；可被取消）。
pub(crate) async fn query_info(
    handle: &russh::client::Handle<crate::ssh::client::ClientHandler>,
    cancel: Option<&CancellationToken>,
) -> AppResult<PeerInfo> {
    let mut channel = handle
        .channel_open_session()
        .await
        .map_err(|e| AppError::ESshChannel(format!("打开能力查询通道失败: {e}")))?;
    channel
        .exec(true, "ruthenium-info")
        .await
        .map_err(|e| AppError::ESshChannel(format!("请求能力查询失败: {e}")))?;
    let mut buf = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let next = tokio::select! {
            _ = crate::ssh::cancelled(cancel) => {
                return Err(AppError::ECancelled("连接已取消".to_string()));
            }
            r = tokio::time::timeout_at(deadline, channel.wait()) => r,
        };
        match next {
            Ok(Some(russh::ChannelMsg::Data { data })) => buf.extend_from_slice(&data),
            Ok(Some(russh::ChannelMsg::Eof)) | Ok(Some(russh::ChannelMsg::Close)) | Ok(None) => {
                break
            }
            Ok(Some(_)) => {}
            // 超时：用已收到的内容判断（不把“能力查询慢”当成连接失败）
            Err(_) => break,
        }
    }
    serde_json::from_slice(&buf).map_err(|e| {
        AppError::EPeerNotPaired(format!(
            "对端能力查询失败（可能版本不匹配或已撤销配对）: {e}"
        ))
    })
}

fn sanitize(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() {
        "peer".to_string()
    } else {
        s
    }
}

/// 受控端：本机确认（+ 可选 Hello）。
///
/// **在 `channel_success` 之前调用**：拒绝时 server 回 `ChannelFailure`，
/// 控制方会立刻收到明确错误（而不是“连上就断”的空标签）。
pub async fn approve(
    hub: &ControlHub,
    app: &tauri::AppHandle,
    peer: &PeerRecord,
    peer_addr: Option<std::net::SocketAddr>,
    size: (u16, u16),
    require_hello: bool,
) -> Result<(), String> {
    let (cols, rows) = clamp_size(size.0, size.1);
    let addr_text = peer_addr
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|| "未知地址".to_string());

    // 频控：同一设备 5s 内只弹一次（避免已配对设备反复刷确认框）
    if !hub.prompt_allowed(&peer.id).await {
        applog!("[peer] 控制请求过于频繁，已忽略: {}", peer.name);
        return Err("请求过于频繁，请稍后再试".to_string());
    }

    // 1) 本机确认：每次控制都要显式允许（30s 不操作按拒绝）。
    let consent_id = Uuid::new_v4().to_string();
    let rx = hub.request_consent(consent_id.clone()).await;
    let _ = app.emit(
        crate::events::PEER_CONTROL_REQUEST,
        ControlRequest {
            consent_id: consent_id.clone(),
            peer_name: peer.name.clone(),
            peer_fp: peer.id.chars().skip(7).take(8).collect::<String>(),
            addr: addr_text.clone(),
            cols,
            rows,
            hello_required: require_hello,
        },
    );
    applog!(
        "[peer] 控制请求: {} from {addr_text}（{}×{}），等待本机确认",
        peer.name,
        cols,
        rows
    );
    let allow = match tokio::time::timeout(CONSENT_TIMEOUT, rx).await {
        Ok(Ok(v)) => v,
        Ok(Err(_)) => false,
        Err(_) => {
            hub.cancel_consent(&consent_id).await;
            false
        }
    };
    if !allow {
        let _ = app.emit(
            crate::events::PEER_DENIED,
            DeniedEvent {
                peer_name: peer.name.clone(),
                reason: "consent".to_string(),
                consent_id: Some(consent_id.clone()),
                kind: "control".to_string(),
            },
        );
        applog!("[peer] 已拒绝 {} 的控制请求（未确认/超时）", peer.name);
        return Err("本机拒绝控制请求".to_string());
    }

    // 2) Hello 二次验证（开启时）：阻塞调用必须放阻塞线程池。
    if require_hello {
        let peer_name = peer.name.clone();
        let outcome = tauri::async_runtime::spawn_blocking(move || {
            crate::hello::verify(&format!("「{peer_name}」请求控制本机"))
        })
        .await
        .unwrap_or(crate::hello::HelloOutcome::Error);
        if outcome != crate::hello::HelloOutcome::Verified {
            let _ = app.emit(
                crate::events::PEER_DENIED,
                DeniedEvent {
                    peer_name: peer.name.clone(),
                    reason: "hello".to_string(),
                    consent_id: Some(consent_id),
                    kind: "control".to_string(),
                },
            );
            applog!("[peer] Windows Hello 未通过（{outcome:?}），拒绝控制");
            return Err("Windows Hello 未通过".to_string());
        }
    }
    Ok(())
}

/// 受控端：创建 ConPTY + 数据泵并接管该 SSH 通道；返回 resize 发送端。
#[allow(clippy::too_many_arguments)]
pub fn start(
    app: tauri::AppHandle,
    hub: Arc<ControlHub>,
    peer: PeerRecord,
    peer_addr: String,
    size: (u16, u16),
    stream: russh::ChannelStream<russh::server::Msg>,
    grant: crate::peer::server::GrantCheck,
) -> AppResult<mpsc::Sender<(u16, u16)>> {
    spawn_pty_pump(app, hub, peer, peer_addr, size, stream, grant)
}

/// 受控端：PTY ↔ SSH 通道双向泵 + 尺寸同步 + 审计与横幅事件。
///
/// 返回 resize 发送端：server 在 `window_change_request` 里推入新尺寸。
#[allow(clippy::too_many_arguments)]
fn spawn_pty_pump(
    app: tauri::AppHandle,
    hub: Arc<ControlHub>,
    peer: PeerRecord,
    peer_addr: String,
    size: (u16, u16),
    stream: russh::ChannelStream<russh::server::Msg>,
    grant: crate::peer::server::GrantCheck,
) -> AppResult<mpsc::Sender<(u16, u16)>> {
    let (cols, rows) = clamp_size(size.0, size.1);
    // 控制会话固定用 PowerShell 优先的候选：脚本能力一致，且可观测窗口宽度（验证 resize）
    let (master, child) =
        crate::local::open_pty_with(cols, rows, crate::local::control_shell_candidates())?;
    let mut reader = master
        .try_clone_reader()
        .map_err(|e| AppError::ESshChannel(format!("克隆 PTY reader 失败: {e}")))?;
    let mut writer = master
        .take_writer()
        .map_err(|e| AppError::ESshChannel(format!("获取 PTY writer 失败: {e}")))?;
    let mut killer = child.clone_killer();

    // PTY → 异步任务（阻塞读放在独立线程，避免占用 tokio 工作线程）
    let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(64);
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 8 * 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if out_tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    // shell 自然退出 → 结束泵
    let (exit_tx, mut exit_rx) = mpsc::channel::<()>(1);
    std::thread::spawn(move || {
        let mut child = child;
        let _ = child.wait();
        let _ = exit_tx.blocking_send(());
    });

    let session_id = Uuid::new_v4().to_string();
    let cancel = CancellationToken::new();
    let started = Instant::now();
    let started_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let stamp = crate::session_log::LogStamp::now();
    let started_at = format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        stamp.year, stamp.month, stamp.day, stamp.hour, stamp.minute, stamp.second
    );
    let peer_name = peer.name.clone();
    let sid = session_id.clone();
    let hub2 = hub.clone();
    let app2 = app.clone();
    let cancel2 = cancel.clone();
    let name2 = peer_name.clone();
    let addr2 = peer_addr.clone();

    let (resize_tx, mut resize_rx) = mpsc::channel::<(u16, u16)>(8);
    tokio::spawn(async move {
        hub2.register(
            sid.clone(),
            ControlSession {
                peer_name: name2.clone(),
                peer_addr: addr2.clone(),
                cols,
                rows,
                started_at: started_at.clone(),
                started_ms,
                started,
                cancel: cancel2.clone(),
            },
        )
        .await;
        let _ = app2.emit(
            crate::events::PEER_SESSION,
            ControlSessionEvent {
                action: "started".to_string(),
                session_id: sid.clone(),
                peer_name: name2.clone(),
                started_ms: Some(started_ms),
                addr: Some(addr2.clone()),
                cols: Some(cols),
                rows: Some(rows),
                reason: None,
            },
        );

        let mut stream = stream;
        let master = master;
        let mut buf = vec![0u8; 8 * 1024];
        let mut reason = Some("closed");
        // 运行中每 5s 复核 peers.toml：撤销控制授权后主动断开
        let mut grant_tick = tokio::time::interval(Duration::from_secs(5));
        grant_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => { reason = Some("kicked"); break; }
                _ = exit_rx.recv() => { reason = Some("exited"); break; }
                _ = grant_tick.tick() => {
                    if !grant.still_granted(|p| p.grant_control).await {
                        reason = Some("revoked");
                        let _ = tokio::time::timeout(
                            Duration::from_secs(2),
                            stream.write_all("\r\n[控制授权已被撤销，连接已断开]\r\n".as_bytes()),
                        )
                        .await;
                        applog!("[peer] 控制授权已撤销，断开会话: {}", name2);
                        break;
                    }
                }
                size = resize_rx.recv() => {
                    if let Some((c, r)) = size {
                        let _ = master.resize(PtySize { rows: r, cols: c, pixel_width: 0, pixel_height: 0 });
                    }
                }
                data = out_rx.recv() => match data {
                    Some(bytes) => {
                        let written = tokio::select! {
                            _ = cancel.cancelled() => false,
                            r = stream.write_all(&bytes) => r.is_ok(),
                        };
                        if !written { break; }
                    }
                    None => { reason = Some("exited"); break; }
                },
                n = stream.read(&mut buf) => match n {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if writer.write_all(&buf[..n]).is_err() { reason = Some("pty"); break; }
                    }
                },
            }
        }
        let _ = killer.kill();
        drop(stream);
        let dur = started.elapsed().as_secs();
        hub2.finish(&sid).await;
        let _ = app2.emit(
            crate::events::PEER_SESSION,
            ControlSessionEvent {
                action: "ended".to_string(),
                session_id: sid.clone(),
                peer_name: name2.clone(),
                started_ms: Some(started_ms),
                addr: Some(addr2.clone()),
                cols: Some(cols),
                rows: Some(rows),
                reason: Some(reason.unwrap_or("closed").to_string()),
            },
        );
        applog!(
            "[peer] 控制会话结束: {} from {addr2}（{}×{}，{}s，原因={}）",
            name2,
            cols,
            rows,
            dur,
            reason.unwrap_or("closed")
        );
        crate::peer::audit::append(&crate::peer::audit::AuditEntry {
            at: crate::peer::audit::stamp(),
            kind: "control".to_string(),
            direction: "incoming".to_string(),
            peer_name: name2.clone(),
            peer_addr: addr2.clone(),
            quality: None,
            width: Some(cols as u32),
            height: Some(rows as u32),
            frames: None,
            bytes: None,
            seconds: dur,
            reason: Some(reason.unwrap_or("closed").to_string()),
        });
    });

    // 真实的 resize 通道由调用方（server）持有：这里返回 sender 供其登记。
    Ok(resize_tx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_size_keeps_pty_usable() {
        // 0×0 会让 CreatePseudoConsole 返回 E_INVALIDARG
        assert_eq!(clamp_size(0, 0), (MIN_COLS, MIN_ROWS));
        assert_eq!(clamp_size(1, 1), (MIN_COLS, MIN_ROWS));
        assert_eq!(clamp_size(120, 32), (120, 32));
        assert_eq!(clamp_size(u16::MAX, u16::MAX), (MAX_COLS, MAX_ROWS));
    }

    #[tokio::test]
    async fn consent_decision_is_single_shot() {
        let hub = ControlHub::new();
        let rx = hub.request_consent("c1".to_string()).await;
        assert!(hub.decide("c1", true).await);
        assert!(rx.await.unwrap());
        // 同一次请求只能决定一次（防重复点击/伪造响应）
        assert!(!hub.decide("c1", true).await);
    }

    #[tokio::test]
    async fn kick_cancels_active_session() {
        let hub = Arc::new(ControlHub::new());
        let cancel = CancellationToken::new();
        hub.register(
            "s1".to_string(),
            ControlSession {
                peer_name: "peer".into(),
                peer_addr: "127.0.0.1".into(),
                cols: 120,
                rows: 32,
                started_at: String::new(),
                started_ms: 0,
                started: Instant::now(),
                cancel: cancel.clone(),
            },
        )
        .await;
        assert_eq!(hub.list().await.len(), 1);
        assert!(hub.kick("s1").await);
        assert!(cancel.is_cancelled());
        assert!(!hub.kick("missing").await);
        assert!(hub.finish("s1").await.is_some());
        assert!(hub.list().await.is_empty());
    }
}
