//! 会话命令：connect / connect_transient / connect_local / write / resize / close / reconnect / list。
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use tauri::AppHandle;
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;

use crate::commands::tunnel::start_tunnels_for_host;
use crate::error::{AppError, AppResult};
use crate::events::{self, ConnStatePayload, TermDataPayload};
use crate::local::AppEventSink;
use crate::models::{AuthMethod, Host, SessionKind};
use crate::session_log::SessionLogger;
use crate::ssh::channel::{open_term_channel, send_init_command, ChannelCmd, TermChannelArgs};
use crate::ssh::client::ClientHandler;
use crate::ssh::{connect_client, make_session_handle, new_session_id, LiveSession};
use crate::state::AppState;

/// 终端数据通道 id（官方推荐的流式 IPC）。前端 `Channel` 对象经 IPC 序列化为
/// `__CHANNEL__:<id>` 字符串，这里反解并 `channel_on` 绑定到当前 webview。
/// 用 `JavaScriptChannelId` 而非直接收 `Channel<T>`：它实现了 `Deserialize`，
/// 因此参数可为 `Option`——旧调用方 / E2E 脚本不传 `onData` 时为 `None`，
/// 数据回退全局 `term://data` 事件，保持完全向后兼容。
type DataChannelId = tauri::ipc::JavaScriptChannelId;

/// 把可选的 channel id 解析成 `Channel<TermDataPayload>`（无 webview 场景返回 None）。
fn resolve_data_channel<R: tauri::Runtime>(
    webview: tauri::Webview<R>,
    on_data: Option<DataChannelId>,
) -> Option<DataChannel> {
    on_data.map(|id| id.channel_on(webview))
}

type DataChannel = tauri::ipc::Channel<TermDataPayload>;

/// 前端建连的默认终端尺寸；重连时若从未 resize 过（0 值）回退到该值。
const DEFAULT_TERM_COLS: u16 = 120;
const DEFAULT_TERM_ROWS: u16 = 32;

/// 认证阶段总超时：agent 多身份 + RSA 多 hash 候选 + keyring 读取都算在内。
/// 服务器接受 TCP/协商却很慢地回认证结果时，不能把命令永远挂住。
const AUTH_TIMEOUT: Duration = Duration::from_secs(30);

/// 日志文件名用的名称 —— 主机名优先，空名回退稳定 id。
fn log_name(host: &Host) -> String {
    let name = host.name.trim();
    if name.is_empty() {
        host.id.clone()
    } else {
        name.to_string()
    }
}

/// 按开关创建会话日志（`host.log || preferences.logSessions`）。
/// 日志是旁路功能：创建失败只打 stderr，不阻断连接。
fn create_session_log(enabled: bool, name: &str) -> Option<SessionLogger> {
    if !enabled {
        return None;
    }
    match SessionLogger::open(name) {
        Ok(log) => {
            applog!("[session-log] 记录会话日志: {}", log.path().display());
            Some(log)
        }
        Err(e) => {
            applog!("[session-log] 创建会话日志失败（本次不记录）: {e}");
            None
        }
    }
}

fn emit_connected(app: &AppHandle, session_id: &str) {
    events::emit(
        app,
        events::CONN_STATE,
        ConnStatePayload {
            session_id: session_id.to_string(),
            state: "connected".to_string(),
            latency_ms: None,
            reason: None,
        },
    );
}

/// 用户主动关闭（close_session / 关标签）时上报一次，前端不得据此自动重连。
fn emit_closed_by_user(app: &AppHandle, session_id: &str) {
    events::emit(
        app,
        events::CONN_STATE,
        ConnStatePayload {
            session_id: session_id.to_string(),
            state: "disconnected".to_string(),
            latency_ms: None,
            reason: Some(events::CONN_REASON_CLOSED_BY_USER.to_string()),
        },
    );
}

/// 建立 SSH 会话。
/// `password` 仅当 `auth = password` 时使用，且只存活于本次调用栈（用户选择不保存）。
///
/// 已知边界（接受）：`password` 经 serde 反序列化进入本函数，该副本由
/// tauri 命令宏生成、无法 zeroize（`auth.rs` 内部的 `Zeroizing` 只覆盖后续拷贝）。
/// 残余风险是内存转储/页面文件中可能留有明文——与进程内任何明文密码处理
/// （含 ssh 客户端本体）同级，由系统级防护（BitLocker/页面文件加密）兜底。
/// `on_data`：终端数据通道；term 数据走 Channel（官方流式 IPC）。
#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub async fn connect<R: tauri::Runtime>(
    webview: tauri::Webview<R>,
    state: tauri::State<'_, AppState>,
    host_id: String,
    cols: u16,
    rows: u16,
    kind: Option<SessionKind>,
    password: Option<String>,
    attempt_id: Option<String>,
    on_data: Option<DataChannelId>,
) -> AppResult<String> {
    if kind == Some(SessionKind::Local) {
        return Err(AppError::EInvalidInput(
            "本地终端请使用 connect_local".to_string(),
        ));
    }
    let host = state
        .config
        .read()
        .await
        .hosts
        .iter()
        .find(|h| h.id == host_id)
        .cloned()
        .ok_or_else(|| AppError::ENotFound(format!("主机 {host_id} 不存在")))?;
    let kind = kind.unwrap_or(SessionKind::Term);
    // 与 connect_transient / upsert_host 保持一致：port 0 不是合法 SSH 端口。
    if host.port == 0 {
        return Err(AppError::EInvalidInput("端口必须大于 0".to_string()));
    }
    let on_data = resolve_data_channel(webview, on_data);
    connect_inner(
        &state, host, cols, rows, kind, password, attempt_id, on_data,
    )
    .await
}

/// 临时主机连接（「快速连接」用）。
///
/// 语义与 [`connect`] 完全一致：同一套指纹校验、按 `host.auth` 认证、同一套 PTY/shell、
/// 隧道仍按 `host.id` 处理；但**不落盘、不修改 config**——`host` 只用于本次连接，
/// `list_sessions` 里该会话的 `hostId` 就是传入的 `host.id`，关标签即丢弃。
#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub async fn connect_transient<R: tauri::Runtime>(
    webview: tauri::Webview<R>,
    state: tauri::State<'_, AppState>,
    host: Host,
    cols: u16,
    rows: u16,
    kind: Option<SessionKind>,
    password: Option<String>,
    attempt_id: Option<String>,
    on_data: Option<DataChannelId>,
) -> AppResult<String> {
    if host.id.trim().is_empty() {
        return Err(AppError::EInvalidInput("临时主机缺少 id".to_string()));
    }
    if host.addr.trim().is_empty() {
        return Err(AppError::EInvalidInput("主机地址不能为空".to_string()));
    }
    if host.port == 0 {
        return Err(AppError::EInvalidInput("端口无效".to_string()));
    }
    // 与落盘路径同一条规范化链（auth != key 时不保留 keyRef），只作用于内存对象。
    let kind = kind.unwrap_or(SessionKind::Term);
    if kind == SessionKind::Local {
        return Err(AppError::EInvalidInput(
            "本地终端请使用 connect_local".to_string(),
        ));
    }
    let mut host = host;
    host.normalize_auth();
    let on_data = resolve_data_channel(webview, on_data);
    connect_inner(
        &state, host, cols, rows, kind, password, attempt_id, on_data,
    )
    .await
}

/// `connect` / `connect_transient` / `reconnect` 共用的建连主路径（唯一一套会话创建逻辑）。
///
/// `attempt_id`：前端"正在连接的标签 id"。传入即注册取消令牌；
/// 用户关闭该标签（`cancel_connect`）时握手/认证立即中止。无论结果如何都会清理注册表。
#[allow(clippy::too_many_arguments)]
async fn connect_inner(
    state: &tauri::State<'_, AppState>,
    host: Host,
    cols: u16,
    rows: u16,
    kind: SessionKind,
    password: Option<String>,
    attempt_id: Option<String>,
    on_data: Option<DataChannel>,
) -> AppResult<String> {
    let attempt = attempt_id.filter(|id| !id.trim().is_empty());
    let cancel = match attempt.as_deref() {
        Some(id) => Some(state.register_connect_attempt(id).await),
        None => None,
    };
    // 收尾时按令牌条件清理：同一 attemptId 可能已被新尝试替换，不能误删新令牌。
    let finish_token = cancel.clone();
    let result =
        connect_inner_cancellable(state, host, cols, rows, kind, password, cancel, on_data).await;
    if let (Some(id), Some(token)) = (attempt.as_deref(), finish_token.as_ref()) {
        state.finish_connect_attempt(id, token).await;
    }
    result
}

/// [`connect_inner`] 的实际实现：`cancel` 为 `None` 表示本次连接不可取消。
#[allow(clippy::too_many_arguments)]
async fn connect_inner_cancellable(
    state: &tauri::State<'_, AppState>,
    host: Host,
    cols: u16,
    rows: u16,
    kind: SessionKind,
    password: Option<String>,
    cancel: Option<CancellationToken>,
    on_data: Option<DataChannel>,
) -> AppResult<String> {
    // 同一主机已有活动终端会话时，SFTP 复用该连接开新 channel，
    // 不得重新握手 / 重新认证（否则未保存密码的用户每开一次 SFTP 都要重输）。
    if kind == SessionKind::Sftp {
        if let Some(session_id) = state.ssh.reuse_sftp_session(&host).await {
            emit_connected(&state.app, &session_id);
            return Ok(session_id);
        }
    }

    // 连接时读取 keepalive 偏好（改设置后对新连接生效）。
    // 日志开关 = 主机开关 || 全局默认。
    let (keepalive_secs, log_sessions) = {
        let config = state.config.read().await;
        (
            config.preferences.keepalive_secs,
            config.preferences.log_sessions,
        )
    };

    let session_id = new_session_id();
    let handler = ClientHandler {
        session_id: session_id.clone(),
        host_id: host.id.clone(),
        host: host.addr.clone(),
        port: host.port,
        pending: state.pending_hostkey.clone(),
        app: Some(state.app.clone()),
        pinned: None,
        pinned_mismatch: None,
    };

    // 握手（含 hostkey 挑战挂起）+ 按 host.auth 认证
    let addr = host.addr.clone();
    let port = host.port;
    // 分段超时（正常 15s；指纹弹窗期间放宽），且可被"关标签"取消。
    let mut handle = connect_client(
        crate::ssh::client_config(keepalive_secs),
        (addr.clone(), port),
        handler,
        cancel.as_ref(),
    )
    .await?;
    let ephemeral = if host.auth == AuthMethod::Password {
        password.as_deref()
    } else {
        None
    };
    // 认证阶段同样限时，避免"TCP 通、认证包不回"把命令永久挂住。
    // 同时 select 取消令牌，用户关标签即中止（不必等认证超时）。
    let auth_result = tokio::select! {
        _ = crate::ssh::cancelled(cancel.as_ref()) => {
            return Err(AppError::ECancelled("连接已取消".to_string()));
        }
        result = tokio::time::timeout(
            AUTH_TIMEOUT,
            crate::ssh::auth::authenticate_with_password(&mut handle, &host, ephemeral),
        ) => result.map_err(|_| {
            AppError::ESshAuth(format!(
                "认证超时（{}s）：服务器无响应或凭据交互卡住",
                AUTH_TIMEOUT.as_secs()
            ))
        })?,
    };
    auth_result?;

    // 认证已完成但用户可能刚关掉标签——在建 PTY / 写会话日志之前再确认一次，
    // 避免"取消了却仍然建出一个会话"（关闭流程只能事后补刀）。
    if cancel.as_ref().is_some_and(|token| token.is_cancelled()) {
        return Err(AppError::ECancelled("连接已取消".to_string()));
    }

    // SFTP 独立建连（复用未命中时）：完成握手 + 认证后不再建 PTY / shell、
    // 不发 initCommand、不创建会话日志——SFTP 数据走 ssh::open_subsystem_channel
    // 的 subsystem 通道，终端侧设施对它毫无意义，还会留下空日志文件。
    if kind == SessionKind::Sftp {
        let live = build_sftp_only_session(&host, handle, session_id.clone());
        state.ssh.insert(live).await;
        emit_connected(&state.app, &session_id);
        // 独立 SFTP 会话拥有底层连接，同样可承载该主机的隧道规则。
        let _ = start_tunnels_for_host(state, &host.id).await;
        return Ok(session_id);
    }

    // 认证通过、即将建 PTY 时创建日志文件（握手/认证失败不留下空日志）。
    let logger = create_session_log(host.log || log_sessions, &log_name(&host));

    // PTY + shell + reader/writer：走到这里的一定是 Term 会话
    // （SFTP 的复用派生与独立建连都在上方提前返回）。
    let (session_handle, mut writer_rx) =
        make_session_handle(session_id.clone(), host.id.clone(), kind);
    let ctrl = open_term_channel(
        &mut handle,
        TermChannelArgs {
            session_id: session_id.clone(),
            cols,
            rows,
            app: Some(state.app.clone()),
            data_channel: on_data,
            cancel: session_handle.cancel.clone(),
            encoding: session_handle.encoding.clone(),
            logger,
            wait_replies: false,
            wait_cancel: None,
        },
    )
    .await?;

    // PTY/shell 就绪、connect 返回前写入 initCommand（空串不发）。
    if let Err(e) = send_init_command(&ctrl, &host.init_command).await {
        session_handle.cancel.cancel();
        return Err(e);
    }

    let ctrl2 = ctrl.clone();
    tokio::spawn(async move {
        while let Some(data) = writer_rx.recv().await {
            if ctrl2.send(ChannelCmd::Data(data)).await.is_err() {
                break;
            }
        }
    });

    let live = Arc::new(LiveSession {
        inner: session_handle,
        connected: Arc::new(AtomicBool::new(true)),
        russh: Arc::new(Mutex::new(handle)),
        ctrl,
        owns_connection: true,
        // 身份快照——SFTP 复用 / 隧道绑定据此判断会话是否仍指向当前配置的服务器
        addr: host.addr.clone(),
        port: host.port,
        user: host.user.clone(),
        cols: std::sync::atomic::AtomicU16::new(cols),
        rows: std::sync::atomic::AtomicU16::new(rows),
    });
    state.ssh.insert(live).await;

    emit_connected(&state.app, &session_id);

    // 已启用规则自动拉起；临时主机同样按 host.id 处理（通常没有规则）。
    let _ = start_tunnels_for_host(state, &host.id).await;

    Ok(session_id)
}

/// SFTP 独立建连的会话装配（不建 PTY/shell/日志/writer 任务）。
/// russh handle 仍要包成 `Arc<Mutex<_>>`：SFTP subsystem 通道与隧道都依赖它。
fn build_sftp_only_session(
    host: &Host,
    handle: russh::client::Handle<ClientHandler>,
    session_id: String,
) -> Arc<LiveSession> {
    let (inner, _writer_rx) =
        make_session_handle(session_id.clone(), host.id.clone(), SessionKind::Sftp);
    // SFTP 会话没有终端输入 / 窗口变更需求：与 reuse_sftp_session 一致，
    // 保留一个已断开接收端的控制通道，使 close / resize 的发送失败成为无害 no-op。
    let (ctrl, _ctrl_rx) = mpsc::channel::<ChannelCmd>(1);
    Arc::new(LiveSession {
        inner,
        connected: Arc::new(AtomicBool::new(true)),
        russh: Arc::new(Mutex::new(handle)),
        ctrl,
        owns_connection: true,
        // 独立 SFTP 会话同样记录身份快照（可承载隧道拉起）
        addr: host.addr.clone(),
        port: host.port,
        user: host.user.clone(),
        // 没有 PTY：尺寸置 0（resize 会被 resize_terminal 的 kind 守卫拦下）。
        cols: std::sync::atomic::AtomicU16::new(0),
        rows: std::sync::atomic::AtomicU16::new(0),
    })
}

/// 新建本地终端（Ctrl+T）：ConPTY 跑真实本地 shell，无 host、无隧道/指标。
#[tauri::command]
pub async fn connect_local<R: tauri::Runtime>(
    webview: tauri::Webview<R>,
    state: tauri::State<'_, AppState>,
    cols: u16,
    rows: u16,
    on_data: Option<DataChannelId>,
) -> AppResult<String> {
    // 本地会话没有宿主开关，只受 preferences.logSessions 控制。
    // 日志文件延迟到 PTY/reader 就绪后才创建（工厂由 connect_with_sink
    // 在成功路径上调用），本地 shell 启动失败不再留下 0 字节日志。
    let log_sessions = state.config.read().await.preferences.log_sessions;
    let make_logger: Option<Box<dyn FnOnce() -> Option<SessionLogger> + Send>> = if log_sessions {
        Some(Box::new(|| create_session_log(true, "local")))
    } else {
        None
    };
    let on_data = resolve_data_channel(webview, on_data);
    let session_id = state
        .local
        .connect_with_sink(
            cols,
            rows,
            Box::new(AppEventSink::with_data_channel(state.app.clone(), on_data)),
            make_logger,
            None,
        )
        .await?;

    // 与 SSH connect 一致：会话建立后告知前端 connected。
    emit_connected(&state.app, &session_id);

    Ok(session_id)
}

/// 写终端输入：先查 SSH 会话表，再查本地会话表；会话不存在返回 E_NOT_FOUND。
///
/// `data` 是 base64（STANDARD alphabet）编码的原始字节——IPC 走 JSON 时避免
/// 「数字数组」把大段粘贴放大 4~5 倍。只有命令边界做编解码，writer 链路仍是
/// `Vec<u8>`；非法 base64 回 `E_INVALID_INPUT`，不 panic。
#[tauri::command]
pub async fn write_terminal(
    state: tauri::State<'_, AppState>,
    session_id: String,
    data: String,
) -> AppResult<()> {
    let bytes = events::b64_decode(&data)
        .map_err(|e| AppError::EInvalidInput(format!("终端输入不是合法的 base64: {e}")))?;
    // 先查 SSH 表；查不到再查本地表（两个会话表互不混用）。
    if let Ok(live) = state.ssh.get(&session_id).await {
        return live
            .inner
            .writer
            .send(bytes)
            .await
            .map_err(|_| AppError::ESshChannel("终端写入失败：会话已关闭".to_string()));
    }
    state.local.write(&session_id, bytes).await
}

/// 调整终端窗口；0 行列返回 E_INVALID_INPUT，SFTP 会话没有 PTY 直接视为成功。
#[tauri::command]
pub async fn resize_terminal(
    state: tauri::State<'_, AppState>,
    session_id: String,
    cols: u16,
    rows: u16,
) -> AppResult<()> {
    // 守卫 1：xterm 在容器隐藏时可能算出 0×0，把 0 列发给 PTY 会让远端 shell 行为异常。
    if cols == 0 || rows == 0 {
        return Err(AppError::EInvalidInput("终端尺寸必须大于 0".to_string()));
    }
    if let Ok(live) = state.ssh.get(&session_id).await {
        // 守卫 2：SFTP 复用连接没有 PTY，发 resize 会报 E_SSH_CHANNEL；直接视为成功。
        // 对端控制（Peer）同样有 PTY，必须放行（否则远端窗口永远停在初始尺寸）。
        if !matches!(live.inner.kind, SessionKind::Term | SessionKind::Peer) {
            return Ok(());
        }
        live.cols.store(cols, std::sync::atomic::Ordering::Relaxed);
        live.rows.store(rows, std::sync::atomic::Ordering::Relaxed);
        return live
            .ctrl
            .send(ChannelCmd::Resize(cols as u32, rows as u32))
            .await
            .map_err(|_| AppError::ESshChannel("窗口变更失败：会话已关闭".to_string()));
    }
    state.local.resize(&session_id, cols, rows).await
}

/// 关闭会话：停指标采样、清理 SFTP/隧道，并按主动关闭上报 `closed_by_user`。
#[tauri::command]
pub async fn close_session(state: tauri::State<'_, AppState>, session_id: String) -> AppResult<()> {
    // 停掉该会话的 metrics 采样（本地会话无采样，no-op）
    state.metrics.cancel(&session_id).await;

    if state.ssh.has(&session_id).await {
        // 先关该会话自己的 SFTP 客户端与 channel（复用会话只关自己的 channel）。
        state.sftp.drop_session(&session_id).await;
        let owns_connection = state.ssh.owns_connection(&session_id).await;
        // 并发关闭（重复点关闭 / reader 级联先到）时 close 返回 E_NOT_FOUND——
        // 视为「已关闭」，后续隧道释放与事件上报照常执行（全部幂等），不能用 `?` 早退。
        let host_id = match state.ssh.close(&session_id).await {
            Ok(host_id) => host_id,
            Err(AppError::ENotFound(_)) => {
                // 会话已不在表里：host_id 只用于隧道清理，重查一次配置兜底不了就空着
                String::new()
            }
            Err(e) => return Err(e),
        };
        // 复用终端连接的 SFTP 会话关闭时，不得释放该主机的隧道；
        // 只有拥有底层连接的会话（终端 / 独立 SFTP）才走全量清理。
        if owns_connection && !host_id.is_empty() {
            crate::commands::tunnel::stop_tunnels_for_host(&state, &host_id).await;
        }
        // 主动关闭上报一次 closed_by_user（reader task 已被 cancel，不再补发 lost）。
        emit_closed_by_user(&state.app, &session_id);
        // 级联：拥有者关闭后同 host 的派生会话（共享连接）全部失效，
        // 逐个清理 SFTP 客户端、移除会话记录并上报 closed_by_user，
        // 让前端同步删掉这些 SFTP 标签。ssh::close 内部锁粒度小，逐个调用安全。
        // 已知边界：若 webview 恰在重载窗口期，前端可能收不到这批事件，
        // 残留灰标签由前端启动时的 list_sessions 对账兜底（useAppStore.init）。
        if owns_connection && !host_id.is_empty() {
            for derived in state.ssh.derived_sessions_of(&host_id).await {
                state.sftp.drop_session(&derived).await;
                let _ = state.ssh.close(&derived).await;
                emit_closed_by_user(&state.app, &derived);
            }
        }
        return Ok(());
    }

    // 本地会话：kill 子进程 + 释放 PTY；移除后再次关闭返回 E_NOT_FOUND。
    // shell 自然退出后 reader 已把条目摘表——这里同样按「已关闭」处理，
    // 前端关标签 / 重连前清理旧会话的调用不能因此报错。
    if let Err(AppError::ENotFound(_)) = state.local.close(&session_id).await {
        applog!("[session] 本地会话 {session_id} 已随 shell 退出清理，关闭视为成功");
    }
    // 本地会话同样区分主动关闭与 PTY 断开。
    emit_closed_by_user(&state.app, &session_id);
    Ok(())
}

/// 切换会话编码。只影响此后接收的字节：换掉 reader 的增量
/// Decoder，不动已输出内容、不清屏、不重建通道。
#[tauri::command]
pub async fn set_session_encoding(
    state: tauri::State<'_, AppState>,
    session_id: String,
    encoding: String,
) -> AppResult<String> {
    let new_enc = crate::encoding::SessionEncoding::new(&encoding)
        .ok_or_else(|| AppError::EInvalidInput(format!("未知编码: {encoding}")))?;

    if let Ok(live) = state.ssh.get(&session_id).await {
        let mut guard = live.inner.encoding.lock().await;
        *guard = new_enc;
        return Ok(guard.name.clone());
    }

    // 本地会话同样支持会话级编码切换。
    let session_encoding = state.local.encoding(&session_id).await?;
    let mut guard = session_encoding.lock().await;
    *guard = new_enc;
    Ok(guard.name.clone())
}

/// 重连：关闭旧会话后按原 kind 重新建立。
/// `password` 仅用于密码认证且未保存密码的场景（一次性，不落盘）。
#[tauri::command]
pub async fn reconnect<R: tauri::Runtime>(
    webview: tauri::Webview<R>,
    state: tauri::State<'_, AppState>,
    session_id: String,
    password: Option<String>,
    attempt_id: Option<String>,
    on_data: Option<DataChannelId>,
) -> AppResult<String> {
    let (host_id, kind, cols, rows) = {
        let live = state.ssh.get(&session_id).await?;
        let c = live.cols.load(std::sync::atomic::Ordering::Relaxed);
        let r = live.rows.load(std::sync::atomic::Ordering::Relaxed);
        (live.inner.host_id.clone(), live.inner.kind, c, r)
    };
    // 关闭旧会话（静默：不 emit，重连后的新会话以 connected 事件接替）。
    // 级联：旧会话若拥有底层连接，同 host 的派生会话（共享连接）随之失效，
    // 与 close_session 同一套语义逐个清理：关 SFTP 客户端、移除会话记录并上报
    // closed_by_user，让前端同步删掉这些 SFTP 标签，避免残留指向旧连接的句柄。
    // 派生会话 id 与旧 id 不同、且过滤条件为 !owns_connection，与旧 id 无重叠，
    // 不会重复清理或双发事件；旧会话自身是派生会话时不级联（终端拥有者仍存活）。
    state.metrics.cancel(&session_id).await;
    state.sftp.drop_session(&session_id).await;
    let owns_connection = state.ssh.owns_connection(&session_id).await;
    // 只有"拥有底层连接"的会话被替换时才停掉该主机的 -L 监听，再由重连后的
    // connect 重新拉起。隧道 accept 循环在启动时捕获了 session_id；旧会话删除后它
    // 仍会接受本地连接，但 open_direct_tcpip(旧 id) 必然 E_NOT_FOUND → 每个连接被
    // 静默丢弃，而 is_active 仍为真导致 start_tunnels_for_host 跳过重建
    // （表现为"规则活着但转发全废"）。
    // 派生 SFTP 会话（不拥有连接）重连时终端连接仍活着，监听必须继续工作，不能停。
    if owns_connection {
        crate::commands::tunnel::stop_tunnels_for_host(&state, &host_id).await;
    }
    let _ = state.ssh.close(&session_id).await;
    if owns_connection {
        for derived in state.ssh.derived_sessions_of(&host_id).await {
            state.sftp.drop_session(&derived).await;
            let _ = state.ssh.close(&derived).await;
            emit_closed_by_user(&state.app, &derived);
        }
    }
    // 沿用上次尺寸；若从未 resize 过（0 值）退回默认尺寸（与前端建连一致）。
    // 隧道已在上方释放：connect_inner 的 start_tunnels_for_host 会按 on=true 重建监听。
    let (cols, rows) = if cols == 0 || rows == 0 {
        (DEFAULT_TERM_COLS, DEFAULT_TERM_ROWS)
    } else {
        (cols, rows)
    };
    let host = {
        let c = state.config.read().await;
        c.hosts
            .iter()
            .find(|h| h.id == host_id)
            .cloned()
            .ok_or_else(|| AppError::ENotFound(format!("主机 {host_id} 不存在")))?
    };
    let on_data = resolve_data_channel(webview, on_data);
    connect_inner(
        &state, host, cols, rows, kind, password, attempt_id, on_data,
    )
    .await
}

/// 取消进行中的连接尝试：前端关闭仍在 connecting 的标签时调用。
/// 返回是否真的有在途连接被取消（不存在时为 false，前端可忽略）。
#[tauri::command]
pub async fn cancel_connect(
    state: tauri::State<'_, AppState>,
    attempt_id: String,
) -> AppResult<bool> {
    Ok(state.cancel_connect_attempt(&attempt_id).await)
}

/// 合并 SSH 与本地会话列表；本地会话 hostId 为空、kind=local。
#[tauri::command]
pub async fn list_sessions(
    state: tauri::State<'_, AppState>,
) -> AppResult<Vec<crate::models::Session>> {
    // SSH 与本地会话合并；本地会话 hostId=""、kind="local"。
    let mut sessions = state.ssh.list().await;
    sessions.extend(state.local.list().await);
    Ok(sessions)
}
