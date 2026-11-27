//! 远程桌面音频：WASAPI loopback 事件驱动采集 → Opus 编码 → 独立 SSH 通道。
//!
//! 职责与边界：
//! - 默认只采系统播放（默认渲染设备的 loopback）；查看端可在本次会话内显式开启
//!   麦克风混音（默认关闭，关闭时立即释放麦克风设备）。DRM 保护的受保护流不会被
//!   loopback 捕获（Windows 音频引擎在采集侧就取不到），属预期行为。
//! - 本模块负责采集、下混/重采样、混音、Opus 编码和通道帧封装；授权由
//!   `peer::server` 的 `grant_audio` 控制，通道令牌复用屏幕会话 id。
//!
//! 设计：
//! - **采集**：共享模式事件驱动（`AUDCLNT_STREAMFLAGS_EVENTCALLBACK` +
//!   `SetEventHandle` + `WaitForSingleObject`），不再 5ms 轮询。等待切成 200ms
//!   一片，只为在没有新数据时响应停止信号和默认设备检查。
//! - **格式**：优先让 WASAPI 用 `AUTOCONVERTPCM | SRC_DEFAULT_QUALITY` 自动转成
//!   48kHz/2ch/f32；驱动拒绝时回退 `GetMixFormat`，按 `dwChannelMask` 做多声道
//!   下混（L = FL + 0.707·FC + 0.707·SL/BL，R 对称，LFE 丢弃），非 48kHz 时再
//!   用线性插值重采样。软件处理只在自动转换不可用时启用。
//! - **麦克风**：默认关闭；查看端发控制帧 `{"type":"mic","on":true}` 后，被控端先
//!   弹本机确认（30s 超时默认拒绝），允许后才懒加载默认录音端点（同样事件驱动 +
//!   自动转换），按样本相加混入系统声并软限幅。关闭时立即 drop 采集流（释放设备）；
//!   打开失败（隐私设置/无设备）只重试并继续系统声，不中断通道。
//!   确认结果与实际采集状态通过帧类型 4 回传查看端，被控端横幅可一键停止。
//! - **跟随默认设备**：每 ~1s 比较 `GetDefaultAudioEndpoint(eRender, eConsole)`
//!   的 `IMMDevice::GetId()`；设备变化或流失效时重建（旧 client Stop/Release →
//!   新流 Start），重建失败只重试不退出，`seq` 保持连续。
//!   未用 `IMMNotificationClient`：需要自己实现并注册 COM 回调对象、处理回调
//!   线程与采集线程的同步，复杂度远高于收益；1s 轮询最多让切换晚 1s 生效。
//! - **编码**：Opus 48kHz 立体声、20ms 帧、`Application::Audio`、
//!   `Signal::Auto`、96kbps VBR、complexity 8；不启用 DTX。
//! - **传输**：`subsystem("ruthenium-audio")`；帧格式 `u32 长度 + [kind][payload]`
//!   （1=格式 JSON，2=数据 `[u32 seq][opus]`，3=错误文本，4=麦克风状态 JSON）。
//!   令牌 = 屏幕会话 id。查看端 → 服务端只有裸 JSON 控制帧（4 字节长度 + JSON），
//!   当前支持 `mic`。
//! - **授权**：`grant_audio`；查看端默认关，用户显式开启本次会话；麦克风还要求
//!   被控端每次本机确认（默认拒绝），不随音频授权自动获得。
//! - **安全**：只传 Opus 音频帧；麦克风只在被控端确认且查看端显式开启时采集，
//!   关闭即释放；会话结束/被踢/撤销后通道立即失效。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::Emitter;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_util::sync::CancellationToken;

use crate::error::{AppError, AppResult};
use crate::peer::rd::ScreenHub;
use crate::state::AppState;

pub const SUBSYSTEM: &str = "ruthenium-audio";
const RATE: u32 = 48_000;
const FRAME_MS: u32 = 20;
const FRAME_SAMPLES: usize = RATE as usize * FRAME_MS as usize / 1000; // 960
/// Opus 目标码率（bit/s）。
const OPUS_BITRATE: i32 = 96_000;
/// Opus 编码复杂度（0..=10）：8 在编码质量和 CPU 占用之间取平衡。
const OPUS_COMPLEXITY: u8 = 8;
/// 默认播放设备的轮询间隔。
#[cfg(windows)]
const DEVICE_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);
/// 单次 `WaitForSingleObject` 的超时（毫秒）。
///
/// 采集本身由事件唤醒，不按固定间隔轮询；这个上限只用于在没有新数据时定期
/// 检查停止标志和默认设备，保证设备切换/关闭能及时响应。麦克风开启时改用
/// [`MIC_LOOP_WAIT_MS`]，否则系统声静音时 loopback 不触发事件，麦克风会被
/// 拖到 200ms 才排空一次。
const WAIT_SLICE_MS: u32 = 200;
/// 麦克风开启时系统声采集的等待上限（毫秒）：把麦克风延迟限制在 ~10ms 量级。
const MIC_LOOP_WAIT_MS: u32 = 10;
/// 麦克风单次等待上限（毫秒）：只用来排空已到达的包，不能拖慢系统声采集节奏。
#[cfg(windows)]
const MIC_WAIT_MS: u32 = 5;
/// 麦克风本机确认超时：超时按拒绝处理。
const MIC_CONSENT_TIMEOUT: Duration = Duration::from_secs(30);
/// 同一会话两次麦克风确认弹窗的最小间隔，避免被反复刷屏。
const MIC_PROMPT_COOLDOWN: Duration = Duration::from_secs(5);

/// 打开音频通道的返回（前端据此初始化 WebAudio）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioStart {
    pub id: String,
    pub rate: u32,
    pub channels: u16,
    pub frame_ms: u32,
}

/// 推送给前端的音频事件（ipc Channel）。
#[derive(Debug, Clone, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum AudioFrame {
    Data {
        session_id: String,
        seq: u32,
        /// Opus 包（base64）
        data: String,
    },
    /// 被控端确认/采集状态变化：on 为实际采集状态，state 用于区分拒绝/不可用。
    Mic {
        session_id: String,
        on: bool,
        state: String,
        detail: String,
    },
    Error {
        session_id: String,
        detail: String,
    },
}

/// 麦克风请求弹窗 payload（`peer://mic-request`）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MicRequest {
    pub consent_id: String,
    pub peer_name: String,
    pub session_id: String,
}

/// 麦克风状态事件（`peer://mic-state`）：被控端横幅据此显示/隐藏。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MicStateEvent {
    pub session_id: String,
    pub peer_name: String,
    pub on: bool,
    /// `on` / `off` / `denied` / `unavailable`
    pub state: String,
    pub detail: String,
}

/// 当前正在采集麦克风的会话（横幅挂载时对账用）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MicSessionView {
    pub session_id: String,
    pub peer_name: String,
    pub on: bool,
}

struct HostMicSession {
    peer_name: String,
    /// 查看端请求的开关状态（采集线程的输入）。
    requested: Arc<AtomicBool>,
    /// 麦克风是否真的已打开（采集线程回写，横幅看这个）。
    active: Arc<AtomicBool>,
    /// 本机点了「停止」：本会话内不再接受新的开启请求。
    blocked: Arc<AtomicBool>,
}

/// 被控端麦克风确认与活动会话表。
///
/// 屏幕会话建立音频通道时登记；查看端每次请求开启麦克风都要过一次本机确认
/// （30s 超时默认拒绝），本机「停止」会置 blocked 并立即释放设备。
#[derive(Default, Clone)]
pub struct MicHub {
    consents: Arc<Mutex<HashMap<String, oneshot::Sender<bool>>>>,
    sessions: Arc<Mutex<HashMap<String, HostMicSession>>>,
    /// 同一会话上一次弹窗时间（标准库锁，只在请求线程短暂持有）。
    prompts: Arc<std::sync::Mutex<HashMap<String, Instant>>>,
}

impl MicHub {
    pub fn new() -> Self {
        Self::default()
    }

    /// 是否允许现在弹窗；调用即占用本次配额。
    fn prompt_allowed(&self, peer_id: &str) -> bool {
        let now = Instant::now();
        let mut prompts = self
            .prompts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if prompts
            .get(peer_id)
            .is_some_and(|last| now.duration_since(*last) < MIC_PROMPT_COOLDOWN)
        {
            return false;
        }
        prompts.insert(peer_id.to_string(), now);
        true
    }

    /// 登记一次确认请求，返回 (consent_id, 决策接收端)。
    async fn request(&self) -> (String, oneshot::Receiver<bool>) {
        let consent_id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        self.consents.lock().await.insert(consent_id.clone(), tx);
        (consent_id, rx)
    }

    /// 前端「允许/拒绝」；超时后返回 false。
    pub async fn decide(&self, consent_id: &str, allow: bool) -> bool {
        let Some(tx) = self.consents.lock().await.remove(consent_id) else {
            return false;
        };
        tx.send(allow).is_ok()
    }

    async fn cancel(&self, consent_id: &str) {
        self.consents.lock().await.remove(consent_id);
    }

    async fn register(
        &self,
        session_id: &str,
        peer_name: String,
        requested: Arc<AtomicBool>,
        active: Arc<AtomicBool>,
        blocked: Arc<AtomicBool>,
    ) {
        self.sessions.lock().await.insert(
            session_id.to_string(),
            HostMicSession {
                peer_name,
                requested,
                active,
                blocked,
            },
        );
    }

    async fn unregister(&self, session_id: &str) {
        self.sessions.lock().await.remove(session_id);
    }

    /// 本机一键停止：立即释放麦克风，并阻止本会话再次开启。
    pub async fn stop(&self, session_id: &str) -> bool {
        let sessions = self.sessions.lock().await;
        let Some(session) = sessions.get(session_id) else {
            return false;
        };
        session.blocked.store(true, Ordering::Relaxed);
        session.requested.store(false, Ordering::Relaxed);
        true
    }

    /// 当前实际在采集麦克风的会话（横幅挂载时对账）。
    pub async fn active(&self) -> Vec<MicSessionView> {
        self.sessions
            .lock()
            .await
            .iter()
            .filter(|(_, session)| session.active.load(Ordering::Relaxed))
            .map(|(session_id, session)| MicSessionView {
                session_id: session_id.clone(),
                peer_name: session.peer_name.clone(),
                on: true,
            })
            .collect()
    }
}

struct AudioSession {
    cancel: CancellationToken,
    /// 查看端 → 服务端的控制帧（当前只有麦克风开关）。
    cmd_tx: mpsc::Sender<serde_json::Value>,
}

#[derive(Default, Clone)]
pub struct AudioManager {
    sessions: Arc<Mutex<HashMap<String, AudioSession>>>,
}

impl AudioManager {
    pub fn new() -> Self {
        Self::default()
    }

    async fn insert(&self, id: String, s: AudioSession) {
        self.sessions.lock().await.insert(id, s);
    }

    /// 会话内开关麦克风混音；返回 `false` 表示会话已结束。
    ///
    /// 麦克风默认关，必须由查看端用户显式点按；服务端采集线程收到控制帧后
    /// 才打开默认录音端点，关闭时立即释放设备。
    pub async fn set_mic(&self, id: &str, on: bool) -> bool {
        let tx = {
            let map = self.sessions.lock().await;
            map.get(id).map(|session| session.cmd_tx.clone())
        };
        let Some(tx) = tx else {
            return false;
        };
        tx.send(serde_json::json!({ "type": "mic", "on": on }))
            .await
            .is_ok()
    }

    pub async fn close(&self, id: &str) -> bool {
        match self.sessions.lock().await.remove(id) {
            Some(s) => {
                s.cancel.cancel();
                true
            }
            None => false,
        }
    }
}

/* ---------------- 查看端 ---------------- */

/// 为某条屏幕会话打开音频通道（令牌 = 服务端下发的 auth token）。
pub async fn open(
    state: &AppState,
    screen_session_id: &str,
    token: Option<&str>,
    on_frame: tauri::ipc::Channel<AudioFrame>,
) -> AppResult<AudioStart> {
    let token = token.filter(|s| !s.is_empty()).unwrap_or(screen_session_id);
    let peer_id = state
        .screen
        .peer_id_of(screen_session_id)
        .await
        .ok_or_else(|| AppError::EPeerNotPaired("屏幕会话不存在或已断开".to_string()))?;
    let peer = state
        .peer
        .peer_record(&peer_id)
        .await
        .ok_or_else(|| AppError::EPeerNotPaired("该设备未配对".to_string()))?;

    let identity = state.peer.identity_handle().await?;
    let mut handle = crate::peer::client::connect(state, &peer, "声音播放").await?;
    crate::peer::client::authenticate(&mut handle, &identity).await?;

    let info = crate::peer::control::query_info(&handle, None).await?;
    state.peer.sync_peer_allows(&peer.id, &info).await;
    if !info.grant_audio {
        return Err(AppError::EPeerNotAllowed(format!(
            "{} 未允许声音播放（可能已撤销授权）",
            peer.name
        )));
    }

    let mut channel = handle
        .channel_open_session()
        .await
        .map_err(|e| AppError::ESshChannel(format!("打开音频通道失败: {e}")))?;
    channel
        .request_subsystem(true, SUBSYSTEM)
        .await
        .map_err(|e| AppError::EPeerNotAllowed(format!("对端拒绝音频通道: {e}")))?;
    match tokio::time::timeout(std::time::Duration::from_secs(5), channel.wait()).await {
        Ok(Some(russh::ChannelMsg::Success)) => {}
        Ok(Some(russh::ChannelMsg::Failure)) => {
            return Err(AppError::EPeerNotAllowed(
                "对端拒绝声音播放：未授权或授权已撤销".to_string(),
            ))
        }
        Ok(None) => return Err(AppError::EPeerNotAllowed("对端关闭了音频通道".to_string())),
        Err(_) => return Err(AppError::EPeerNotAllowed("对端未响应音频请求".to_string())),
        Ok(Some(_)) => {}
    }
    let stream = channel.into_stream();
    let (mut rd, mut wr) = tokio::io::split(stream);
    let cancel = CancellationToken::new();
    let id = uuid::Uuid::new_v4().to_string();

    // 握手：只带屏幕会话令牌
    write_json(&mut wr, &serde_json::json!({ "session": token })).await?;
    let first = tokio::time::timeout(std::time::Duration::from_secs(5), read_frame(&mut rd))
        .await
        .map_err(|_| AppError::EPeerNotAllowed("对端未响应音频握手".to_string()))?
        .ok_or_else(|| AppError::EPeerNotAllowed("对端关闭了音频通道".to_string()))?;
    let (rate, channels) = match first.first().copied() {
        Some(1) => {
            let v: serde_json::Value = serde_json::from_slice(&first[1..]).unwrap_or_default();
            let rate = v.get("rate").and_then(|x| x.as_u64()).unwrap_or(48_000) as u32;
            let channels = v.get("channels").and_then(|x| x.as_u64()).unwrap_or(2) as u16;
            (rate, channels)
        }
        Some(3) => {
            let detail = String::from_utf8_lossy(&first[1..]).to_string();
            return Err(AppError::EPeerNotAllowed(format!(
                "对端音频不可用: {detail}"
            )));
        }
        _ => {
            return Err(AppError::EPeerNotAllowed(
                "对端音频握手格式错误".to_string(),
            ))
        }
    };

    // 控制通道：查看端 → 服务端（麦克风开关）。与音频帧方向相反，互不阻塞。
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<serde_json::Value>(8);
    {
        let cancel = cancel.clone();
        let mut wr = wr;
        tokio::spawn(async move {
            loop {
                let msg = tokio::select! {
                    _ = cancel.cancelled() => break,
                    msg = cmd_rx.recv() => match msg {
                        Some(msg) => msg,
                        None => break,
                    },
                };
                if write_client_json(&mut wr, &msg).await.is_err() {
                    break;
                }
            }
            let _ = wr.shutdown().await;
        });
    }

    // 读端：Opus 包 → 前端 Channel；连接/会话结束推 Error
    {
        let cancel = cancel.clone();
        let sid = screen_session_id.to_string();
        tokio::spawn(async move {
            let _keep = handle;
            loop {
                let next = tokio::select! {
                    _ = cancel.cancelled() => None,
                    frame = read_frame(&mut rd) => frame,
                };
                let Some(body) = next else { break };
                match body.first().copied() {
                    Some(2) if body.len() >= 5 => {
                        let seq = u32::from_be_bytes([body[1], body[2], body[3], body[4]]);
                        let data = base64_encode(&body[5..]);
                        let _ = on_frame.send(AudioFrame::Data {
                            session_id: sid.clone(),
                            seq,
                            data,
                        });
                    }
                    Some(3) => {
                        let detail = String::from_utf8_lossy(&body[1..]).to_string();
                        let _ = on_frame.send(AudioFrame::Error {
                            session_id: sid.clone(),
                            detail,
                        });
                        break;
                    }
                    Some(4) => {
                        let v: serde_json::Value =
                            serde_json::from_slice(&body[1..]).unwrap_or_default();
                        let on = v.get("on").and_then(|x| x.as_bool()).unwrap_or(false);
                        let state = v
                            .get("state")
                            .and_then(|x| x.as_str())
                            .unwrap_or("off")
                            .to_string();
                        let detail = v
                            .get("detail")
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .to_string();
                        let _ = on_frame.send(AudioFrame::Mic {
                            session_id: sid.clone(),
                            on,
                            state,
                            detail,
                        });
                    }
                    _ => {}
                }
            }
            if !cancel.is_cancelled() {
                let _ = on_frame.send(AudioFrame::Error {
                    session_id: sid.clone(),
                    detail: "音频连接已断开".to_string(),
                });
            }
        });
    }

    state
        .audio
        .insert(id.clone(), AudioSession { cancel, cmd_tx })
        .await;
    Ok(AudioStart {
        id,
        rate,
        channels,
        frame_ms: FRAME_MS,
    })
}

/* ---------------- 受查看端 ---------------- */

enum Out {
    Fmt { rate: u32, channels: u16 },
    Data { seq: u32, data: Vec<u8> },
    /// 麦克风确认/采集状态；同时回传查看端并驱动被控端横幅。
    MicState {
        on: bool,
        /// `on` / `off` / `denied` / `unavailable`
        state: String,
        detail: String,
    },
    Error(String),
}

/// 受查看端：采集 → 编码 → 通道（屏幕会话结束/被踢/写失败即停止）。
///
/// `grant_check` 每 5s 复核一次授权撤销（只在通道建立时查一次不够，
/// 撤销后已在传的音频会一直流到屏幕会话结束）。
///
/// 麦克风开启前必须经被控端本机确认；确认结果与实际采集状态通过帧类型 4
/// 回传查看端，同时以 `peer://mic-state` 事件驱动被控端横幅。
pub async fn serve(
    hub: Arc<ScreenHub>,
    grant_check: crate::peer::server::GrantCheck,
    mic_hub: Arc<MicHub>,
    app: tauri::AppHandle,
    peer_name: String,
    stream: russh::ChannelStream<russh::server::Msg>,
) -> AppResult<()> {
    let (mut rd, mut wr) = tokio::io::split(stream);
    // 握手：客户端发 {"session": "<屏幕会话 id>"}
    let hello = tokio::time::timeout(std::time::Duration::from_secs(5), read_frame(&mut rd))
        .await
        .map_err(|_| AppError::EPeerNotAllowed("音频握手超时".to_string()))?
        .ok_or_else(|| AppError::EPeerNotAllowed("音频握手失败".to_string()))?;
    let v: serde_json::Value = serde_json::from_slice(&hello).unwrap_or_default();
    let token = v
        .get("session")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    if !hub.has_session(&token).await {
        let _ = write_frame(&mut wr, 3, b"viewing session not found").await;
        return Err(AppError::EPeerNotAllowed(
            "查看会话不存在或已结束".to_string(),
        ));
    }
    let screen_cancel = hub.session_cancel(&token).await.unwrap_or_default();

    let stop = Arc::new(AtomicBool::new(false));
    // requested：查看端请求的开关（采集线程的输入）；active：麦克风实际已打开
    // （采集线程回写，横幅看这个）；blocked：本机点了「停止」。
    let mic_requested = Arc::new(AtomicBool::new(false));
    let mic_active = Arc::new(AtomicBool::new(false));
    let mic_blocked = Arc::new(AtomicBool::new(false));
    let (tx, mut rx) = mpsc::channel::<Out>(32);
    mic_hub
        .register(
            &token,
            peer_name.clone(),
            mic_requested.clone(),
            mic_active.clone(),
            mic_blocked.clone(),
        )
        .await;

    // 控制帧读端：查看端 → 服务端。当前只有麦克风开关；开启必须过本机确认。
    // 查看端断开时停止采集，避免通道结束后还继续占用麦克风/播放设备。
    {
        let mic_hub = mic_hub.clone();
        let app = app.clone();
        let peer_name = peer_name.clone();
        let session_id = token.clone();
        let mic_requested = mic_requested.clone();
        let mic_active = mic_active.clone();
        let mic_blocked = mic_blocked.clone();
        let stop = stop.clone();
        let cancel = screen_cancel.clone();
        let tx = tx.clone();
        let mut rd = rd;
        tokio::spawn(async move {
            loop {
                let frame = tokio::select! {
                    _ = cancel.cancelled() => break,
                    frame = read_frame(&mut rd) => frame,
                };
                let Some(body) = frame else { break };
                let Some(on) = parse_mic_control(&body) else {
                    continue;
                };
                if !on {
                    mic_requested.store(false, Ordering::Relaxed);
                    let _ = tx.try_send(Out::MicState {
                        on: false,
                        state: "off".to_string(),
                        detail: String::new(),
                    });
                    applog!("[peer] 麦克风混音: 关闭");
                    continue;
                }
                if mic_requested.load(Ordering::Relaxed) {
                    // 已在开启/重试中：重复请求只回报当前实际状态。
                    let active = mic_active.load(Ordering::Relaxed);
                    let _ = tx.try_send(Out::MicState {
                        on: active,
                        state: if active { "on" } else { "off" }.to_string(),
                        detail: String::new(),
                    });
                    continue;
                }
                if mic_blocked.load(Ordering::Relaxed) {
                    let _ = tx.try_send(Out::MicState {
                        on: false,
                        state: "denied".to_string(),
                        detail: "本机已停止麦克风".to_string(),
                    });
                    continue;
                }
                if !mic_hub.prompt_allowed(&session_id) {
                    let _ = tx.try_send(Out::MicState {
                        on: false,
                        state: "denied".to_string(),
                        detail: "麦克风请求过于频繁".to_string(),
                    });
                    continue;
                }
                let (consent_id, consent) = mic_hub.request().await;
                let _ = app.emit(
                    crate::events::PEER_MIC_REQUEST,
                    MicRequest {
                        consent_id: consent_id.clone(),
                        peer_name: peer_name.clone(),
                        session_id: session_id.clone(),
                    },
                );
                applog!("[peer] 麦克风请求: {peer_name}，等待本机确认");
                let allowed = tokio::select! {
                    _ = cancel.cancelled() => {
                        mic_hub.cancel(&consent_id).await;
                        break;
                    }
                    result = tokio::time::timeout(MIC_CONSENT_TIMEOUT, consent) => {
                        matches!(result, Ok(Ok(true)))
                    }
                };
                if !allowed {
                    mic_hub.cancel(&consent_id).await;
                    let _ = tx.try_send(Out::MicState {
                        on: false,
                        state: "denied".to_string(),
                        detail: "本机拒绝麦克风采集".to_string(),
                    });
                    applog!("[peer] 麦克风请求被拒绝: {peer_name}");
                    continue;
                }
                if mic_blocked.load(Ordering::Relaxed) {
                    let _ = tx.try_send(Out::MicState {
                        on: false,
                        state: "denied".to_string(),
                        detail: "本机已停止麦克风".to_string(),
                    });
                    continue;
                }
                mic_requested.store(true, Ordering::Relaxed);
                applog!("[peer] 麦克风请求已允许: {peer_name}");
            }
            stop.store(true, Ordering::Relaxed);
        });
    }
    {
        let stop = stop.clone();
        let mic_requested = mic_requested.clone();
        let mic_active = mic_active.clone();
        let tx = tx.clone();
        std::thread::spawn(move || {
            capture_loop(tx, stop, mic_requested, mic_active);
        });
    }
    drop(tx);

    // 授权周期复核（设备删除 / grant_audio 撤销 → 停止发送并结束）
    let mut grant_tick = tokio::time::interval(std::time::Duration::from_secs(5));
    grant_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // interval 的第一次 tick 立即到期：先消费掉，避免建立即断（建立时刚查过授权）
    grant_tick.tick().await;

    let mut revoked = false;
    loop {
        let item = tokio::select! {
            _ = grant_tick.tick() => {
                if !grant_check.still_granted(|p| p.grant_audio).await {
                    revoked = true;
                    applog!("[peer] 音频授权已撤销，停止采集");
                    break;
                }
                continue;
            }
            _ = screen_cancel.cancelled() => break,
            item = rx.recv() => match item {
                Some(i) => i,
                None => break, // 采集线程结束
            },
        };
        let result = tokio::select! {
            _ = screen_cancel.cancelled() => break,
            r = async {
                match item {
                    Out::Fmt { rate, channels } => {
                        let json = serde_json::json!({
                            "rate": rate,
                            "channels": channels,
                            "frameMs": FRAME_MS,
                        });
                        let bytes = serde_json::to_vec(&json).unwrap_or_default();
                        write_frame(&mut wr, 1, &bytes).await
                    }
                    Out::Data { seq, data } => {
                        let mut payload = Vec::with_capacity(4 + data.len());
                        payload.extend_from_slice(&seq.to_be_bytes());
                        payload.extend_from_slice(&data);
                        write_frame(&mut wr, 2, &payload).await
                    }
                    Out::MicState { on, state, detail } => {
                        let _ = app.emit(
                            crate::events::PEER_MIC_STATE,
                            MicStateEvent {
                                session_id: token.clone(),
                                peer_name: peer_name.clone(),
                                on,
                                state: state.clone(),
                                detail: detail.clone(),
                            },
                        );
                        let json = serde_json::json!({
                            "on": on,
                            "state": state,
                            "detail": detail,
                        });
                        let bytes = serde_json::to_vec(&json).unwrap_or_default();
                        write_frame(&mut wr, 4, &bytes).await
                    }
                    Out::Error(detail) => write_frame(&mut wr, 3, detail.as_bytes()).await,
                }
            } => r,
        };
        if result.is_err() {
            break;
        }
    }
    let _ = (&revoked, ()); // revoked 仅作日志语义保留
    stop.store(true, Ordering::Relaxed);
    mic_requested.store(false, Ordering::Relaxed);
    // 采集线程退出时不会再发状态帧，这里无条件补一条 off，避免被控端横幅
    // 在会话消失后残留（前端对不存在的条目会直接忽略）。
    mic_active.store(false, Ordering::Relaxed);
    let _ = app.emit(
        crate::events::PEER_MIC_STATE,
        MicStateEvent {
            session_id: token.clone(),
            peer_name: peer_name.clone(),
            on: false,
            state: "off".to_string(),
            detail: "音频会话已结束".to_string(),
        },
    );
    mic_hub.unregister(&token).await;
    let _ = wr.shutdown().await;
    Ok(())
}

/// 采集线程主循环：loopback 采集 → 48kHz 立体声 f32 → 20ms Opus 帧。
///
/// 设备切换/失效重建在采集器内部消化，这里只在编码成功时递增 `seq`，
/// 因此重建期间 seq 保持连续（只是短暂没有新帧，丢帧可接受）。
///
/// 麦克风由 `mic_requested` 控制：默认不打开；开启后懒加载并按样本混入系统声，
/// 关闭/采集失败时立即释放设备并回写 `mic_active`。打开失败只记一次日志并
/// 2s 重试，不影响系统声通道；实际状态经 `Out::MicState` 回传查看端。
fn capture_loop(
    tx: mpsc::Sender<Out>,
    stop: Arc<AtomicBool>,
    mic_requested: Arc<AtomicBool>,
    mic_active: Arc<AtomicBool>,
) {
    let mut cap = match platform::Loopback::new() {
        Ok(c) => c,
        Err(e) => {
            applog!("[peer] 音频采集初始化失败: {e}");
            let _ = tx.blocking_send(Out::Error(e));
            return;
        }
    };
    let _ = tx.blocking_send(Out::Fmt {
        rate: RATE,
        channels: 2,
    });
    let enc = match opus_encoder() {
        Ok(e) => e,
        Err(e) => {
            let _ = tx.blocking_send(Out::Error(e));
            return;
        }
    };
    let mut pending: Vec<f32> = Vec::with_capacity(FRAME_SAMPLES * 4);
    let mut out = vec![0u8; 4000];
    let mut seq = 0u32;
    let mut mic: Option<platform::MicCapture> = None;
    let mut mic_pending: Vec<f32> = Vec::with_capacity(FRAME_SAMPLES * 2);
    let mut mic_next_try = Instant::now();
    let mut mic_error_logged = false;
    let mut mic_status: Option<&'static str> = Some("off");
    while !stop.load(Ordering::Relaxed) {
        let want_mic = mic_requested.load(Ordering::Relaxed);
        if !want_mic {
            if mic.take().is_some() {
                applog!("[peer] 麦克风混音已关闭，释放麦克风设备");
                mic_error_logged = false;
                mic_active.store(false, Ordering::Relaxed);
                report_mic(&tx, &mut mic_status, false, "off", "");
            } else {
                // 下次开启失败时仍要能重新上报 unavailable，不能被旧状态吞掉。
                mic_status = Some("off");
            }
            mic_pending.clear();
        } else if mic.is_none() && Instant::now() >= mic_next_try {
            match platform::MicCapture::new() {
                Ok(capture) => {
                    applog!("[peer] 麦克风采集已开启（混入系统声）");
                    mic = Some(capture);
                    mic_error_logged = false;
                    mic_active.store(true, Ordering::Relaxed);
                    report_mic(&tx, &mut mic_status, true, "on", "");
                }
                Err(e) => {
                    if !mic_error_logged {
                        applog!("[peer] 麦克风不可用（检查 Windows 隐私设置/设备）: {e}");
                        mic_error_logged = true;
                    }
                    mic_next_try = Instant::now() + Duration::from_secs(2);
                    mic_active.store(false, Ordering::Relaxed);
                    report_mic(&tx, &mut mic_status, false, "unavailable", &e);
                }
            }
        }
        // 麦克风开启时缩短等待，避免系统声静音时麦克风被拖到 200ms 才排空。
        let wait_ms = if mic.is_some() {
            MIC_LOOP_WAIT_MS
        } else {
            WAIT_SLICE_MS
        };
        if let Err(e) = cap.read_into(&mut pending, wait_ms) {
            // 只有 Win32 事件等待失败这类不可恢复错误才会到这里；
            // 设备失效/切换都在采集器内部重建。
            applog!("[peer] 音频采集中断: {e}");
            mic_active.store(false, Ordering::Relaxed);
            let _ = tx.blocking_send(Out::Error(e));
            break;
        }
        if mic.is_some() {
            mic_pending.clear();
            let read = mic
                .as_mut()
                .map(|capture| capture.read_into(&mut mic_pending));
            match read {
                Some(Ok(())) if !mic_pending.is_empty() => {
                    dsp::mix_into(&mut pending, &mic_pending);
                }
                Some(Ok(())) => {}
                Some(Err(e)) => {
                    applog!("[peer] 麦克风采集中断，2s 后重试: {e}");
                    mic = None;
                    mic_next_try = Instant::now() + Duration::from_secs(2);
                    mic_active.store(false, Ordering::Relaxed);
                    report_mic(&tx, &mut mic_status, false, "unavailable", &e);
                }
                None => {}
            }
        }
        while pending.len() >= FRAME_SAMPLES * 2 {
            let frame: Vec<f32> = pending.drain(..FRAME_SAMPLES * 2).collect();
            match enc.encode_float(&frame, &mut out) {
                Ok(n) if n > 0 => {
                    if tx
                        .blocking_send(Out::Data {
                            seq,
                            data: out[..n].to_vec(),
                        })
                        .is_err()
                    {
                        mic_active.store(false, Ordering::Relaxed);
                        return;
                    }
                    seq = seq.wrapping_add(1);
                }
                Ok(_) => {}
                Err(e) => {
                    applog!("[peer] Opus 编码失败: {e:?}");
                    mic_active.store(false, Ordering::Relaxed);
                    let _ = tx.blocking_send(Out::Error(format!("Opus 编码失败: {e:?}")));
                    return;
                }
            }
        }
    }
    // 线程退出（停止/会话结束）：`mic` 随作用域释放，这里只回落对外的 active 标记。
    mic_active.store(false, Ordering::Relaxed);
}

/// 只在状态变化时发一条麦克风状态，避免每帧刷控制帧。
fn report_mic(
    tx: &mpsc::Sender<Out>,
    last: &mut Option<&'static str>,
    on: bool,
    state: &'static str,
    detail: &str,
) {
    if last.as_deref() == Some(state) {
        return;
    }
    *last = Some(state);
    if tx
        .try_send(Out::MicState {
            on,
            state: state.to_string(),
            detail: detail.to_string(),
        })
        .is_err()
    {
        applog!("[peer] 麦克风状态上报失败（通道已关闭）");
    }
}

/// 解析查看端控制帧：`{"type":"mic","on":true}`；其它或非法帧返回 `None`。
fn parse_mic_control(body: &[u8]) -> Option<bool> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    if v.get("type").and_then(serde_json::Value::as_str) != Some("mic") {
        return None;
    }
    Some(
        v.get("on")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
    )
}

/// 创建 Opus 编码器：48kHz 立体声、Audio 应用、96kbps VBR、复杂度 8。
///
/// 不启用 DTX：DTX 主要作用于 LPC/CELT 层，`Application::Audio` 下收益很低，
/// 还会让静音段的包间隔不规律；VBR 本身已能把静音压到很小的包。
fn opus_encoder() -> Result<audiopus::coder::Encoder, String> {
    use audiopus::{Application, Bitrate, Channels, SampleRate, Signal};
    let mut enc =
        audiopus::coder::Encoder::new(SampleRate::Hz48000, Channels::Stereo, Application::Audio)
            .map_err(|e| format!("创建 Opus 编码器失败: {e:?}"))?;
    enc.set_bitrate(Bitrate::BitsPerSecond(OPUS_BITRATE))
        .map_err(|e| format!("设置 Opus 码率失败: {e:?}"))?;
    enc.set_vbr(true)
        .map_err(|e| format!("设置 Opus VBR 失败: {e:?}"))?;
    enc.set_complexity(OPUS_COMPLEXITY)
        .map_err(|e| format!("设置 Opus 复杂度失败: {e:?}"))?;
    enc.set_signal(Signal::Auto)
        .map_err(|e| format!("设置 Opus 信号类型失败: {e:?}"))?;
    Ok(enc)
}

/* ---------------- 帧协议 ---------------- */

async fn write_frame<W: AsyncWriteExt + Unpin>(
    wr: &mut W,
    kind: u8,
    payload: &[u8],
) -> std::io::Result<()> {
    let mut framed = Vec::with_capacity(5 + payload.len());
    framed.extend_from_slice(&((payload.len() + 1) as u32).to_be_bytes());
    framed.push(kind);
    framed.extend_from_slice(payload);
    tokio::time::timeout(std::time::Duration::from_millis(500), wr.write_all(&framed))
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "audio write timeout"))?
}

async fn write_json<W: AsyncWriteExt + Unpin>(wr: &mut W, v: &serde_json::Value) -> AppResult<()> {
    let bytes = serde_json::to_vec(v)
        .map_err(|e| AppError::EInvalidInput(format!("音频握手序列化失败: {e}")))?;
    let mut framed = Vec::with_capacity(4 + bytes.len());
    framed.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    framed.extend_from_slice(&bytes);
    wr.write_all(&framed)
        .await
        .map_err(|e| AppError::ESshChannel(format!("音频握手发送失败: {e}")))?;
    Ok(())
}

/// 查看端 → 服务端控制帧：4 字节大端长度 + JSON（与握手同格式）。
async fn write_client_json<W: AsyncWriteExt + Unpin>(
    wr: &mut W,
    v: &serde_json::Value,
) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(v)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let mut framed = Vec::with_capacity(4 + bytes.len());
    framed.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    framed.extend_from_slice(&bytes);
    tokio::time::timeout(std::time::Duration::from_millis(500), wr.write_all(&framed))
        .await
        .map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::TimedOut, "audio control write timeout")
        })?
}

async fn read_frame<R: AsyncReadExt + Unpin>(rd: &mut R) -> Option<Vec<u8>> {
    let mut len = [0u8; 4];
    rd.read_exact(&mut len).await.ok()?;
    let n = u32::from_be_bytes(len) as usize;
    if n == 0 || n > 64 * 1024 {
        return None;
    }
    let mut buf = vec![0u8; n];
    rd.read_exact(&mut buf).await.ok()?;
    Some(buf)
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    STANDARD.encode(bytes)
}

/* ---------------- 采集纯逻辑（下混/限幅/重采样） ---------------- */

/// 采集链路中不依赖 Win32 的纯逻辑。
///
/// 平台模块只负责把 WASAPI 原始数据解码成 f32 再交给这里，所以下混、限幅、
/// 重采样和格式换算都能在没有真实音频设备的单测里验证。
#[cfg(windows)]
mod dsp {
    use super::RATE;

    /// Windows `SPEAKER_*` 声道掩码位（与 `ksmedia.h` 一致）。
    const SPEAKER_FRONT_LEFT: u32 = 0x0001;
    const SPEAKER_FRONT_RIGHT: u32 = 0x0002;
    const SPEAKER_FRONT_CENTER: u32 = 0x0004;
    const SPEAKER_LOW_FREQUENCY: u32 = 0x0008;
    const SPEAKER_BACK_LEFT: u32 = 0x0010;
    const SPEAKER_BACK_RIGHT: u32 = 0x0020;
    const SPEAKER_FRONT_LEFT_OF_CENTER: u32 = 0x0040;
    const SPEAKER_FRONT_RIGHT_OF_CENTER: u32 = 0x0080;
    const SPEAKER_BACK_CENTER: u32 = 0x0100;
    const SPEAKER_SIDE_LEFT: u32 = 0x0200;
    const SPEAKER_SIDE_RIGHT: u32 = 0x0400;

    /// 中置/环绕声道下混到单侧的增益（-3dB，保持能量）。
    const MIX_SQRT_HALF: f32 = std::f32::consts::FRAC_1_SQRT_2;

    /// 采集样本类型；共享模式混音格式只会出现 f32 或 16/32 位 PCM。
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum SampleKind {
        F32,
        I16,
        I32,
    }

    impl SampleKind {
        /// 一个样本占用的字节数。
        pub(super) fn bytes(self) -> usize {
            match self {
                Self::F32 | Self::I32 => 4,
                Self::I16 => 2,
            }
        }

        /// 从原始缓冲解码一个样本（PCM 归一化到 [-1, 1)）。
        ///
        /// 安全前提：`base` 指向的包数据至少还有 `offset + bytes()` 字节可读。
        pub(super) unsafe fn decode(self, base: *const u8, offset: usize) -> f32 {
            unsafe {
                match self {
                    Self::F32 => std::ptr::read_unaligned(base.add(offset) as *const f32),
                    Self::I16 => {
                        std::ptr::read_unaligned(base.add(offset) as *const i16) as f32 / 32_768.0
                    }
                    Self::I32 => {
                        std::ptr::read_unaligned(base.add(offset) as *const i32) as f32
                            / 2_147_483_648.0
                    }
                }
            }
        }
    }

    /// 把格式标签与位深换算成样本类型。
    ///
    /// `WAVE_FORMAT_EXTENSIBLE` 的 SubFormat 由调用方先归一成 PCM/浮点标签。
    pub(super) fn sample_kind(tag: u16, bits: u16) -> Result<SampleKind, String> {
        match (tag, bits) {
            (3, 32) => Ok(SampleKind::F32), // WAVE_FORMAT_IEEE_FLOAT
            (1, 16) => Ok(SampleKind::I16), // WAVE_FORMAT_PCM
            (1, 32) => Ok(SampleKind::I32),
            _ => Err(format!("不支持的混音格式 tag={tag} bits={bits}")),
        }
    }

    /// 一路采集流的原生格式（WASAPI 自动转换结果或 `GetMixFormat` 解析结果）。
    #[derive(Clone, Copy)]
    pub(super) struct CaptureFormat {
        pub(super) rate: u32,
        pub(super) channels: usize,
        pub(super) block_align: usize,
        pub(super) kind: SampleKind,
        pub(super) channel_mask: u32,
    }

    impl CaptureFormat {
        /// 自动转换成功时的固定格式：48kHz/2ch/f32。
        pub(super) fn converted() -> Self {
            Self {
                rate: RATE,
                channels: 2,
                block_align: 2 * 4,
                kind: SampleKind::F32,
                channel_mask: SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT,
            }
        }

        /// 日志用的简短描述。
        pub(super) fn describe(&self) -> String {
            format!(
                "{}Hz {}ch {}bit 掩码 0x{:X}",
                self.rate,
                self.channels,
                self.kind.bytes() * 8,
                self.channel_mask
            )
        }
    }

    /// 声道掩码缺失时按声道数推断标准布局；返回 0 表示无法推断。
    pub(super) fn default_channel_mask(channels: usize) -> u32 {
        match channels {
            1 => SPEAKER_FRONT_CENTER,
            2 => SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT,
            4 => SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT | SPEAKER_BACK_LEFT | SPEAKER_BACK_RIGHT,
            5 => {
                SPEAKER_FRONT_LEFT
                    | SPEAKER_FRONT_RIGHT
                    | SPEAKER_FRONT_CENTER
                    | SPEAKER_BACK_LEFT
                    | SPEAKER_BACK_RIGHT
            }
            6 => {
                SPEAKER_FRONT_LEFT
                    | SPEAKER_FRONT_RIGHT
                    | SPEAKER_FRONT_CENTER
                    | SPEAKER_LOW_FREQUENCY
                    | SPEAKER_BACK_LEFT
                    | SPEAKER_BACK_RIGHT
            }
            8 => {
                SPEAKER_FRONT_LEFT
                    | SPEAKER_FRONT_RIGHT
                    | SPEAKER_FRONT_CENTER
                    | SPEAKER_LOW_FREQUENCY
                    | SPEAKER_BACK_LEFT
                    | SPEAKER_BACK_RIGHT
                    | SPEAKER_SIDE_LEFT
                    | SPEAKER_SIDE_RIGHT
            }
            _ => 0,
        }
    }

    /// 按 `dwChannelMask` 计算每个输入声道到左右声道的增益。
    ///
    /// 掩码位从低到高的顺序就是 WASAPI 交织数据的声道顺序。未识别的声道
    /// （例如顶部声道）权重为 0，直接丢弃；LFE 也不参与下混，避免低频在
    /// 双声道里叠加出浑浊、失真的低频。
    pub(super) fn downmix_weights(channels: usize, channel_mask: u32) -> Vec<(f32, f32)> {
        let mut weights = Vec::with_capacity(channels);
        let mut rest = channel_mask;
        while weights.len() < channels && rest != 0 {
            let bit = rest & rest.wrapping_neg();
            rest &= rest - 1;
            weights.push(match bit {
                SPEAKER_FRONT_LEFT => (1.0, 0.0),
                SPEAKER_FRONT_RIGHT => (0.0, 1.0),
                SPEAKER_FRONT_CENTER => (MIX_SQRT_HALF, MIX_SQRT_HALF),
                SPEAKER_LOW_FREQUENCY => (0.0, 0.0),
                SPEAKER_BACK_LEFT | SPEAKER_SIDE_LEFT | SPEAKER_FRONT_LEFT_OF_CENTER => {
                    (MIX_SQRT_HALF, 0.0)
                }
                SPEAKER_BACK_RIGHT | SPEAKER_SIDE_RIGHT | SPEAKER_FRONT_RIGHT_OF_CENTER => {
                    (0.0, MIX_SQRT_HALF)
                }
                SPEAKER_BACK_CENTER => (MIX_SQRT_HALF, MIX_SQRT_HALF),
                _ => (0.0, 0.0),
            });
        }
        // 掩码为 0 或声位数不足（驱动不规范）时，按前两个声道兜底。
        while weights.len() < channels {
            weights.push(match weights.len() {
                0 => (1.0, 0.0),
                1 => (0.0, 1.0),
                _ => (0.0, 0.0),
            });
        }
        weights
    }

    /// 把一帧多声道样本下混成 `(左, 右)`，未参与声道的权重为 0。
    pub(super) fn downmix_frame(samples: &[f32], weights: &[(f32, f32)]) -> (f32, f32) {
        let mut left = 0.0;
        let mut right = 0.0;
        for (sample, (gain_left, gain_right)) in samples.iter().zip(weights) {
            left += sample * gain_left;
            right += sample * gain_right;
        }
        (left, right)
    }

    /// 软限幅：0.95 以下原样通过，超过后平滑压缩到 ±1 以内。
    ///
    /// 多声道下混会把多个声道的能量叠加到一路，峰值可能超过满量程；
    /// 硬削波会产生刺耳的高频，`tanh` 曲线连续且单调，听感上接近轻度压缩。
    pub(super) fn soft_limit(sample: f32) -> f32 {
        const KNEE: f32 = 0.95;
        if !sample.is_finite() {
            return 0.0;
        }
        let magnitude = sample.abs();
        if magnitude <= KNEE {
            return sample;
        }
        let over = (magnitude - KNEE) / (1.0 - KNEE);
        (KNEE + (1.0 - KNEE) * over.tanh()).copysign(sample)
    }

    /// 把麦克风帧按样本混入系统声帧（不足补零），逐样本软限幅。
    ///
    /// 两路都已是 48kHz 立体声 f32；不做 AEC/降噪（M2 只做基础混音，外放场景
    /// 可能听到回声，属已知边界）。长度不一致时按零补齐短的一侧：系统声与
    /// 麦克风的包长度天然不必对齐，不能丢弃较长的一侧。
    pub(super) fn mix_into(dst: &mut Vec<f32>, src: &[f32]) {
        if dst.len() < src.len() {
            dst.resize(src.len(), 0.0);
        }
        for (value, extra) in dst.iter_mut().zip(src.iter()) {
            *value = soft_limit(*value + *extra);
        }
    }

    /// 比较前后两次枚举到的默认播放设备 ID。
    ///
    /// 空 ID 表示本轮枚举失败（设备切换、驱动重载的瞬间可能出现），不能据此
    /// 判定设备变化；设备 ID 是 GUID 字符串，按大小写不敏感比较。
    pub(super) fn device_id_changed(previous: &str, current: &str) -> bool {
        !current.is_empty() && !previous.eq_ignore_ascii_case(current)
    }

    /// 线性插值的流式重采样器：把任意采样率的交织立体声转到 48kHz。
    ///
    /// 只在 WASAPI 自动转换不可用、且混音采样率不是 48kHz 时启用。
    pub(super) struct LinearResampler {
        /// 每个输出帧对应的输入帧数（输入采样率 / 48000）。
        step: f64,
        /// 下一个输出帧在输入帧轴上的位置（相对当前数据块首帧，可为负）。
        pos: f64,
        /// 上一数据块的最后一帧，用于跨块插值。
        last: [f32; 2],
    }

    impl LinearResampler {
        pub(super) fn new(input_rate: u32) -> Self {
            Self {
                step: f64::from(input_rate) / f64::from(RATE),
                pos: 0.0,
                last: [0.0; 2],
            }
        }

        /// 处理一块交织立体声输入，重采样后追加到 `out`。
        pub(super) fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
            let frames = input.len() / 2;
            if frames == 0 {
                return;
            }
            while self.pos + 1.0 < frames as f64 {
                let (left, right) = self.interpolate(input);
                out.push(left);
                out.push(right);
                self.pos += self.step;
            }
            self.pos -= frames as f64;
            let last = (frames - 1) * 2;
            self.last = [input[last], input[last + 1]];
        }

        /// 在 `pos` 位置做线性插值；`pos` 为负时借用上一块的最后一帧。
        fn interpolate(&self, input: &[f32]) -> (f32, f32) {
            if self.pos < 0.0 {
                let t = (self.pos + 1.0) as f32;
                (
                    self.last[0] + (input[0] - self.last[0]) * t,
                    self.last[1] + (input[1] - self.last[1]) * t,
                )
            } else {
                let index = self.pos.floor() as usize;
                let t = (self.pos - index as f64) as f32;
                let at = index * 2;
                (
                    input[at] + (input[at + 2] - input[at]) * t,
                    input[at + 1] + (input[at + 3] - input[at + 1]) * t,
                )
            }
        }
    }
}

/* ---------------- WASAPI loopback ---------------- */

#[cfg(windows)]
mod platform {
    use std::ffi::c_void;
    use std::ptr;
    use std::time::{Duration, Instant};

    use windows::core::{GUID, PCWSTR};
    use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_FAILED};
    use windows::Win32::Media::Audio::{
        eCapture, eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDevice,
        IMMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT,
        AUDCLNT_E_DEVICE_INVALIDATED, AUDCLNT_E_RESOURCES_INVALIDATED, AUDCLNT_SHAREMODE_SHARED,
        AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
        AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, WAVEFORMATEX,
        WAVEFORMATEXTENSIBLE, WAVEFORMATEXTENSIBLE_0,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
        COINIT_MULTITHREADED,
    };
    use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

    use super::dsp::{
        default_channel_mask, device_id_changed, downmix_frame, downmix_weights, sample_kind,
        soft_limit, CaptureFormat, LinearResampler,
    };
    use super::{DEVICE_POLL_INTERVAL, MIC_WAIT_MS, RATE};

    const CLSID_MM_DEVICE_ENUMERATOR: GUID =
        GUID::from_u128(0xBCDE0395_E52F_467C_8E3D_C4579291692E);
    const WAVE_FORMAT_PCM: u16 = 0x0001;
    const WAVE_FORMAT_IEEE_FLOAT: u16 = 0x0003;
    const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
    const KSDATAFORMAT_SUBTYPE_PCM: GUID = GUID::from_u128(0x00000001_0000_0010_8000_00AA00389B71);
    const KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: GUID =
        GUID::from_u128(0x00000003_0000_0010_8000_00AA00389B71);

    /// 默认渲染设备的 loopback 采集器（共享模式，只收系统播放，不含麦克风）。
    pub struct Loopback {
        enumerator: IMMDeviceEnumerator,
        /// 采集事件句柄（自动重置）；跨流重建复用同一个。
        event: HANDLE,
        stream: Option<Stream>,
        /// 当前流对应的默认设备 ID；空串表示未知。
        device_id: String,
        /// 下一次检查默认设备 / 重试建立流的时刻。
        next_device_check: Instant,
        /// 连续失败只记一次日志，恢复后重置。
        logged_unavailable: bool,
    }

    /// 默认设备枚举结果：区分“没有默认端点”和“有端点但取不到 ID”。
    enum DeviceSnapshot {
        Missing,
        UnknownId,
        Id(String),
    }

    /// 一路已启动的采集流；Drop 时负责 Stop。
    struct Stream {
        client: IAudioClient,
        capture: IAudioCaptureClient,
        format: CaptureFormat,
        weights: Vec<(f32, f32)>,
        /// 仅在原生采样率不是 48kHz 时启用（自动转换不可用的回退路径）。
        resampler: Option<LinearResampler>,
        /// 单帧多声道解码暂存。
        frame: Vec<f32>,
        /// 下混后的原生采样率立体声暂存（重采样路径使用）。
        native: Vec<f32>,
        /// 断点日志只打一次，避免设备抖动时刷屏。
        logged_discontinuity: bool,
    }

    impl Stream {
        /// 排空采集缓冲里所有可用包，把 48kHz 立体声 f32 追加到 `out`。
        fn drain(&mut self, out: &mut Vec<f32>) -> windows::core::Result<()> {
            loop {
                let pending = unsafe { self.capture.GetNextPacketSize()? };
                if pending == 0 {
                    return Ok(());
                }
                let mut data: *mut u8 = ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                unsafe {
                    self.capture
                        .GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                }
                if frames == 0 {
                    // 兜底：GetNextPacketSize 已保证非空，出现 0 帧时直接退出。
                    unsafe { self.capture.ReleaseBuffer(0)? };
                    return Ok(());
                }
                self.convert(data, frames, flags, out);
                unsafe { self.capture.ReleaseBuffer(frames)? };
            }
        }

        /// 把一包原生格式样本转成 48kHz 立体声 f32。
        fn convert(&mut self, data: *mut u8, frames: u32, flags: u32, out: &mut Vec<f32>) {
            let silent = flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0;
            if flags & (AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32) != 0
                && !self.logged_discontinuity
            {
                applog!("[peer] 音频数据流出现断点（采集刚建立或设备/格式切换），继续采集");
                self.logged_discontinuity = true;
            }
            let channels = self.format.channels;
            let block_align = self.format.block_align;
            let kind = self.format.kind;
            self.frame.resize(channels, 0.0);
            self.native.clear();
            for index in 0..frames as usize {
                if silent || data.is_null() {
                    self.frame.fill(0.0);
                } else {
                    let base = unsafe { data.add(index * block_align) };
                    for (channel, sample) in self.frame.iter_mut().enumerate() {
                        *sample = unsafe { kind.decode(base, channel * kind.bytes()) };
                    }
                }
                let (left, right) = downmix_frame(&self.frame, &self.weights);
                self.native.push(soft_limit(left));
                self.native.push(soft_limit(right));
            }
            if let Some(resampler) = &mut self.resampler {
                resampler.process(&self.native, out);
            } else {
                out.extend_from_slice(&self.native);
            }
        }
    }

    impl Drop for Stream {
        fn drop(&mut self) {
            // 重建/退出统一走这里：旧 client 先 Stop，再由 COM 析构释放接口。
            unsafe {
                let _ = self.client.Stop();
            }
        }
    }

    impl Loopback {
        /// 初始化 COM、事件与默认设备的采集流。
        ///
        /// 没有默认播放设备会直接返回错误（音频通道会告知查看端不可用）；
        /// 开始采集后的设备失效/切换由 `read_into` 内部重建，不影响通道。
        pub fn new() -> Result<Self, String> {
            unsafe {
                CoInitializeEx(None, COINIT_MULTITHREADED)
                    .ok()
                    .map_err(|e| format!("COM 初始化失败: {e}"))?;
            }
            let enumerator: IMMDeviceEnumerator =
                match unsafe { CoCreateInstance(&CLSID_MM_DEVICE_ENUMERATOR, None, CLSCTX_ALL) } {
                    Ok(enumerator) => enumerator,
                    Err(error) => {
                        unsafe { CoUninitialize() };
                        return Err(format!("创建音频设备枚举器失败: {error}"));
                    }
                };
            let event = match unsafe { CreateEventW(None, false, false, PCWSTR::null()) } {
                Ok(event) => event,
                Err(error) => {
                    unsafe { CoUninitialize() };
                    return Err(format!("创建音频事件失败: {error}"));
                }
            };
            let mut loopback = Self {
                enumerator,
                event,
                stream: None,
                device_id: String::new(),
                next_device_check: Instant::now(),
                logged_unavailable: false,
            };
            loopback.open_default_stream()?;
            Ok(loopback)
        }

        /// 采集一拍：等事件、排空缓冲，把 48kHz 立体声 f32 追加到 `out`。
        ///
        /// 设备失效、默认设备切换、驱动重载都在内部重建采集流并继续返回 `Ok`，
        /// 调用方无需感知；只有 Win32 等待失败这类不可恢复错误才返回 `Err`。
        /// 采集一拍：等事件、排空缓冲，把 48kHz 立体声 f32 追加到 `out`。
        ///
        /// `wait_ms` 由调用方决定：麦克风开启时用更短的切片，保证麦克风延迟有界。
        pub fn read_into(&mut self, out: &mut Vec<f32>, wait_ms: u32) -> Result<(), String> {
            self.maintain();
            let drained = {
                let Some(stream) = self.stream.as_mut() else {
                    // 当前没有可用设备：按重试节奏等待，避免忙等。
                    std::thread::sleep(Duration::from_millis(u64::from(wait_ms)));
                    return Ok(());
                };
                // 事件驱动：有新数据才唤醒；超时只用于响应停止/设备检查。
                if unsafe { WaitForSingleObject(self.event, wait_ms) } == WAIT_FAILED {
                    return Err("等待音频事件失败".to_string());
                }
                stream.drain(out)
            };
            match drained {
                Ok(()) => Ok(()),
                Err(error) => {
                    if !self.logged_unavailable {
                        if is_invalidated(&error) {
                            applog!("[peer] 音频设备已失效，重建采集流: {error}");
                        } else {
                            applog!("[peer] 音频采集出错，重建采集流: {error}");
                        }
                        self.logged_unavailable = true;
                    }
                    self.drop_stream();
                    Ok(())
                }
            }
        }

        /// 每秒检查一次默认播放设备；变化则重建，失败只重试不退出。
        fn maintain(&mut self) {
            let now = Instant::now();
            if now < self.next_device_check {
                return;
            }
            self.next_device_check = now + DEVICE_POLL_INTERVAL;
            if self.stream.is_none() {
                self.try_open_stream();
                return;
            }
            match self.default_device() {
                DeviceSnapshot::Missing => {
                    applog!("[peer] 默认播放设备不可用，暂停音频采集等待恢复");
                    self.drop_stream();
                    self.device_id.clear();
                }
                DeviceSnapshot::UnknownId => {}
                DeviceSnapshot::Id(id) => {
                    if self.device_id.is_empty() {
                        // 开流时取 ID 失败过：先补记，避免无谓重建一次。
                        self.device_id = id;
                    } else if device_id_changed(&self.device_id, &id) {
                        applog!("[peer] 默认播放设备已切换，重建音频采集流");
                        self.drop_stream();
                        self.try_open_stream();
                    }
                }
            }
        }

        fn try_open_stream(&mut self) {
            match self.open_default_stream() {
                Ok(()) => self.logged_unavailable = false,
                Err(error) => {
                    if !self.logged_unavailable {
                        applog!("[peer] 音频采集暂不可用，稍后重试: {error}");
                        self.logged_unavailable = true;
                    }
                }
            }
        }

        /// 打开当前默认播放设备的采集流，成功后更新设备 ID 与下次检查时间。
        fn open_default_stream(&mut self) -> Result<(), String> {
            let device = unsafe { self.enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }
                .map_err(|e| format!("没有默认播放设备: {e}"))?;
            let device_id = device_id_of(&device).unwrap_or_default();
            let stream = open_stream(&device, self.event, true)?;
            self.stream = Some(stream);
            self.device_id = device_id;
            self.next_device_check = Instant::now() + DEVICE_POLL_INTERVAL;
            self.logged_unavailable = false;
            Ok(())
        }

        /// 丢弃当前流（`Stream::drop` 里 Stop/Release），并安排下一拍立即重试。
        fn drop_stream(&mut self) {
            self.stream = None;
            self.next_device_check = Instant::now();
        }

        fn default_device(&self) -> DeviceSnapshot {
            let Ok(device) =
                (unsafe { self.enumerator.GetDefaultAudioEndpoint(eRender, eConsole) })
            else {
                return DeviceSnapshot::Missing;
            };
            match device_id_of(&device) {
                Some(id) => DeviceSnapshot::Id(id),
                None => DeviceSnapshot::UnknownId,
            }
        }
    }

    impl Drop for Loopback {
        fn drop(&mut self) {
            // 先停流再关事件句柄，最后释放本线程的 COM 引用。
            self.stream = None;
            unsafe {
                let _ = CloseHandle(self.event);
                CoUninitialize();
            }
        }
    }

    /// 默认录音端点采集器：由查看端显式开启，Drop 即释放麦克风设备。
    ///
    /// COM 引用按「每次 `new` 一次 `CoInitializeEx`、`Drop` 时一次 `CoUninitialize`」
    /// 配对：采集线程已初始化过 MTA，这里会得到 `S_FALSE`，仍需要配对释放。
    pub struct MicCapture {
        stream: Stream,
        event: HANDLE,
        com_initialized: bool,
    }

    impl MicCapture {
        /// 打开默认录音端点并按 48kHz/2ch/f32 采集；无设备/无权限时返回错误。
        pub fn new() -> Result<Self, String> {
            let com_initialized = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).is_ok() };
            let result = (|| -> Result<Self, String> {
                let enumerator: IMMDeviceEnumerator =
                    unsafe { CoCreateInstance(&CLSID_MM_DEVICE_ENUMERATOR, None, CLSCTX_ALL) }
                        .map_err(|e| format!("创建音频设备枚举器失败: {e}"))?;
                let device = unsafe { enumerator.GetDefaultAudioEndpoint(eCapture, eConsole) }
                    .map_err(|e| format!("没有默认录音设备: {e}"))?;
                let event = unsafe { CreateEventW(None, false, false, PCWSTR::null()) }
                    .map_err(|e| format!("创建麦克风事件失败: {e}"))?;
                match open_stream(&device, event, false) {
                    Ok(stream) => {
                        applog!("[peer] 麦克风采集流已建立（{}）", stream.format.describe());
                        Ok(Self {
                            stream,
                            event,
                            com_initialized,
                        })
                    }
                    Err(e) => {
                        unsafe {
                            let _ = CloseHandle(event);
                        }
                        Err(e)
                    }
                }
            })();
            if result.is_err() && com_initialized {
                unsafe { CoUninitialize() };
            }
            result
        }

        /// 排空当前可用的麦克风包（最多等 5ms），追加 48kHz 立体声 f32。
        pub fn read_into(&mut self, out: &mut Vec<f32>) -> Result<(), String> {
            if unsafe { WaitForSingleObject(self.event, MIC_WAIT_MS) } == WAIT_FAILED {
                return Err("等待麦克风事件失败".to_string());
            }
            self.stream
                .drain(out)
                .map_err(|e| format!("读取麦克风数据失败: {e}"))
        }
    }

    impl Drop for MicCapture {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.event);
                if self.com_initialized {
                    CoUninitialize();
                }
            }
        }
    }

    /// 建立并启动采集流：优先 WASAPI 自动转换，失败回退混音格式 + 软件处理。
    ///
    /// `loopback = true` 采默认渲染设备的系统声；`false` 采默认录音设备（麦克风）。
    fn open_stream(device: &IMMDevice, event: HANDLE, loopback: bool) -> Result<Stream, String> {
        match open_converted(device, event, loopback) {
            Ok(stream) => {
                applog!(
                    "[peer] 音频采集：{}（WASAPI 自动转换）",
                    stream.format.describe()
                );
                Ok(stream)
            }
            Err(error) => {
                applog!("[peer] 音频自动转换不可用，回退混音格式: {error}");
                let stream = open_mix_format(device, event, loopback)?;
                applog!(
                    "[peer] 音频采集：{}（软件下混/重采样）",
                    stream.format.describe()
                );
                Ok(stream)
            }
        }
    }

    /// 请求 48kHz/2ch/f32，由 WASAPI 自动转换到设备混音格式对应的流。
    fn open_converted(device: &IMMDevice, event: HANDLE, loopback: bool) -> Result<Stream, String> {
        let client = activate_client(device)?;
        let requested = converted_format();
        let mut flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK
            | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
            | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
        if loopback {
            flags |= AUDCLNT_STREAMFLAGS_LOOPBACK;
        }
        unsafe {
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                flags,
                0,
                0,
                &requested as *const WAVEFORMATEXTENSIBLE as *const WAVEFORMATEX,
                None,
            )
        }
        .map_err(|e| format!("初始化自动转换采集流失败: {e}"))?;
        start_stream(client, event, CaptureFormat::converted())
    }

    /// 回退路径：用设备混音格式采集，采样率/声道交给软件处理。
    fn open_mix_format(
        device: &IMMDevice,
        event: HANDLE,
        loopback: bool,
    ) -> Result<Stream, String> {
        let client = activate_client(device)?;
        let raw = unsafe { client.GetMixFormat() }.map_err(|e| format!("读取混音格式失败: {e}"))?;
        let format = match unsafe { mix_format(raw) } {
            Ok(format) => format,
            Err(error) => {
                unsafe { CoTaskMemFree(Some(raw as *const c_void)) };
                return Err(error);
            }
        };
        let mut flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
        if loopback {
            flags |= AUDCLNT_STREAMFLAGS_LOOPBACK;
        }
        let initialized =
            unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 0, 0, raw, None) };
        unsafe { CoTaskMemFree(Some(raw as *const c_void)) };
        initialized.map_err(|e| format!("初始化回退采集流失败: {e}"))?;
        start_stream(client, event, format)
    }

    /// Initialize 之后的公共启动步骤：绑定事件、取采集接口、Start。
    fn start_stream(
        client: IAudioClient,
        event: HANDLE,
        format: CaptureFormat,
    ) -> Result<Stream, String> {
        unsafe { client.SetEventHandle(event) }.map_err(|e| format!("绑定音频事件失败: {e}"))?;
        let capture = unsafe { client.GetService::<IAudioCaptureClient>() }
            .map_err(|e| format!("获取音频采集接口失败: {e}"))?;
        unsafe { client.Start() }.map_err(|e| format!("启动音频采集失败: {e}"))?;
        Ok(Stream {
            weights: downmix_weights(format.channels, format.channel_mask),
            resampler: (format.rate != RATE).then(|| LinearResampler::new(format.rate)),
            frame: vec![0.0; format.channels],
            native: Vec::new(),
            logged_discontinuity: false,
            client,
            capture,
            format,
        })
    }

    fn activate_client(device: &IMMDevice) -> Result<IAudioClient, String> {
        let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }
            .map_err(|e| format!("激活音频客户端失败: {e}"))?;
        Ok(client)
    }

    /// 解析混音格式；返回后 `raw` 仍需由调用方在 `Initialize` 之后再释放。
    unsafe fn mix_format(raw: *const WAVEFORMATEX) -> Result<CaptureFormat, String> {
        if raw.is_null() {
            return Err("混音格式为空".to_string());
        }
        let wfx = unsafe { ptr::read_unaligned(raw) };
        let channels = wfx.nChannels as usize;
        if channels == 0 || wfx.nSamplesPerSec == 0 {
            return Err("混音格式的声道数或采样率为 0".to_string());
        }
        let mut tag = wfx.wFormatTag;
        let mut channel_mask = 0;
        if tag == WAVE_FORMAT_EXTENSIBLE {
            let ext = unsafe { ptr::read_unaligned(raw as *const WAVEFORMATEXTENSIBLE) };
            channel_mask = ext.dwChannelMask;
            // packed 结构字段不能取引用：比较/格式化都会隐式取引用，
            // 所以先按值拷贝到局部变量再判断。
            let sub_format = ext.SubFormat;
            tag = if sub_format == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT {
                WAVE_FORMAT_IEEE_FLOAT
            } else if sub_format == KSDATAFORMAT_SUBTYPE_PCM {
                WAVE_FORMAT_PCM
            } else {
                return Err(format!("不支持的混音子格式: {sub_format:?}"));
            };
        }
        let kind = sample_kind(tag, wfx.wBitsPerSample)?;
        let block_align = wfx.nBlockAlign as usize;
        if block_align < channels * kind.bytes() {
            return Err(format!(
                "混音格式块对齐异常: blockAlign={block_align} channels={channels}"
            ));
        }
        if channel_mask == 0 {
            channel_mask = default_channel_mask(channels);
        }
        Ok(CaptureFormat {
            rate: wfx.nSamplesPerSec,
            channels,
            block_align,
            kind,
            channel_mask,
        })
    }

    /// 自动转换请求的目标格式：48kHz/2ch/f32 的 WAVEFORMATEXTENSIBLE。
    fn converted_format() -> WAVEFORMATEXTENSIBLE {
        WAVEFORMATEXTENSIBLE {
            Format: WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_EXTENSIBLE,
                nChannels: 2,
                nSamplesPerSec: RATE,
                nAvgBytesPerSec: RATE * 2 * 4,
                nBlockAlign: (2 * 4) as u16,
                wBitsPerSample: 32,
                cbSize: (std::mem::size_of::<WAVEFORMATEXTENSIBLE>()
                    - std::mem::size_of::<WAVEFORMATEX>()) as u16,
            },
            Samples: WAVEFORMATEXTENSIBLE_0 {
                wValidBitsPerSample: 32,
            },
            dwChannelMask: 0x0003, // SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT
            SubFormat: KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
        }
    }

    /// 读取默认播放设备 ID；失败返回 `None`（按“ID 未知”处理，不触发重建）。
    fn device_id_of(device: &IMMDevice) -> Option<String> {
        unsafe {
            let raw = device.GetId().ok()?;
            let id = raw.to_string().ok();
            CoTaskMemFree(Some(raw.as_ptr() as *const c_void));
            id.filter(|value| !value.is_empty())
        }
    }

    /// 设备被移除/禁用，或底层音频资源失效：需要重建采集流。
    fn is_invalidated(error: &windows::core::Error) -> bool {
        let code = error.code();
        code == AUDCLNT_E_DEVICE_INVALIDATED || code == AUDCLNT_E_RESOURCES_INVALIDATED
    }

    #[cfg(test)]
    mod tests {
        use windows::Win32::Media::Audio::{
            WAVEFORMATEX, WAVEFORMATEXTENSIBLE, WAVEFORMATEXTENSIBLE_0,
        };

        use super::super::dsp::{downmix_weights, SampleKind};
        use super::{
            mix_format, KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_EXTENSIBLE, WAVE_FORMAT_PCM,
        };

        /// 回退路径必须正确解析扩展格式的声道数、掩码与子格式（无需真实设备）。
        #[test]
        fn mix_format_parses_extensible_5_1() {
            let fmt = WAVEFORMATEXTENSIBLE {
                Format: WAVEFORMATEX {
                    wFormatTag: WAVE_FORMAT_EXTENSIBLE,
                    nChannels: 6,
                    nSamplesPerSec: 44_100,
                    nAvgBytesPerSec: 44_100 * 6 * 4,
                    nBlockAlign: (6 * 4) as u16,
                    wBitsPerSample: 32,
                    cbSize: 22,
                },
                Samples: WAVEFORMATEXTENSIBLE_0 {
                    wValidBitsPerSample: 32,
                },
                dwChannelMask: 0,
                SubFormat: KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
            };
            let parsed =
                unsafe { mix_format(&fmt as *const WAVEFORMATEXTENSIBLE as *const WAVEFORMATEX) }
                    .expect("5.1 f32 混音格式");
            assert_eq!(parsed.rate, 44_100);
            assert_eq!(parsed.channels, 6);
            assert_eq!(parsed.kind, SampleKind::F32);
            assert_eq!(parsed.block_align, 24);
            // 掩码为 0 时按 5.1 标准布局推断，下混权重覆盖全部声道。
            assert_eq!(parsed.channel_mask, 0x3F);
            assert_eq!(
                downmix_weights(parsed.channels, parsed.channel_mask).len(),
                6
            );
        }

        /// 非扩展格式（16 位 PCM）也能解析。
        #[test]
        fn mix_format_parses_plain_pcm16() {
            let fmt = WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_PCM,
                nChannels: 2,
                nSamplesPerSec: 48_000,
                nAvgBytesPerSec: 48_000 * 2 * 2,
                nBlockAlign: 4,
                wBitsPerSample: 16,
                cbSize: 0,
            };
            let parsed = unsafe { mix_format(&fmt as *const WAVEFORMATEX) }.expect("PCM16");
            assert_eq!(parsed.kind, SampleKind::I16);
            assert_eq!(parsed.rate, 48_000);
            assert_eq!(parsed.channels, 2);
            assert_eq!(parsed.channel_mask, 0x3);
            assert!(parsed.block_align >= parsed.channels * parsed.kind.bytes());
        }
    }
}

#[cfg(not(windows))]
mod platform {
    pub struct Loopback;
    impl Loopback {
        pub fn new() -> Result<Self, String> {
            Err("音频仅支持 Windows".to_string())
        }
        pub fn read_into(&mut self, _out: &mut Vec<f32>, _wait_ms: u32) -> Result<(), String> {
            Err("音频仅支持 Windows".to_string())
        }
    }

    pub struct MicCapture;
    impl MicCapture {
        pub fn new() -> Result<Self, String> {
            Err("音频仅支持 Windows".to_string())
        }
        pub fn read_into(&mut self, _out: &mut Vec<f32>) -> Result<(), String> {
            Err("音频仅支持 Windows".to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_start_serializes_camel_case() {
        let v = serde_json::to_value(AudioStart {
            id: "a1".into(),
            rate: 48000,
            channels: 2,
            frame_ms: 20,
        })
        .unwrap();
        assert_eq!(v["frameMs"], 20);
        assert_eq!(v["rate"], 48000);
    }

    #[test]
    fn opus_roundtrip() {
        use audiopus::{coder::Decoder, Channels, SampleRate};
        let enc = opus_encoder().expect("encoder");
        let mut dec = Decoder::new(SampleRate::Hz48000, Channels::Stereo).expect("decoder");
        // 440Hz 正弦，20ms
        let mut pcm = Vec::with_capacity(FRAME_SAMPLES * 2);
        for i in 0..FRAME_SAMPLES {
            let t = i as f32 / RATE as f32;
            let s = (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.3;
            pcm.push(s);
            pcm.push(s);
        }
        let mut out = vec![0u8; 4000];
        let n = enc.encode_float(&pcm, &mut out).expect("encode");
        assert!(n > 0);
        let mut decoded = vec![0i16; FRAME_SAMPLES * 2];
        let n = dec
            .decode(
                Some(audiopus::packet::Packet::try_from(&out[..n]).expect("packet")),
                audiopus::MutSignals::try_from(&mut decoded[..]).expect("signals"),
                false,
            )
            .expect("decode");
        assert!(n > 0);
        assert!(decoded.iter().any(|v| v.abs() > 100));
    }

    #[test]
    fn audio_frame_fields_are_camel_case() {
        let v = serde_json::to_value(AudioFrame::Data {
            session_id: "s".into(),
            seq: 7,
            data: "abc".into(),
        })
        .unwrap();
        assert_eq!(v["kind"], "data");
        assert_eq!(v["sessionId"], "s");
        assert_eq!(v["seq"], 7);
    }

    /// 5.1 下混按声道掩码取 FL/FR/FC/BL/BR，LFE 丢弃。
    #[cfg(windows)]
    #[test]
    fn downmix_5_1_follows_channel_mask() {
        use super::dsp::{downmix_frame, downmix_weights};
        let weights = downmix_weights(6, 0x3F);
        // 各声道值保持在软限幅拐点以下，便于精确断言增益。
        let samples = [0.1, 0.2, 0.2, 1.0, 0.1, 0.2];
        let (left, right) = downmix_frame(&samples, &weights);
        let c = std::f32::consts::FRAC_1_SQRT_2;
        let expected_left = 0.1 + c * 0.2 + c * 0.1;
        let expected_right = 0.2 + c * 0.2 + c * 0.2;
        assert!((left - expected_left).abs() < 1e-6, "left={left}");
        assert!((right - expected_right).abs() < 1e-6, "right={right}");
    }

    /// 单声道按中置处理：左右各 0.707。
    #[cfg(windows)]
    #[test]
    fn downmix_mono_center_to_both() {
        use super::dsp::{default_channel_mask, downmix_frame, downmix_weights};
        let weights = downmix_weights(1, default_channel_mask(1));
        let (left, right) = downmix_frame(&[0.5], &weights);
        let expected = 0.5 * std::f32::consts::FRAC_1_SQRT_2;
        assert!((left - expected).abs() < 1e-6);
        assert!((right - expected).abs() < 1e-6);
    }

    /// 掩码缺失时兜底为“前两个声道即左右声道”。
    #[cfg(windows)]
    #[test]
    fn downmix_unknown_mask_falls_back_to_first_two() {
        use super::dsp::downmix_weights;
        assert_eq!(
            downmix_weights(4, 0),
            vec![(1.0, 0.0), (0.0, 1.0), (0.0, 0.0), (0.0, 0.0)]
        );
    }

    /// 软限幅：拐点以下不改变信号，超限后单调且始终在 ±1 以内。
    #[cfg(windows)]
    #[test]
    fn soft_limit_passes_normal_and_bounds_overs() {
        use super::dsp::soft_limit;
        assert_eq!(soft_limit(0.0), 0.0);
        assert_eq!(soft_limit(0.5), 0.5);
        assert_eq!(soft_limit(-0.9), -0.9);
        let just_over = soft_limit(1.0);
        assert!(just_over > 0.95 && just_over < 1.0, "value={just_over}");
        // 极大值经 f32 的 tanh 饱和后正好落在 ±1，边界按闭区间断言。
        assert!(soft_limit(8.0) <= 1.0);
        assert!(soft_limit(-8.0) >= -1.0);
        assert!(soft_limit(0.4) < soft_limit(0.6));
        assert!(soft_limit(2.0) >= soft_limit(1.0));
        assert_eq!(soft_limit(f32::NAN), 0.0);
    }

    /// 空 ID（枚举失败）不算设备变化，设备 ID 比较大小写不敏感。
    #[cfg(windows)]
    #[test]
    fn device_id_changed_ignores_unknown_and_case() {
        use super::dsp::device_id_changed;
        assert!(!device_id_changed("abc", ""));
        assert!(!device_id_changed("ABC", "abc"));
        assert!(device_id_changed("abc", "def"));
        assert!(device_id_changed("", "abc"));
    }

    /// 44.1k/96k 之类的降采样与 24k 升采样都能得到 48kHz 输出。
    #[cfg(windows)]
    #[test]
    fn resampler_hits_48k_target() {
        use super::dsp::LinearResampler;
        let mut up = LinearResampler::new(24_000);
        let input: Vec<f32> = (0..10).flat_map(|_| [0.5f32, -0.5]).collect();
        let mut out = Vec::new();
        up.process(&input, &mut out);
        assert_eq!(out.len(), 18 * 2);
        for frame in out.chunks_exact(2) {
            assert!((frame[0] - 0.5).abs() < 1e-6);
            assert!((frame[1] + 0.5).abs() < 1e-6);
        }

        let mut down = LinearResampler::new(96_000);
        let input: Vec<f32> = (0..8).flat_map(|i| [i as f32, -(i as f32)]).collect();
        let mut out = Vec::new();
        down.process(&input, &mut out);
        let lefts: Vec<f32> = out.chunks_exact(2).map(|frame| frame[0]).collect();
        assert_eq!(lefts, vec![0.0, 2.0, 4.0, 6.0]);
    }

    /// 样本类型换算与声道掩码推断。
    #[cfg(windows)]
    #[test]
    fn format_helpers_cover_shared_mix_formats() {
        use super::dsp::{default_channel_mask, sample_kind, SampleKind};
        assert_eq!(sample_kind(3, 32).unwrap(), SampleKind::F32);
        assert_eq!(sample_kind(1, 16).unwrap(), SampleKind::I16);
        assert_eq!(sample_kind(1, 32).unwrap(), SampleKind::I32);
        assert!(sample_kind(1, 24).is_err());

        assert_eq!(default_channel_mask(2), 0x3);
        assert_eq!(default_channel_mask(6), 0x3F);
        assert_eq!(default_channel_mask(8), 0x63F);
        assert_eq!(default_channel_mask(3), 0);
    }

    /// 有默认播放设备时验证事件等待/排空路径；没有设备的环境直接跳过。
    ///
    /// 只做短时 loopback 读取，不播放声音、不弹窗；无音频设备/音频服务时
    /// `Loopback::new()` 返回错误，测试视为环境不满足而返回。
    #[cfg(windows)]
    #[test]
    fn loopback_smoke_skips_without_device() {
        let opened = std::thread::spawn(|| match super::platform::Loopback::new() {
            Ok(mut loopback) => {
                let mut samples = Vec::new();
                let _ = loopback.read_into(&mut samples, MIC_LOOP_WAIT_MS);
                true
            }
            Err(_) => false,
        })
        .join()
        .unwrap_or(false);
        if !opened {
            // 无默认播放设备或音频服务不可用：测试环境不满足，优雅跳过。
            return;
        }
    }

    /// 麦克风控制帧只接受 `{"type":"mic","on":bool}`，其它帧一律忽略。
    #[test]
    fn mic_control_parses_only_mic_frames() {
        assert_eq!(
            parse_mic_control(br#"{"type":"mic","on":true}"#),
            Some(true)
        );
        assert_eq!(
            parse_mic_control(br#"{"type":"mic","on":false}"#),
            Some(false)
        );
        assert_eq!(parse_mic_control(br#"{"type":"mic"}"#), Some(false));
        assert_eq!(parse_mic_control(br#"{"type":"other","on":true}"#), None);
        assert_eq!(parse_mic_control(b"not json"), None);
    }

    /// 麦克风混音：逐样本相加 + 软限幅；长度不一致时补齐短的一侧。
    #[test]
    fn mix_into_adds_and_soft_limits() {
        let mut dst = vec![0.2f32, -0.2];
        dsp::mix_into(&mut dst, &[0.3, -0.3]);
        assert!((dst[0] - 0.5).abs() < 1e-6);
        assert!((dst[1] + 0.5).abs() < 1e-6);

        let mut loud = vec![0.9f32];
        dsp::mix_into(&mut loud, &[0.9]);
        assert!(loud[0] > 0.95 && loud[0] <= 1.0);

        let mut short = vec![0.1f32];
        dsp::mix_into(&mut short, &[0.2, 0.4]);
        assert_eq!(short.len(), 2);
        assert!((short[0] - 0.3).abs() < 1e-6);
        assert!((short[1] - 0.4).abs() < 1e-6);
    }
}
