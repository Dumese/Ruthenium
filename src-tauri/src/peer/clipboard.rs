//! 剪贴板文本同步（`subsystem("ruthenium-clipboard")`）。
//!
//! 安全边界：
//! - 持久授权 `grant_clipboard`（默认关）+ 屏幕查看会话令牌（随机 UUID）；
//! - 只同步 **Unicode 文本**（`CF_UNICODETEXT`），不传图片/文件/自定义格式；
//! - 查看端默认不同步：用户在工具栏显式开启本次会话的剪贴板通道后才可拉取/发送；
//! - 每条消息都复核会话仍然存在（查看结束/被踢后立即失效）。

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_util::sync::CancellationToken;

use crate::error::{AppError, AppResult};
use crate::peer::rd::ScreenHub;
use crate::state::AppState;

pub const SUBSYSTEM: &str = "ruthenium-clipboard";
/// 文本长度上限（防止误传超大内容；16 MiB UTF-16 ≈ 800 万字符）。
const MAX_TEXT_BYTES: usize = 16 * 1024 * 1024;

/// 查看端打开剪贴板通道的返回：通道 id + 远端当前剪贴板文本。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClipboardStart {
    pub id: String,
    pub text: String,
}

struct ClipSession {
    tx: mpsc::Sender<Vec<u8>>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    cancel: CancellationToken,
}

#[derive(Default, Clone)]
pub struct ClipboardManager {
    sessions: Arc<Mutex<HashMap<String, ClipSession>>>,
}

impl ClipboardManager {
    pub fn new() -> Self {
        Self::default()
    }

    async fn insert(&self, id: String, s: ClipSession) {
        self.sessions.lock().await.insert(id, s);
    }

    /// 发一次请求并等待同 id 的应答（5s 超时）。
    async fn request(&self, id: &str, msg: &Value) -> Option<Value> {
        let (tx, pending) = {
            let map = self.sessions.lock().await;
            let s = map.get(id)?;
            (s.tx.clone(), s.pending.clone())
        };
        let req_id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(1);
        let mut msg = msg.clone();
        if let Some(obj) = msg.as_object_mut() {
            obj.insert("id".to_string(), Value::from(req_id));
        }
        let (rtx, rrx) = oneshot::channel();
        pending.lock().await.insert(req_id, rtx);
        let Ok(bytes) = serde_json::to_vec(&msg) else {
            return None;
        };
        let mut framed = Vec::with_capacity(4 + bytes.len());
        framed.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        framed.extend_from_slice(&bytes);
        if tx.send(framed).await.is_err() {
            return None;
        }
        match tokio::time::timeout(std::time::Duration::from_secs(5), rrx).await {
            Ok(Ok(v)) => Some(v),
            _ => {
                pending.lock().await.remove(&req_id);
                None
            }
        }
    }

    pub async fn get_text(&self, id: &str) -> Result<String, String> {
        let v = self
            .request(id, &serde_json::json!({ "type": "get" }))
            .await
            .ok_or_else(|| "对端未响应剪贴板请求".to_string())?;
        if v.get("type").and_then(|x| x.as_str()) != Some("text") {
            return Err(v
                .get("detail")
                .and_then(|x| x.as_str())
                .unwrap_or("对端拒绝读取剪贴板")
                .to_string());
        }
        Ok(v.get("text")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string())
    }

    pub async fn set_text(&self, id: &str, text: &str) -> Result<bool, String> {
        let v = self
            .request(id, &serde_json::json!({ "type": "set", "text": text }))
            .await
            .ok_or_else(|| "对端未响应剪贴板请求".to_string())?;
        if v.get("type").and_then(|x| x.as_str()) != Some("ok") {
            return Err(v
                .get("detail")
                .and_then(|x| x.as_str())
                .unwrap_or("对端拒绝写入剪贴板")
                .to_string());
        }
        Ok(true)
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

/// 为某条屏幕会话打开剪贴板通道（令牌 = 服务端下发的 auth token）。
pub async fn open(
    state: &AppState,
    screen_session_id: &str,
    token: Option<&str>,
) -> AppResult<ClipboardStart> {
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
    let mut handle = crate::peer::client::connect(state, &peer, "剪贴板同步").await?;
    crate::peer::client::authenticate(&mut handle, &identity).await?;

    // 实时复核（授权可能刚被撤销）
    let info = crate::peer::control::query_info(&handle, None).await?;
    state.peer.sync_peer_allows(&peer.id, &info).await;
    if !info.grant_clipboard {
        return Err(AppError::EPeerNotAllowed(format!(
            "{} 未允许剪贴板同步（可能已撤销授权）",
            peer.name
        )));
    }

    let mut channel = handle
        .channel_open_session()
        .await
        .map_err(|e| AppError::ESshChannel(format!("打开剪贴板通道失败: {e}")))?;
    channel
        .request_subsystem(true, SUBSYSTEM)
        .await
        .map_err(|e| AppError::EPeerNotAllowed(format!("对端拒绝剪贴板通道: {e}")))?;
    match tokio::time::timeout(std::time::Duration::from_secs(5), channel.wait()).await {
        Ok(Some(russh::ChannelMsg::Success)) => {}
        Ok(Some(russh::ChannelMsg::Failure)) => {
            return Err(AppError::EPeerNotAllowed(
                "对端拒绝剪贴板同步：未授权或授权已撤销".to_string(),
            ))
        }
        Ok(None) => {
            return Err(AppError::EPeerNotAllowed(
                "对端关闭了剪贴板通道".to_string(),
            ))
        }
        Err(_) => {
            return Err(AppError::EPeerNotAllowed(
                "对端未响应剪贴板请求".to_string(),
            ))
        }
        Ok(Some(_)) => {}
    }

    let stream = channel.into_stream();
    let (mut rd, mut wr) = tokio::io::split(stream);
    let cancel = CancellationToken::new();
    let id = uuid::Uuid::new_v4().to_string();
    let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>> =
        Arc::new(Mutex::new(HashMap::new()));

    // hello：带令牌，服务端返回当前剪贴板文本
    let hello = serde_json::json!({ "type": "hello", "session": token, "id": 0 });
    let hello_bytes = serde_json::to_vec(&hello)
        .map_err(|e| AppError::EInvalidInput(format!("hello 序列化失败: {e}")))?;
    let mut framed = Vec::with_capacity(4 + hello_bytes.len());
    framed.extend_from_slice(&(hello_bytes.len() as u32).to_be_bytes());
    framed.extend_from_slice(&hello_bytes);
    wr.write_all(&framed)
        .await
        .map_err(|e| AppError::ESshChannel(format!("发送剪贴板握手失败: {e}")))?;
    let ack = tokio::time::timeout(std::time::Duration::from_secs(5), read_frame(&mut rd)).await;
    let initial = match ack {
        Ok(Some(body)) => {
            let v: Value = serde_json::from_slice(&body).unwrap_or_default();
            if v.get("type").and_then(|x| x.as_str()) != Some("text") {
                let detail = v
                    .get("detail")
                    .and_then(|x| x.as_str())
                    .unwrap_or("对端拒绝剪贴板同步");
                return Err(AppError::EPeerNotAllowed(detail.to_string()));
            }
            v.get("text")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string()
        }
        Ok(None) => {
            return Err(AppError::EPeerNotAllowed(
                "对端拒绝剪贴板同步：授权已撤销".to_string(),
            ))
        }
        Err(_) => {
            return Err(AppError::EPeerNotAllowed(
                "对端未响应剪贴板握手".to_string(),
            ))
        }
    };

    // 读端：分发应答 + 保持连接（handle 由该任务持有）
    {
        let cancel = cancel.clone();
        let pending = pending.clone();
        // 通道结束（对端断开 / 授权撤销 / 本端 stop）时通知前端收起「已开启」
        // 状态，并清掉管理器条目，后续 get/set 立即失败而不是等 5s 超时。
        let app = state.app.clone();
        let manager = state.clipboard.clone();
        let screen_id = screen_session_id.to_string();
        let clip_id = id.clone();
        tokio::spawn(async move {
            let _keep = handle;
            loop {
                let next = tokio::select! {
                    _ = cancel.cancelled() => None,
                    frame = read_frame(&mut rd) => frame,
                };
                let Some(body) = next else { break };
                let Ok(v) = serde_json::from_slice::<Value>(&body) else {
                    continue;
                };
                if let Some(rid) = v.get("id").and_then(|x| x.as_u64()) {
                    if let Some(tx) = pending.lock().await.remove(&rid) {
                        let _ = tx.send(v);
                    }
                }
            }
            pending.lock().await.clear();
            manager.close(&clip_id).await;
            crate::events::emit(
                &app,
                crate::events::PEER_CLIPBOARD,
                crate::events::ClipboardEndedPayload {
                    session_id: screen_id,
                    id: clip_id,
                },
            );
        });
    }
    // 写端
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<Vec<u8>>(16);
    {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    msg = cmd_rx.recv() => match msg {
                        Some(bytes) => { if wr.write_all(&bytes).await.is_err() { break; } }
                        None => break,
                    }
                }
            }
            let _ = wr.shutdown().await;
        });
    }

    state
        .clipboard
        .insert(
            id.clone(),
            ClipSession {
                tx: cmd_tx,
                pending,
                cancel,
            },
        )
        .await;
    applog!(
        "[peer] 剪贴板同步已开启: {}（{} 字符）",
        peer.name,
        initial.chars().count()
    );
    Ok(ClipboardStart { id, text: initial })
}

/* ---------------- 受控端 ---------------- */

/// 受控端：服务剪贴板读写，直到通道关闭或会话失效。
///
/// 每条消息处理前复核 `grant_clipboard` 实时授权（撤销/删除设备即断开）。
pub async fn serve(
    hub: Arc<ScreenHub>,
    grant_check: crate::peer::server::GrantCheck,
    stream: russh::ChannelStream<russh::server::Msg>,
) -> AppResult<()> {
    let (mut rd, mut wr) = tokio::io::split(stream);
    let mut token = String::new();
    let mut screen_cancel: Option<CancellationToken> = None;
    loop {
        let next = match screen_cancel.as_ref() {
            Some(c) => tokio::select! {
                _ = c.cancelled() => None,
                f = read_frame(&mut rd) => f,
            },
            None => read_frame(&mut rd).await,
        };
        let Some(body) = next else { break };
        if body.len() > MAX_TEXT_BYTES {
            break;
        }
        // 会话建立后逐条复核授权（hello 前没有会话，跳过）
        if !token.is_empty() && !grant_check.still_granted(|p| p.grant_clipboard).await {
            applog!("[peer] 剪贴板授权已撤销，断开通道");
            return Err(AppError::EPeerNotAllowed("剪贴板授权已撤销".to_string()));
        }
        let Ok(v) = serde_json::from_slice::<Value>(&body) else {
            continue;
        };
        let id = v.get("id").and_then(|x| x.as_u64()).unwrap_or(0);
        match v.get("type").and_then(|x| x.as_str()).unwrap_or("") {
            "hello" => {
                let s = v
                    .get("session")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                if !hub.has_session(&s).await {
                    let _ = write_msg(
                        &mut wr,
                        &serde_json::json!({ "type": "error", "id": id, "detail": "查看会话不存在或已结束" }),
                    )
                    .await;
                    return Err(AppError::EPeerNotAllowed(
                        "查看会话不存在或已结束".to_string(),
                    ));
                }
                // hello 也必须实时复核授权：开通道时用的 peer 记录可能已过期，
                // 不能让已撤销剪贴板的设备靠新开通道读一次。
                if !grant_check.still_granted(|p| p.grant_clipboard).await {
                    let _ = write_msg(
                        &mut wr,
                        &serde_json::json!({ "type": "error", "id": id, "detail": "剪贴板授权已撤销" }),
                    )
                    .await;
                    return Err(AppError::EPeerNotAllowed("剪贴板授权已撤销".to_string()));
                }
                screen_cancel = hub.session_cancel(&s).await;
                token = s;
                applog!("[peer] 剪贴板通道已授权: {token}");
                let text = platform::get_text().unwrap_or_default();
                let _ = write_msg(
                    &mut wr,
                    &serde_json::json!({ "type": "text", "id": id, "text": text }),
                )
                .await;
            }
            "get" => {
                if !hub.has_session(&token).await {
                    return Err(AppError::EPeerNotAllowed("查看会话已结束".to_string()));
                }
                let text = platform::get_text().unwrap_or_default();
                let _ = write_msg(
                    &mut wr,
                    &serde_json::json!({ "type": "text", "id": id, "text": text }),
                )
                .await;
            }
            "set" => {
                if !hub.has_session(&token).await {
                    return Err(AppError::EPeerNotAllowed("查看会话已结束".to_string()));
                }
                let text = v.get("text").and_then(|x| x.as_str()).unwrap_or("");
                let ok = platform::set_text(text);
                let _ = write_msg(
                    &mut wr,
                    &serde_json::json!({
                        "type": if ok { "ok" } else { "error" },
                        "id": id,
                        "detail": if ok { "" } else { "写入对端剪贴板失败" },
                    }),
                )
                .await;
            }
            _ => {}
        }
    }
    Ok(())
}

async fn write_msg<W: AsyncWriteExt + Unpin>(wr: &mut W, v: &Value) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(v)?;
    let mut framed = Vec::with_capacity(4 + bytes.len());
    framed.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    framed.extend_from_slice(&bytes);
    wr.write_all(&framed).await
}

async fn read_frame<R: AsyncReadExt + Unpin>(rd: &mut R) -> Option<Vec<u8>> {
    let mut len = [0u8; 4];
    rd.read_exact(&mut len).await.ok()?;
    let n = u32::from_be_bytes(len) as usize;
    if n == 0 || n > MAX_TEXT_BYTES {
        return None;
    }
    let mut buf = vec![0u8; n];
    rd.read_exact(&mut buf).await.ok()?;
    Some(buf)
}

/// 读取本机剪贴板（查看端按钮用）。
pub fn local_get() -> String {
    platform::get_text().unwrap_or_default()
}

/// 写入本机剪贴板（查看端按钮用）。
pub fn local_set(text: &str) -> bool {
    platform::set_text(text)
}

/* ---------------- Windows 剪贴板 ---------------- */

#[cfg(windows)]
mod platform {
    use windows::Win32::Foundation::{HANDLE, HGLOBAL};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    use windows::Win32::System::Ole::CF_UNICODETEXT;

    /// 读取剪贴板文本（非文本/打不开 → None）。
    pub fn get_text() -> Option<String> {
        unsafe {
            if OpenClipboard(None).is_err() {
                return None;
            }
            let out = (|| {
                let handle = GetClipboardData(CF_UNICODETEXT.0 as u32).ok()?;
                let ptr = GlobalLock(HGLOBAL(handle.0)) as *const u16;
                if ptr.is_null() {
                    return None;
                }
                let mut len = 0usize;
                // 扫描到 NUL（上限 4M 码元，避免异常数据导致死循环）
                while len < 4 * 1024 * 1024 && *ptr.add(len) != 0 {
                    len += 1;
                }
                let s = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
                let _ = GlobalUnlock(HGLOBAL(handle.0));
                Some(s)
            })();
            let _ = CloseClipboard();
            out
        }
    }

    /// 写入剪贴板文本；成功返回 true（系统接管内存所有权）。
    pub fn set_text(text: &str) -> bool {
        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let bytes = wide.len() * 2;
        unsafe {
            if OpenClipboard(None).is_err() {
                return false;
            }
            let _ = EmptyClipboard();
            let ok = (|| {
                let h = GlobalAlloc(GMEM_MOVEABLE, bytes).ok()?;
                let p = GlobalLock(h) as *mut u16;
                if p.is_null() {
                    return None;
                }
                std::ptr::copy_nonoverlapping(wide.as_ptr(), p, wide.len());
                let _ = GlobalUnlock(h);
                SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(h.0))).ok()
            })();
            let _ = CloseClipboard();
            ok.is_some()
        }
    }
}

#[cfg(not(windows))]
mod platform {
    pub fn get_text() -> Option<String> {
        None
    }
    pub fn set_text(_text: &str) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_payload_is_camel_case() {
        let v = serde_json::to_value(ClipboardStart {
            id: "c1".into(),
            text: "hi".into(),
        })
        .unwrap();
        assert_eq!(v["id"], "c1");
        assert_eq!(v["text"], "hi");
    }

    #[test]
    fn clipboard_text_roundtrip() {
        let text = format!("Ruthenium 剪贴板 {}", uuid::Uuid::new_v4());
        let old = platform::get_text();
        assert!(platform::set_text(&text), "set_text should succeed");
        assert_eq!(platform::get_text().as_deref(), Some(text.as_str()));
        // 尽力恢复原内容，避免污染开发者剪贴板
        if let Some(old) = old {
            let _ = platform::set_text(&old);
        }
    }
}
