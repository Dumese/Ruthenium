//! 远程桌面：键鼠输入注入（`subsystem("ruthenium-input")`）。
//!
//! 授权模型：
//! - 连接层：对端必须授予 `grant_control`（复用终端控制的授权）；
//! - 会话层：**每次查看**在本机确认时可单独勾选「允许操作」；只勾「允许查看」时
//!   输入子系统会拒绝（只读由服务端强制，前端隐藏入口只是 UX）。
//!
//! 输入帧：`u32 大端长度 + JSON`，首帧必须是
//! `{"type":"hello","session":"<屏幕会话 id>"}`（令牌即屏幕会话 id，随机 UUID、
//! 只在加密的 SSH 通道里出现）。之后支持 move / button / wheel / key / text。
//!
//! 注入语义：绝对坐标按 **VIRTUALDESK** 0..65535 映射；
//! `MapVirtualKeyW(MAPVK_VK_TO_VSC_EX)` + 扩展键标志；滚轮 delta 恒为 120；
//! Unicode 文本按 UTF-16 码元逐字符发送（代理对拆两个事件）。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;

use crate::error::{AppError, AppResult};
use crate::peer::rd::ScreenHub;
use crate::state::AppState;

/// 子系统名。
pub const SUBSYSTEM: &str = "ruthenium-input";
/// 查看端每 2s 发一次 ping；15s 没有任何帧说明链路已半开/卡死，必须释放按键。
const INPUT_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum InputMsg {
    Hello {
        session: String,
    },
    Move {
        x: f64,
        y: f64,
        #[serde(default)]
        monitor: usize,
    },
    Button {
        button: String,
        down: bool,
    },
    Wheel {
        delta: i32,
    },
    Key {
        vk: u16,
        down: bool,
    },
    Text {
        s: String,
    },
    Ping {
        t: i64,
    },
}

/// 查看端输入会话（命令发送 + 取消）。
pub struct InputSession {
    cmd_tx: mpsc::Sender<Vec<u8>>,
    cancel: CancellationToken,
}

#[derive(Default, Clone)]
pub struct InputManager {
    sessions: Arc<Mutex<HashMap<String, InputSession>>>,
}

impl InputManager {
    pub fn new() -> Self {
        Self::default()
    }

    async fn insert(&self, id: String, s: InputSession) {
        self.sessions.lock().await.insert(id, s);
    }

    /// 发送一条输入事件（JSON 自动加长度前缀）。
    pub async fn send(&self, id: &str, msg: Value) -> bool {
        let tx = {
            let map = self.sessions.lock().await;
            match map.get(id) {
                Some(s) => s.cmd_tx.clone(),
                None => return false,
            }
        };
        let Ok(bytes) = serde_json::to_vec(&msg) else {
            return false;
        };
        let mut framed = Vec::with_capacity(4 + bytes.len());
        framed.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        framed.extend_from_slice(&bytes);
        tx.send(framed).await.is_ok()
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

/// 查看端：为某条屏幕会话打开输入连接（令牌 = 屏幕会话 id）。
///
/// 开启 `require_windows_hello` 时，打开**注入通道**前先过一次本机 Hello——
/// SendInput 是桌面级原语（等价完全接管键鼠），网络侧受控端有确认 + 每条事件
/// 复核授权，本机侧这条 IPC 入口同样要有“是本人在操作”的存在性校验。
pub async fn open(
    state: &AppState,
    screen_session_id: &str,
    token: Option<&str>,
) -> AppResult<String> {
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

    // Hello 前置（开启时）。失败/取消即不建立注入通道。
    let need_hello = state.config.read().await.preferences.require_windows_hello;
    if need_hello {
        let peer_name = peer.name.clone();
        let outcome = tauri::async_runtime::spawn_blocking(move || {
            crate::hello::verify(&format!("对「{peer_name}」启用远程操作"))
        })
        .await
        .map_err(|e| AppError::EPeerHelloDenied(format!("Windows Hello 线程失败: {e}")))?;
        if outcome != crate::hello::HelloOutcome::Verified {
            return Err(AppError::EPeerHelloDenied(format!(
                "Windows Hello 未验证通过（{outcome:?}），已取消远程操作"
            )));
        }
    }

    let identity = state.peer.identity_handle().await?;
    let mut handle = crate::peer::client::connect(state, &peer, "远程操作").await?;
    crate::peer::client::authenticate(&mut handle, &identity).await?;

    let mut channel = handle
        .channel_open_session()
        .await
        .map_err(|e| AppError::ESshChannel(format!("打开输入通道失败: {e}")))?;
    channel
        .request_subsystem(true, SUBSYSTEM)
        .await
        .map_err(|e| AppError::EPeerNotAllowed(format!("对端拒绝输入通道: {e}")))?;
    match tokio::time::timeout(std::time::Duration::from_secs(5), channel.wait()).await {
        Ok(Some(russh::ChannelMsg::Success)) => {}
        Ok(Some(russh::ChannelMsg::Failure)) => {
            return Err(AppError::EPeerNotAllowed(
                "对端拒绝键鼠操作：本次查看只允许「看」，或授权已撤销".to_string(),
            ))
        }
        Ok(None) => return Err(AppError::EPeerNotAllowed("对端关闭了输入通道".to_string())),
        Err(_) => return Err(AppError::EPeerNotAllowed("对端未响应输入请求".to_string())),
        Ok(Some(_)) => {}
    }
    let stream = channel.into_stream();
    let (mut rd, mut wr) = tokio::io::split(stream);
    let cancel = CancellationToken::new();
    let input_id = uuid::Uuid::new_v4().to_string();
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<Vec<u8>>(64);

    // 先发 hello 并等服务端回执（只读会话在这里就被拒绝，而不是悄悄不生效）
    let hello = serde_json::json!({ "type": "hello", "session": token });
    let hello_bytes = serde_json::to_vec(&hello)
        .map_err(|e| AppError::EInvalidInput(format!("hello 序列化失败: {e}")))?;
    let mut framed = Vec::with_capacity(4 + hello_bytes.len());
    framed.extend_from_slice(&(hello_bytes.len() as u32).to_be_bytes());
    framed.extend_from_slice(&hello_bytes);
    wr.write_all(&framed)
        .await
        .map_err(|e| AppError::ESshChannel(format!("发送输入握手失败: {e}")))?;
    let ack = tokio::time::timeout(std::time::Duration::from_secs(5), read_frame(&mut rd)).await;
    match ack {
        Ok(Some(body)) => {
            let v: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
            if v.get("type").and_then(|x| x.as_str()) != Some("ok") {
                let detail = v
                    .get("detail")
                    .and_then(|x| x.as_str())
                    .unwrap_or("对端拒绝键鼠操作");
                return Err(AppError::EPeerNotAllowed(detail.to_string()));
            }
        }
        Ok(None) => {
            return Err(AppError::EPeerNotAllowed(
                "对端拒绝键鼠操作：本次查看只允许「看」，或授权已撤销".to_string(),
            ))
        }
        Err(_) => return Err(AppError::EPeerNotAllowed("对端未响应输入握手".to_string())),
    }

    // 读端：探测断线 + 接收 pong（延迟）；handle 由该任务持有以保持连接
    {
        let cancel = cancel.clone();
        let name = peer.name.clone();
        let app = state.app.clone();
        let sid = screen_session_id.to_string();
        let iid = input_id.clone();
        tokio::spawn(async move {
            let _keep = handle;
            loop {
                let next = tokio::select! {
                    _ = cancel.cancelled() => None,
                    frame = tokio::time::timeout(INPUT_IDLE_TIMEOUT, read_frame(&mut rd)) => {
                        frame.ok().flatten()
                    },
                };
                let Some(body) = next else { break };
                let Ok(v) = serde_json::from_slice::<serde_json::Value>(&body) else {
                    continue;
                };
                if v.get("type").and_then(|x| x.as_str()) == Some("pong") {
                    if let Some(t) = v.get("t").and_then(|x| x.as_i64()) {
                        let rtt = (now_ms() - t).clamp(0, 60_000);
                        crate::events::emit(
                            &app,
                            crate::events::PEER_INPUT_STATS,
                            serde_json::json!({ "sessionId": sid, "rttMs": rtt }),
                        );
                    }
                }
            }
            // 输入通道单独断开（屏幕流可能还在）：通知前端退出操作模式，
            // 否则它会继续拦截本机键鼠并把事件发往已关闭的通道。
            crate::events::emit(
                &app,
                crate::events::PEER_INPUT_STATS,
                serde_json::json!({
                    "sessionId": sid,
                    "inputId": iid,
                    "rttMs": -1,
                    "ended": true
                }),
            );
            applog!("[peer] 输入通道结束: {name}");
        });
    }
    // 延迟探测：每 2s 一次 ping（服务端原样回 pong）
    {
        let cancel = cancel.clone();
        let tx = cmd_tx.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {}
                }
                let bytes =
                    match serde_json::to_vec(&serde_json::json!({ "type": "ping", "t": now_ms() }))
                    {
                        Ok(b) => b,
                        Err(_) => continue,
                    };
                let mut framed = Vec::with_capacity(4 + bytes.len());
                framed.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
                framed.extend_from_slice(&bytes);
                if tx.send(framed).await.is_err() {
                    break;
                }
            }
        });
    }
    // writer：命令 → 通道（hello 已在上方发出）
    {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    msg = cmd_rx.recv() => match msg {
                        Some(bytes) => {
                            match tokio::time::timeout(
                                std::time::Duration::from_secs(5),
                                wr.write_all(&bytes),
                            )
                            .await
                            {
                                Ok(Ok(())) => {}
                                _ => break,
                            }
                        }
                        None => break,
                    }
                }
            }
            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), wr.shutdown()).await;
            // 写入失败/通道半开：让读端也结束并通知前端退出操作模式。
            cancel.cancel();
        });
    }

    state
        .input
        .insert(input_id.clone(), InputSession { cmd_tx, cancel })
        .await;
    Ok(input_id)
}

/* ---------------- 受控端（注入） ---------------- */

fn normalize_button(button: &str) -> &str {
    match button {
        "right" | "middle" => button,
        _ => "left",
    }
}

/// 跟踪当前按下的键/鼠标键；无论正常结束、断线、撤销还是提前返回，
/// Drop 都会补发 keyup/mouseup，避免远端残留下压键导致本机“卡键/拖拽”。
#[derive(Default)]
struct HeldInput {
    keys: HashSet<u16>,
    buttons: HashSet<String>,
}

impl HeldInput {
    fn key(&mut self, vk: u16, down: bool) {
        if down {
            self.keys.insert(vk);
        } else {
            self.keys.remove(&vk);
        }
    }

    fn button(&mut self, button: &str, down: bool) {
        if down {
            self.buttons.insert(button.to_string());
        } else {
            self.buttons.remove(button);
        }
    }

    fn release(&mut self) {
        if self.keys.is_empty() && self.buttons.is_empty() {
            return;
        }
        platform::release_all(&self.keys, &self.buttons);
        self.keys.clear();
        self.buttons.clear();
    }
}

impl Drop for HeldInput {
    fn drop(&mut self) {
        self.release();
    }
}

/// 受控端：读取输入帧并注入（直到通道关闭/会话被踢/授权撤销）。
pub async fn serve(
    hub: Arc<ScreenHub>,
    grant: crate::peer::server::GrantCheck,
    stream: russh::ChannelStream<russh::server::Msg>,
) -> AppResult<()> {
    let (mut rd, mut wr) = tokio::io::split(stream);
    let mut session = String::new();
    let mut screen_cancel: Option<CancellationToken> = None;
    let mut held = HeldInput::default();
    let mut rects = platform::monitor_rects();
    let mut last_rects_refresh = std::time::Instant::now();
    // 注入事件可能上百条/秒：授权复核最多 1s 一次（撤销后 1s 内停止注入）
    let mut last_grant_check = std::time::Instant::now() - std::time::Duration::from_secs(2);
    loop {
        // 屏幕会话结束时立即收尾（即使查看端还没发下一条输入），由 HeldInput
        // 的 Drop 补发 keyup/mouseup。
        let next = match screen_cancel.as_ref() {
            Some(c) => tokio::select! {
                biased;
                _ = c.cancelled() => None,
                frame = tokio::time::timeout(INPUT_IDLE_TIMEOUT, read_frame(&mut rd)) => {
                    frame.ok().flatten()
                },
            },
            None => tokio::time::timeout(INPUT_IDLE_TIMEOUT, read_frame(&mut rd))
                .await
                .ok()
                .flatten(),
        };
        let Some(body) = next else {
            break;
        };
        let Ok(msg) = serde_json::from_slice::<InputMsg>(&body) else {
            continue;
        };
        match msg {
            InputMsg::Hello { session: s } => {
                if hub.allows_input(&s).await.is_some() {
                    applog!("[peer] 输入通道已授权: {s}");
                    // 重新握手前先释放上一轮按下的键/鼠标键。
                    held.release();
                    session = s.clone();
                    screen_cancel = hub.session_cancel(&s).await;
                    let _ = write_msg(&mut wr, &serde_json::json!({ "type": "ok" })).await;
                } else {
                    applog!("[peer] 输入通道拒绝: 会话未允许操作（{s}）");
                    let _ = write_msg(
                        &mut wr,
                        &serde_json::json!({ "type": "error", "detail": "该查看会话只允许查看" }),
                    )
                    .await;
                    return Err(AppError::EPeerNotAllowed(
                        "该查看会话只允许查看".to_string(),
                    ));
                }
            }
            other => {
                if session.is_empty() {
                    continue;
                }
                // 每条事件都复核屏幕会话的「允许操作」（撤销/踢出后立即失效）
                if hub.allows_input(&session).await.is_none() {
                    return Err(AppError::EPeerNotAllowed("操作授权已撤销".to_string()));
                }
                // 每 1s 再看一次 peers.toml：设备被删除或控制权限被撤销
                if last_grant_check.elapsed() >= std::time::Duration::from_secs(1) {
                    last_grant_check = std::time::Instant::now();
                    if !grant.still_granted(|p| p.grant_control).await {
                        applog!("[peer] 控制授权已撤销，停止输入注入: {session}");
                        return Err(AppError::EPeerNotAllowed("控制授权已撤销".to_string()));
                    }
                }
                match other {
                    InputMsg::Move { x, y, monitor } => {
                        // 显示器分辨率/旋转/排列可能在会话中变化；定期刷新几何，
                        // 否则后续点击会一直落在旧坐标上。
                        if monitor >= rects.len()
                            || last_rects_refresh.elapsed() >= std::time::Duration::from_secs(2)
                        {
                            rects = platform::monitor_rects();
                            last_rects_refresh = std::time::Instant::now();
                        }
                        if let Some(rect) = rects.get(monitor) {
                            platform::mouse_move(rect, x, y);
                        }
                    }
                    InputMsg::Button { button, down } => {
                        let button = normalize_button(&button);
                        held.button(button, down);
                        platform::mouse_button(button, down);
                    }
                    InputMsg::Wheel { delta } => platform::wheel(delta),
                    InputMsg::Key { vk, down } => {
                        held.key(vk, down);
                        platform::key(vk, down);
                    }
                    InputMsg::Text { s } => {
                        // 单帧最多 64 KiB；不设上限会让一个已授权对端用数万次
                        // SendInput 拖死注入线程。文本输入本身也不需要超大包。
                        const MAX_TEXT_CHARS: usize = 4096;
                        let text: String = s.chars().take(MAX_TEXT_CHARS).collect();
                        platform::text(&text);
                    }
                    InputMsg::Ping { t } => {
                        let _ = write_msg(&mut wr, &serde_json::json!({ "type": "pong", "t": t }))
                            .await;
                    }
                    InputMsg::Hello { .. } => {}
                }
            }
        }
    }
    Ok(())
}

async fn write_msg<W: AsyncWriteExt + Unpin>(
    wr: &mut W,
    v: &serde_json::Value,
) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(v)?;
    let mut framed = Vec::with_capacity(4 + bytes.len());
    framed.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    framed.extend_from_slice(&bytes);
    // 半开链路不能让 pong/ok 写入永久卡住输入泵（否则 screen_cancel 也没机会清理按键）。
    match tokio::time::timeout(std::time::Duration::from_secs(5), wr.write_all(&framed)).await {
        Ok(r) => r,
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "input write timeout",
        )),
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
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

/* ---------------- Windows 注入实现 ---------------- */

#[cfg(windows)]
mod platform {
    use std::collections::HashSet;

    use windows::Win32::UI::Input::KeyboardAndMouse::{
        MapVirtualKeyW, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT,
        KEYBD_EVENT_FLAGS, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE,
        MAPVK_VK_TO_VSC_EX, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
        MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
        MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEINPUT, VIRTUAL_KEY,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
        SM_YVIRTUALSCREEN,
    };

    /// 显示器虚拟坐标矩形（x, y, w, h）。
    ///
    /// 与屏幕采集共用同一套枚举（跳过休眠/断开的影子输出），保证注入坐标
    /// 与查看端选择的显示器索引一致。
    pub fn monitor_rects() -> Vec<(i32, i32, u32, u32)> {
        crate::peer::rd::monitor_rects()
    }

    fn virtual_screen() -> (i32, i32, i32, i32) {
        unsafe {
            let x = GetSystemMetrics(SM_XVIRTUALSCREEN);
            let y = GetSystemMetrics(SM_YVIRTUALSCREEN);
            let w = GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1);
            let h = GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1);
            (x, y, w, h)
        }
    }

    fn send(inputs: &[INPUT]) {
        unsafe {
            let _ = SendInput(inputs, std::mem::size_of::<INPUT>() as i32);
        }
    }

    /// 绝对移动：`x/y` 是相对该显示器的 0..1。
    pub fn mouse_move(rect: &(i32, i32, u32, u32), x: f64, y: f64) {
        let (vx, vy, vw, vh) = virtual_screen();
        let px = rect.0 as f64 + x.clamp(0.0, 1.0) * rect.2 as f64;
        let py = rect.1 as f64 + y.clamp(0.0, 1.0) * rect.3 as f64;
        let dx = ((px - vx as f64) * 65535.0 / vw as f64).round() as i32;
        let dy = ((py - vy as f64) * 65535.0 / vh as f64).round() as i32;
        send(&[INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx,
                    dy,
                    mouseData: 0,
                    dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }]);
    }

    pub fn mouse_button(button: &str, down: bool) {
        let flag = match (button, down) {
            ("right", true) => MOUSEEVENTF_RIGHTDOWN,
            ("right", false) => MOUSEEVENTF_RIGHTUP,
            ("middle", true) => MOUSEEVENTF_MIDDLEDOWN,
            ("middle", false) => MOUSEEVENTF_MIDDLEUP,
            (_, true) => MOUSEEVENTF_LEFTDOWN,
            (_, false) => MOUSEEVENTF_LEFTUP,
        };
        send(&[INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: 0,
                    dy: 0,
                    mouseData: 0,
                    dwFlags: flag,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }]);
    }

    pub fn wheel(delta: i32) {
        // 统一成 ±120 的整数倍（前端可能给出任意值）
        let steps = if delta == 0 { 0 } else { delta / delta.abs() };
        let data = (steps * 120) as u32;
        send(&[INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: 0,
                    dy: 0,
                    mouseData: data,
                    dwFlags: MOUSEEVENTF_WHEEL,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }]);
    }

    fn is_extended(vk: u16) -> bool {
        matches!(
            vk,
            0x21..=0x2E // PageUp/PageDown/End/Home/Arrows/Insert/Delete
                | 0x5B | 0x5C // Win
                | 0x6F // numpad divide
                | 0xA3 | 0xA5 // RCtrl / RAlt
        )
    }

    fn key_event(vk: u16, scan: u16, flags: KEYBD_EVENT_FLAGS) {
        send(&[INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(vk),
                    wScan: scan,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }]);
    }

    pub fn key(vk: u16, down: bool) {
        let scan = unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC_EX) as u16 };
        let mut flags = KEYBD_EVENT_FLAGS(0);
        if !down {
            flags |= KEYEVENTF_KEYUP;
        }
        if is_extended(vk) || (scan & 0xFF00) == 0xE000 {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        key_event(vk, scan & 0xFF, flags);
    }

    /// Unicode 文本：每个 UTF-16 码元一个 down+up（代理对会被拆成两个事件）。
    pub fn text(s: &str) {
        for unit in s.encode_utf16() {
            key_event(0, unit, KEYEVENTF_UNICODE);
            key_event(0, unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP);
        }
    }

    /// 断线/撤销时释放所有仍处于按下状态的键和鼠标键。
    pub fn release_all(keys: &HashSet<u16>, buttons: &HashSet<String>) {
        for button in buttons {
            mouse_button(button, false);
        }
        for vk in keys {
            key(*vk, false);
        }
    }
}

#[cfg(not(windows))]
mod platform {
    pub fn monitor_rects() -> Vec<(i32, i32, u32, u32)> {
        Vec::new()
    }
    pub fn mouse_move(_r: &(i32, i32, u32, u32), _x: f64, _y: f64) {}
    pub fn mouse_button(_b: &str, _d: bool) {}
    pub fn wheel(_d: i32) {}
    pub fn key(_vk: u16, _down: bool) {}
    pub fn text(_s: &str) {}
    pub fn release_all(
        _keys: &std::collections::HashSet<u16>,
        _buttons: &std::collections::HashSet<String>,
    ) {
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn button_names_are_normalized() {
        assert_eq!(normalize_button("left"), "left");
        assert_eq!(normalize_button("right"), "right");
        assert_eq!(normalize_button("middle"), "middle");
        assert_eq!(normalize_button("back"), "left");
    }

    #[test]
    fn hello_frame_parses_camel_case() {
        let m: InputMsg = serde_json::from_str(r#"{"type":"hello","session":"screen-1"}"#).unwrap();
        match m {
            InputMsg::Hello { session } => assert_eq!(session, "screen-1"),
            _ => panic!("expected hello"),
        }
        let mv: InputMsg =
            serde_json::from_str(r#"{"type":"move","x":0.5,"y":0.25,"monitor":1}"#).unwrap();
        match mv {
            InputMsg::Move { x, y, monitor } => {
                assert!((x - 0.5).abs() < f64::EPSILON);
                assert!((y - 0.25).abs() < f64::EPSILON);
                assert_eq!(monitor, 1);
            }
            _ => panic!("expected move"),
        }
    }
}
