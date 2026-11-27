//! 局域网对等层命令（前端唯一入口）。
//!
//! 约定：所有命令返回 `AppResult<_>`，失败带 `E_PEER_*` 错误码；
//! 可见性/端口的**持久化**由偏好（hosts.toml）承担，本层只负责运行态。

use tauri::State;

use crate::error::{AppError, AppResult};
use crate::models::Preferences;
use crate::peer::{self, PeerConfig, PeerStatus};
use crate::state::AppState;

/// 组装状态：设备名偏好为空时回退主机名。
fn build_status(
    prefs: &Preferences,
    svc: &peer::PeerService,
    fingerprint: String,
    peer_count: usize,
) -> PeerStatus {
    let device_name = if prefs.lan_device_name.trim().is_empty() {
        std::env::var("COMPUTERNAME").unwrap_or_else(|_| "Ruthenium".to_string())
    } else {
        prefs.lan_device_name.trim().to_string()
    };
    PeerStatus {
        listening: svc.port().is_some(),
        always_on: svc.is_always_on(),
        window_left_sec: svc.window_left_sec(),
        port: svc.port(),
        pair_port: svc.pair_port(),
        device_name,
        fingerprint,
        share_root: prefs.peer_share_root.clone(),
        peer_count,
    }
}

/// 当前可见性/监听状态（设置页与设备页共用）。
#[tauri::command]
pub async fn peer_status(state: State<'_, AppState>) -> AppResult<PeerStatus> {
    let fingerprint = state.peer.ensure_fingerprint().await;
    let prefs = state.config.read().await.preferences.clone();
    let count = state.peer.peer_count().await;
    Ok(build_status(&prefs, &state.peer, fingerprint, count))
}

/// 常开开关：写偏好 + 启动/停止监听。
#[tauri::command]
pub async fn peer_set_visible(state: State<'_, AppState>, visible: bool) -> AppResult<PeerStatus> {
    let prefs = state.config.read().await.preferences.clone();
    let cfg = PeerConfig::from_prefs(&prefs);
    if visible {
        // 先启动，成功后再写偏好：启动失败不会留下“配置已开启但未监听”的状态。
        state.peer.start(&cfg, None).await?;
        if let Err(e) = state
            .mutate_config(|c| {
                c.preferences.lan_visible = true;
            })
            .await
        {
            state.peer.stop().await;
            return Err(e);
        }
    } else {
        state
            .mutate_config(|c| {
                c.preferences.lan_visible = false;
            })
            .await?;
        state.peer.stop().await;
    }
    let fingerprint = state.peer.ensure_fingerprint().await;
    let count = state.peer.peer_count().await;
    Ok(build_status(&prefs, &state.peer, fingerprint, count))
}

/// 打开一次性可发现窗口（配对/刷新设备时自动调用；默认 5 分钟）。
#[tauri::command]
pub async fn peer_open_window(
    state: State<'_, AppState>,
    minutes: Option<u8>,
) -> AppResult<PeerStatus> {
    let prefs = state.config.read().await.preferences.clone();
    let cfg = PeerConfig::from_prefs(&prefs);
    let minutes = minutes.unwrap_or(peer::DEFAULT_WINDOW_MINUTES);
    // 常开开关打开时不安排窗口：否则 5 分钟后会把手动开启的常开监听关掉
    let window = if prefs.lan_visible {
        None
    } else {
        Some(minutes)
    };
    state.peer.start(&cfg, window).await?;
    let fingerprint = state.peer.ensure_fingerprint().await;
    let count = state.peer.peer_count().await;
    Ok(build_status(&prefs, &state.peer, fingerprint, count))
}

/// 手动关闭可发现窗口；若常开开关打开则保持监听（仅清掉窗口状态）。
#[tauri::command]
pub async fn peer_close_window(state: State<'_, AppState>) -> AppResult<PeerStatus> {
    let prefs = state.config.read().await.preferences.clone();
    if !prefs.lan_visible {
        state.peer.stop().await;
    }
    let fingerprint = state.peer.ensure_fingerprint().await;
    let count = state.peer.peer_count().await;
    Ok(build_status(&prefs, &state.peer, fingerprint, count))
}

/// 已配对设备列表（`peers.toml`，从运行态快照返回）。
#[tauri::command]
pub async fn peer_list(state: State<'_, AppState>) -> AppResult<Vec<peer::peers::PeerRecord>> {
    Ok(state.peer.list_peers().await)
}

/// 发现设备（未监听时自动打开 5 分钟窗口）。
#[tauri::command]
pub async fn peer_discover(
    state: State<'_, AppState>,
) -> AppResult<Vec<peer::discovery::DiscoveredDevice>> {
    ensure_window(&state).await?;
    Ok(state.peer.discovered().await)
}

/// 计算配对目标：手填 `addr` 优先；否则按 `deviceId` 在发现列表里找 **配对地址**。
///
/// 发现报文的 `addr` 是 SSH 连接地址（27410+），与配对端口（28410+）不同；
/// 两者混用会让“从设备列表点配对”连到 SSH 监听、握手直接失败。
fn resolve_pair_target(
    addr: Option<&str>,
    device_id: Option<&str>,
    devices: &[peer::discovery::DiscoveredDevice],
) -> AppResult<String> {
    if let Some(a) = addr.map(str::trim).filter(|a| !a.is_empty()) {
        return Ok(a.to_string());
    }
    if let Some(id) = device_id {
        let d = devices.iter().find(|d| d.device_id == id).ok_or_else(|| {
            crate::error::AppError::EPeerOffline("设备已不在发现列表".to_string())
        })?;
        if !d.pairing {
            return Err(crate::error::AppError::EPeerOffline(
                "对方未开启配对（配对端口不可用）".to_string(),
            ));
        }
        return Ok(d.pair_addr.clone());
    }
    Err(crate::error::AppError::EInvalidInput(
        "缺少配对目标（deviceId 或 addr）".to_string(),
    ))
}

/// 发起配对：目标可以是已发现设备的 deviceId，或手填 `IP:端口`。
#[tauri::command]
pub async fn peer_pair_start(
    state: State<'_, AppState>,
    device_id: Option<String>,
    addr: Option<String>,
) -> AppResult<String> {
    ensure_window(&state).await?;
    let devices = state.peer.discovered().await;
    // 手填地址（含主机名）来源不可信 → 强制数字比对；发现列表 → 一键允许。
    let manual = addr
        .as_deref()
        .map(str::trim)
        .is_some_and(|a| !a.is_empty());
    let target = resolve_pair_target(addr.as_deref(), device_id.as_deref(), &devices)?;
    state.peer.pair_start(&target, manual).await
}

/// 数字比较结果（一致/不一致）+ 六项授权（仅被请求方勾选生效）。
/// 参数较多是 Tauri 命令的线上契约（与前端 `peerPairConfirm` 一一对应）。
#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub async fn peer_pair_confirm(
    state: State<'_, AppState>,
    pairing_id: String,
    agree: bool,
    grant_files: Option<bool>,
    grant_control: Option<bool>,
    grant_config: Option<bool>,
    grant_screen: Option<bool>,
    grant_clipboard: Option<bool>,
    grant_audio: Option<bool>,
) -> AppResult<bool> {
    let grants = peer::pairing::Grants {
        files: grant_files.unwrap_or(false),
        control: grant_control.unwrap_or(false),
        config: grant_config.unwrap_or(false),
        screen: grant_screen.unwrap_or(false),
        clipboard: grant_clipboard.unwrap_or(false),
        audio: grant_audio.unwrap_or(false),
    };
    Ok(state.peer.pair_decide(&pairing_id, agree, grants).await)
}

/// 取消/关闭配对弹窗。
#[tauri::command]
pub async fn peer_pair_cancel(state: State<'_, AppState>, pairing_id: String) -> AppResult<bool> {
    Ok(state.peer.pair_cancel(&pairing_id).await)
}

/// 解除配对（删记录；该设备的后续连接会被拒绝）。
#[tauri::command]
pub async fn peer_remove(state: State<'_, AppState>, peer_id: String) -> AppResult<bool> {
    state.peer.remove_peer(&peer_id).await
}

/// 更新本机对某设备的授权。
// 参数较多是 Tauri 命令的线上契约（与前端 updatePeerGrants 一一对应）。
#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub async fn peer_update_grants(
    state: State<'_, AppState>,
    peer_id: String,
    grant_files: bool,
    grant_control: bool,
    grant_config: bool,
    grant_screen: bool,
    grant_clipboard: bool,
    grant_audio: bool,
) -> AppResult<Option<peer::peers::PeerRecord>> {
    state
        .peer
        .update_grants(
            &peer_id,
            peer::pairing::Grants {
                files: grant_files,
                control: grant_control,
                config: grant_config,
                screen: grant_screen,
                clipboard: grant_clipboard,
                audio: grant_audio,
            },
        )
        .await
}

/// 刷新某设备「对方允许本机」的授权快照（按需连接 ruthenium-info）。
#[tauri::command]
pub async fn peer_refresh_grants(
    state: State<'_, AppState>,
    peer_id: String,
) -> AppResult<Option<peer::peers::PeerRecord>> {
    peer::control::refresh_peer_grants(&state, &peer_id).await
}

/// 打开对端文件互传会话（返回 SFTP sessionId；前端据此开标签）。
#[tauri::command]
pub async fn peer_open_sftp(state: State<'_, AppState>, peer_id: String) -> AppResult<String> {
    peer::client::open_peer_sftp(&state, &peer_id).await
}

/// 推送本机配置到对端收件箱（返回写入字节数）。
#[tauri::command]
pub async fn peer_send_config(state: State<'_, AppState>, peer_id: String) -> AppResult<u64> {
    peer::client::send_config(&state, &peer_id).await
}

/// 配置收件箱列表（含摘要；不自动导入）。
#[tauri::command]
pub async fn peer_inbox() -> AppResult<Vec<peer::inbox::InboxItem>> {
    tauri::async_runtime::spawn_blocking(peer::inbox::list)
        .await
        .map_err(|e| crate::error::AppError::EConfigIo(format!("收件箱任务失败: {e}")))?
}

/// 导入收件箱条目（二次确认后调用）：校验 → 原子写 → 写前备份 → 清理失效凭据。
#[tauri::command]
pub async fn peer_import_inbox(
    state: State<'_, AppState>,
    item_id: String,
) -> AppResult<crate::models::Config> {
    let text = {
        let id = item_id.clone();
        tauri::async_runtime::spawn_blocking(move || peer::inbox::read_text(&id))
            .await
            .map_err(|e| crate::error::AppError::EConfigIo(format!("收件箱任务失败: {e}")))??
    };
    let config = crate::commands::config::apply_backup_text(&state, &text).await?;
    let id = item_id.clone();
    let _ = tauri::async_runtime::spawn_blocking(move || peer::inbox::delete(&id)).await;
    applog!(
        "[peer] 已导入收件箱条目 {item_id}（{} 台主机）",
        config.hosts.len()
    );
    Ok(config)
}

/// 忽略（删除）收件箱条目。
#[tauri::command]
pub async fn peer_delete_inbox(item_id: String) -> AppResult<bool> {
    tauri::async_runtime::spawn_blocking(move || peer::inbox::delete(&item_id))
        .await
        .map_err(|e| crate::error::AppError::EConfigIo(format!("收件箱任务失败: {e}")))?
}

/// 打开对端终端（远程控制）：本机 Hello 前置 → 钉扎连接 → 能力预检 → pty/shell。
#[tauri::command]
pub async fn peer_open_terminal(
    state: State<'_, AppState>,
    peer_id: String,
    cols: u16,
    rows: u16,
    attempt_id: Option<String>,
) -> AppResult<String> {
    peer::control::open_terminal(&state, &peer_id, cols, rows, attempt_id).await
}

/// 受控端：确认 / 拒绝控制请求（30s 内有效）。
#[tauri::command]
pub async fn peer_control_consent(
    state: State<'_, AppState>,
    consent_id: String,
    allow: bool,
) -> AppResult<bool> {
    Ok(state.peer.control_consent(&consent_id, allow).await)
}

/// 当前受控会话（被控横幅）。
#[tauri::command]
pub async fn peer_control_sessions(
    state: State<'_, AppState>,
) -> AppResult<Vec<peer::control::ControlSessionView>> {
    Ok(state.peer.control_sessions().await)
}

/// 立即断开某条受控会话。
#[tauri::command]
pub async fn peer_kick_session(state: State<'_, AppState>, session_id: String) -> AppResult<bool> {
    Ok(state.peer.kick_control(&session_id).await)
}

/* ---------- 远程桌面（只看） ---------- */

/// 把可选的 channel id 解析到指定 webview；`None` 时返回丢弃式 Channel
/// （连接与帧解析照常运行，仅不推送前端——旧调用方/测试不传 onFrame 也能工作）。
/// 与 session.rs 的 resolve_data_channel 同一模式；不复用是因为泛型 TSend 不同。
fn frame_channel<T: serde::Serialize + Clone, R: tauri::Runtime>(
    webview: tauri::Webview<R>,
    on_frame: Option<tauri::ipc::JavaScriptChannelId>,
) -> tauri::ipc::Channel<T> {
    match on_frame {
        Some(id) => id.channel_on(webview),
        None => tauri::ipc::Channel::new(|_msg| Ok(())),
    }
}

/// 打开对端屏幕（只看）：帧经 ipc Channel 推送；返回会话 id。
///
/// `on_frame` 可选——不传时连接/授权/解析照常，仅不推送（测试与不传该参数的调用方兼容）。
#[tauri::command]
pub async fn peer_open_screen<R: tauri::Runtime>(
    webview: tauri::Webview<R>,
    state: State<'_, AppState>,
    peer_id: String,
    quality: u8,
    input_requested: Option<bool>,
    on_frame: Option<tauri::ipc::JavaScriptChannelId>,
) -> AppResult<String> {
    let on_frame = frame_channel(webview, on_frame);
    peer::rd::open_viewer(
        &state,
        &peer_id,
        quality,
        input_requested.unwrap_or(false),
        on_frame,
    )
    .await
}

/// 调整画质 / 切换显示器（`cmd` 形如 `{"cmd":"quality","q":65}`）。
#[tauri::command]
pub async fn peer_screen_command(
    state: State<'_, AppState>,
    session_id: String,
    cmd: serde_json::Value,
) -> AppResult<bool> {
    Ok(state.screen.command(&session_id, &cmd).await)
}

/// 关闭屏幕查看（断开连接）。
#[tauri::command]
pub async fn peer_close_screen(state: State<'_, AppState>, session_id: String) -> AppResult<bool> {
    Ok(state.screen.close(&session_id).await)
}

/// 受查看端：确认 / 拒绝屏幕查看请求。
#[tauri::command]
pub async fn peer_screen_consent(
    state: State<'_, AppState>,
    consent_id: String,
    allow: bool,
    allow_input: Option<bool>,
) -> AppResult<bool> {
    Ok(state
        .peer
        .screen_consent(&consent_id, allow, allow_input.unwrap_or(false))
        .await)
}

/// 当前「正在被查看」的会话（横幅）。
#[tauri::command]
pub async fn peer_screen_sessions(
    state: State<'_, AppState>,
) -> AppResult<Vec<peer::rd::ViewSessionView>> {
    Ok(state.peer.screen_sessions().await)
}

/// 立即断开某条屏幕查看。
#[tauri::command]
pub async fn peer_screen_kick(state: State<'_, AppState>, session_id: String) -> AppResult<bool> {
    Ok(state.peer.kick_screen(&session_id).await)
}

/// 编码器能力诊断（设置页「复制编码诊断」用）：枚举 + 激活检查会阻塞，放后台线程。
#[tauri::command]
pub async fn peer_encoder_diagnostics() -> AppResult<peer::h264::EncoderDiagnostics> {
    tauri::async_runtime::spawn_blocking(peer::h264::diagnostics)
        .await
        .map_err(|e| AppError::EInternal(format!("编码器诊断线程失败: {e}")))
}

/* ---------- 远程桌面（操作） ---------- */

/// 为某条屏幕会话打开输入通道（令牌 = 屏幕会话 id）。
#[tauri::command]
pub async fn peer_input_start(
    state: State<'_, AppState>,
    screen_session_id: String,
    token: Option<String>,
) -> AppResult<String> {
    peer::input::open(&state, &screen_session_id, token.as_deref()).await
}

/// 发送一条输入事件（`{"type":"move","x":0.5,"y":0.5}` 等）。
#[tauri::command]
pub async fn peer_input_send(
    state: State<'_, AppState>,
    input_id: String,
    event: serde_json::Value,
) -> AppResult<bool> {
    Ok(state.input.send(&input_id, event).await)
}

/// 关闭输入通道（退出操作模式）。
#[tauri::command]
pub async fn peer_input_stop(state: State<'_, AppState>, input_id: String) -> AppResult<bool> {
    Ok(state.input.close(&input_id).await)
}

/* ---------- 声音播放 ---------- */

/// 为某条屏幕会话打开音频通道（默认关，用户显式开启）。
///
/// `on_frame` 可选（同 peer_open_screen）。
#[tauri::command]
pub async fn peer_audio_start<R: tauri::Runtime>(
    webview: tauri::Webview<R>,
    state: State<'_, AppState>,
    screen_session_id: String,
    token: Option<String>,
    on_frame: Option<tauri::ipc::JavaScriptChannelId>,
) -> AppResult<peer::audio::AudioStart> {
    let on_frame = frame_channel(webview, on_frame);
    peer::audio::open(&state, &screen_session_id, token.as_deref(), on_frame).await
}

/// 关闭音频通道。
#[tauri::command]
pub async fn peer_audio_stop(state: State<'_, AppState>, id: String) -> AppResult<bool> {
    Ok(state.audio.close(&id).await)
}

/// 会话内开关麦克风混音（默认关；被控端懒加载默认录音设备并与系统声混音）。
#[tauri::command]
pub async fn peer_audio_set_mic(
    state: State<'_, AppState>,
    id: String,
    on: bool,
) -> AppResult<bool> {
    Ok(state.audio.set_mic(&id, on).await)
}

/* ---------- 剪贴板同步 ---------- */

/// 为某条屏幕会话打开剪贴板通道（默认关，用户显式开启）。
///
/// 返回体在 `id`/`text` 之外可带富格式 `formats`（旧被控端没有该字段）。
#[tauri::command]
pub async fn peer_clipboard_start(
    state: State<'_, AppState>,
    screen_session_id: String,
    token: Option<String>,
) -> AppResult<peer::clipboard::ClipboardStart> {
    peer::clipboard::open(&state, &screen_session_id, token.as_deref()).await
}

/// 拉取对端剪贴板文本。
#[tauri::command]
pub async fn peer_clipboard_get(state: State<'_, AppState>, id: String) -> AppResult<String> {
    state
        .clipboard
        .get_text(&id)
        .await
        .map_err(AppError::EPeerNotAllowed)
}

/// 把文本写入对端剪贴板。
#[tauri::command]
pub async fn peer_clipboard_set(
    state: State<'_, AppState>,
    id: String,
    text: String,
) -> AppResult<bool> {
    state
        .clipboard
        .set_text(&id, &text)
        .await
        .map_err(AppError::EPeerNotAllowed)
}

/// 拉取对端剪贴板富格式（文本 + HTML/RTF/PNG/DIB；图片为 base64）。
#[tauri::command]
pub async fn peer_clipboard_get_rich(
    state: State<'_, AppState>,
    id: String,
) -> AppResult<peer::clipboard::ClipboardRich> {
    let (content, truncated) = state
        .clipboard
        .get_rich(&id)
        .await
        .map_err(AppError::EPeerNotAllowed)?;
    let mut rich = peer::clipboard::ClipboardRich::from_content(&content);
    rich.truncated = truncated;
    Ok(rich)
}

/// 把富格式写入对端剪贴板；旧被控端只落其中的纯文本。
#[tauri::command]
pub async fn peer_clipboard_set_rich(
    state: State<'_, AppState>,
    id: String,
    content: peer::clipboard::ClipboardRich,
) -> AppResult<bool> {
    let content = content.into_content().map_err(AppError::EInvalidInput)?;
    state
        .clipboard
        .set_rich(&id, &content)
        .await
        .map_err(AppError::EPeerNotAllowed)
}

/// 关闭剪贴板通道。
#[tauri::command]
pub async fn peer_clipboard_stop(state: State<'_, AppState>, id: String) -> AppResult<bool> {
    Ok(state.clipboard.close(&id).await)
}

/// 本机剪贴板文本（查看端按钮用；不经过前端剪贴板权限）。
///
/// 放在阻塞线程执行：`OpenClipboard` 失败会退避重试，且文本可能达到数 MiB，
/// 不能让 Tauri 主线程跟着等。
#[tauri::command]
pub async fn peer_clipboard_local_get() -> AppResult<String> {
    tauri::async_runtime::spawn_blocking(peer::clipboard::local_get)
        .await
        .map_err(|e| AppError::EInternal(format!("读取本机剪贴板线程失败: {e}")))
}

/// 本机剪贴板富格式（查看端按钮用；图片为 base64，不渲染 HTML）。
#[tauri::command]
pub async fn peer_clipboard_local_get_rich() -> AppResult<peer::clipboard::ClipboardRich> {
    tauri::async_runtime::spawn_blocking(peer::clipboard::local_get_rich)
        .await
        .map_err(|e| AppError::EInternal(format!("读取本机剪贴板线程失败: {e}")))
}

/// 本机剪贴板写入的上限（与网络侧同步上限一致）。
/// 该命令无用户确认（WebView 即可调用），至少要把误用/滥用限制在网络侧同一量级。
const LOCAL_CLIPBOARD_MAX: usize = 16 * 1024 * 1024;

/// 写入本机剪贴板（查看端按钮用）。
///
/// 无确认的任意剪贴板写入是 WebView 失守后的高危原语（粘贴劫持）——
/// 至少施加与网络侧同步相同的长度上限，并写入审计日志留痕。
#[tauri::command]
pub async fn peer_clipboard_local_set(text: String) -> AppResult<bool> {
    tauri::async_runtime::spawn_blocking(move || {
        if text.len() > LOCAL_CLIPBOARD_MAX {
            return Err(AppError::EInvalidInput(format!(
                "剪贴板内容过大（{} 字节，上限 {} MiB）",
                text.len(),
                LOCAL_CLIPBOARD_MAX / (1024 * 1024)
            )));
        }
        applog!(
            "[peer] 本机剪贴板写入: {} 字节（IPC peer_clipboard_local_set）",
            text.len()
        );
        Ok(peer::clipboard::local_set(&text))
    })
    .await
    .map_err(|e| AppError::EInternal(format!("写入本机剪贴板线程失败: {e}")))?
}

/// 写入本机剪贴板富格式（查看端按钮用）。
///
/// 与旧命令同样是「无确认的任意剪贴板写入」：沿用网络侧的
/// 单格式 24 MiB / 总量 32 MiB 上限，并记录各格式大小（不记内容）。
#[tauri::command]
pub async fn peer_clipboard_local_set_rich(
    content: peer::clipboard::ClipboardRich,
) -> AppResult<bool> {
    tauri::async_runtime::spawn_blocking(move || {
        let content = content.into_content().map_err(AppError::EInvalidInput)?;
        if content.is_empty() {
            return Err(AppError::EInvalidInput("剪贴板内容为空".to_string()));
        }
        applog!(
            "[peer] 本机剪贴板写入: {}（IPC peer_clipboard_local_set_rich）",
            content.summary()
        );
        Ok(peer::clipboard::local_set_content(&content))
    })
    .await
    .map_err(|e| AppError::EInternal(format!("写入本机剪贴板线程失败: {e}")))?
}

/* ---------- 审计 ---------- */

/// 最近局域网会话记录（新的在前，最多 500 条）。
#[tauri::command]
pub fn peer_audit_recent(limit: Option<usize>) -> AppResult<Vec<peer::audit::AuditEntry>> {
    Ok(peer::audit::recent(limit.unwrap_or(50).clamp(1, 500)))
}

/// 清空局域网会话记录。
#[tauri::command]
pub fn peer_audit_clear() -> AppResult<bool> {
    Ok(peer::audit::clear())
}

/// 发现/配对需要监听：未监听时按需打开 5 分钟窗口（常开已开则不变）。
async fn ensure_window(state: &State<'_, AppState>) -> AppResult<()> {
    if state.peer.port().is_some() {
        // 已监听：顺便补一次配对监听（启动时端口被占用/服务意外退出的恢复路径）。
        state.peer.ensure_pairing().await;
        return Ok(());
    }
    let prefs = state.config.read().await.preferences.clone();
    if prefs.lan_visible {
        let cfg = PeerConfig::from_prefs(&prefs);
        state.peer.start(&cfg, None).await?;
    } else {
        let cfg = PeerConfig::from_prefs(&prefs);
        state
            .peer
            .start(&cfg, Some(peer::DEFAULT_WINDOW_MINUTES))
            .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device() -> peer::discovery::DiscoveredDevice {
        peer::discovery::DiscoveredDevice {
            device_id: "dev-b".to_string(),
            name: "B".to_string(),
            addr: "192.168.1.7:27411".to_string(),
            pair_addr: "192.168.1.7:28411".to_string(),
            fp: "abcd1234".to_string(),
            pairing: true,
            paired: false,
        }
    }

    /// 按 deviceId 配对必须用 pairAddr（配对端口与 SSH 端口不同）。
    #[test]
    fn pair_target_by_device_id_uses_pair_addr() {
        let devices = vec![device()];
        let t = resolve_pair_target(None, Some("dev-b"), &devices).expect("target");
        assert_eq!(t, "192.168.1.7:28411");
    }

    #[test]
    fn pair_target_prefers_manual_addr() {
        let devices = vec![device()];
        let t =
            resolve_pair_target(Some(" 10.0.0.9:28412 "), Some("dev-b"), &devices).expect("target");
        assert_eq!(t, "10.0.0.9:28412");
    }

    #[test]
    fn pair_target_rejects_device_without_pairing() {
        let mut d = device();
        d.pairing = false;
        assert!(resolve_pair_target(None, Some("dev-b"), &[d]).is_err());
    }

    #[test]
    fn pair_target_rejects_unknown_device_and_missing_input() {
        let devices = vec![device()];
        assert!(resolve_pair_target(None, Some("gone"), &devices).is_err());
        assert!(resolve_pair_target(None, None, &devices).is_err());
        assert!(resolve_pair_target(Some("   "), None, &devices).is_err());
    }
}
