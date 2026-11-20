//! 远程桌面音频：WASAPI loopback 采集 → Opus 编码 → 独立 SSH 通道。
//!
//! 设计：
//! - **采集**：默认渲染设备的 WASAPI loopback（共享模式，只收系统播放，不碰麦克风）；
//!   `GetMixFormat` 自适应采样率/声道，必要时混成 48kHz 立体声（非 48k 用线性重采样）。
//! - **编码**：Opus 48kHz 立体声、20ms 帧、64kbps VBR + DTX（静音时几乎不发包）。
//! - **传输**：`subsystem("ruthenium-audio")`；帧格式 `u32 长度 + [kind][payload]`
//!   （1=格式 JSON，2=数据 `[u32 seq][opus]`，3=错误文本）。令牌 = 屏幕会话 id。
//! - **授权**：`grant_audio`；查看端默认关，用户显式开启本次会话。
//! - **安全**：只传 Opus 音频帧，不做麦克风采集；会话结束/被踢/撤销后通道立即失效。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;

use crate::error::{AppError, AppResult};
use crate::peer::rd::ScreenHub;
use crate::state::AppState;

pub const SUBSYSTEM: &str = "ruthenium-audio";
const RATE: u32 = 48_000;
const FRAME_MS: u32 = 20;
const FRAME_SAMPLES: usize = RATE as usize * FRAME_MS as usize / 1000; // 960

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
    Error {
        session_id: String,
        detail: String,
    },
}

struct AudioSession {
    cancel: CancellationToken,
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
        .insert(id.clone(), AudioSession { cancel })
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
    Error(String),
}

/// 受查看端：采集 → 编码 → 通道（屏幕会话结束/被踢/写失败即停止）。
///
/// `grant_check` 每 5s 复核一次授权撤销（只在通道建立时查一次不够，
/// 撤销后已在传的音频会一直流到屏幕会话结束）。
pub async fn serve(
    hub: Arc<ScreenHub>,
    grant_check: crate::peer::server::GrantCheck,
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
    let (tx, mut rx) = mpsc::channel::<Out>(32);
    {
        let stop = stop.clone();
        let tx = tx.clone();
        std::thread::spawn(move || {
            capture_loop(tx, stop);
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
    let _ = wr.shutdown().await;
    Ok(())
}

fn capture_loop(tx: mpsc::Sender<Out>, stop: Arc<AtomicBool>) {
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
    while !stop.load(Ordering::Relaxed) {
        if let Err(e) = cap.read_into(&mut pending) {
            applog!("[peer] 音频采集中断: {e}");
            let _ = tx.blocking_send(Out::Error(e));
            break;
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
                        return;
                    }
                    seq = seq.wrapping_add(1);
                }
                Ok(_) => {}
                Err(e) => {
                    applog!("[peer] Opus 编码失败: {e:?}");
                    let _ = tx.blocking_send(Out::Error(format!("Opus 编码失败: {e:?}")));
                    return;
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

fn opus_encoder() -> Result<audiopus::coder::Encoder, String> {
    use audiopus::{Application, Bitrate, Channels, SampleRate, Signal};
    let mut enc =
        audiopus::coder::Encoder::new(SampleRate::Hz48000, Channels::Stereo, Application::Audio)
            .map_err(|e| format!("创建 Opus 编码器失败: {e:?}"))?;
    let _ = enc.set_bitrate(Bitrate::BitsPerSecond(64_000));
    let _ = enc.set_vbr(true);
    let _ = enc.set_complexity(5);
    let _ = enc.set_signal(Signal::Music);
    let _ = enc.set_dtx(true);
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

/* ---------------- WASAPI loopback ---------------- */

#[cfg(windows)]
mod platform {
    use super::*;
    use windows::core::GUID;
    use windows::Win32::Media::Audio::{
        eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator,
        AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK,
        WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED,
    };

    const CLSID_MM_DEVICE_ENUMERATOR: GUID =
        GUID::from_u128(0xBCDE0395_E52F_467C_8E3D_C4579291692E);
    const WAVE_FORMAT_PCM: u32 = 1;
    const WAVE_FORMAT_IEEE_FLOAT: u32 = 3;
    const WAVE_FORMAT_EXTENSIBLE: u32 = 65534;
    const KSDATAFORMAT_SUBTYPE_PCM: GUID = GUID::from_u128(0x00000001_0000_0010_8000_00AA00389B71);
    const KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: GUID =
        GUID::from_u128(0x00000003_0000_0010_8000_00AA00389B71);

    pub struct Loopback {
        client: IAudioClient,
        capture: IAudioCaptureClient,
        channels: usize,
        rate: u32,
        block_align: usize,
        float: bool,
        bits: u16,
        resample_pos: f64,
    }

    impl Loopback {
        pub fn new() -> Result<Self, String> {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                let enumerator: IMMDeviceEnumerator =
                    CoCreateInstance(&CLSID_MM_DEVICE_ENUMERATOR, None, CLSCTX_ALL)
                        .map_err(|e| format!("创建设备枚举失败: {e}"))?;
                let device = enumerator
                    .GetDefaultAudioEndpoint(eRender, eConsole)
                    .map_err(|e| format!("没有默认播放设备: {e}"))?;
                let client: IAudioClient = device
                    .Activate(CLSCTX_ALL, None)
                    .map_err(|e| format!("激活音频客户端失败: {e}"))?;
                let fmt = client
                    .GetMixFormat()
                    .map_err(|e| format!("读取混音格式失败: {e}"))?;
                let (channels, rate, block_align, float, bits) = parse_format(fmt)?;
                let init = client.Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    AUDCLNT_STREAMFLAGS_LOOPBACK,
                    2_000_000,
                    0,
                    fmt,
                    None,
                );
                CoTaskMemFree(Some(fmt as *const core::ffi::c_void));
                init.map_err(|e| format!("初始化 WASAPI 失败: {e}"))?;
                let capture: IAudioCaptureClient = client
                    .GetService()
                    .map_err(|e| format!("获取采集客户端失败: {e}"))?;
                client
                    .Start()
                    .map_err(|e| format!("启动音频采集失败: {e}"))?;
                applog!(
                    "[peer] 音频采集: {rate}Hz {channels}ch {bits}bit {}",
                    if float { "float" } else { "pcm" }
                );
                Ok(Self {
                    client,
                    capture,
                    channels,
                    rate,
                    block_align,
                    float,
                    bits,
                    resample_pos: 0.0,
                })
            }
        }

        /// 读取当前可用的所有包，追加 48kHz 立体声 f32 到 `out`。
        pub fn read_into(&mut self, out: &mut Vec<f32>) -> Result<(), String> {
            unsafe {
                loop {
                    let n = self
                        .capture
                        .GetNextPacketSize()
                        .map_err(|e| format!("查询音频包失败: {e}"))?;
                    if n == 0 {
                        break;
                    }
                    let mut data: *mut u8 = std::ptr::null_mut();
                    let mut frames = 0u32;
                    let mut flags = 0u32;
                    self.capture
                        .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
                        .map_err(|e| format!("读取音频包失败: {e}"))?;
                    let mut raw: Vec<f32> = Vec::with_capacity(frames as usize * 2);
                    if flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0 || data.is_null() {
                        raw.resize(frames as usize * 2, 0.0);
                    } else {
                        for i in 0..frames as usize {
                            let base = data.add(i * self.block_align);
                            let l = self.sample(base, 0);
                            let r = if self.channels >= 2 {
                                self.sample(base, 1)
                            } else {
                                l
                            };
                            raw.push(l);
                            raw.push(r);
                        }
                    }
                    self.capture
                        .ReleaseBuffer(frames)
                        .map_err(|e| format!("释放音频包失败: {e}"))?;
                    if self.rate == RATE {
                        out.extend_from_slice(&raw);
                    } else {
                        self.resample_into(&raw, out);
                    }
                }
            }
            Ok(())
        }

        unsafe fn sample(&self, base: *const u8, channel: usize) -> f32 {
            let bytes = (self.bits / 8) as usize;
            let p = base.add(channel * bytes);
            if self.float {
                f32::from_le_bytes([*p, *p.add(1), *p.add(2), *p.add(3)])
            } else if self.bits == 16 {
                i16::from_le_bytes([*p, *p.add(1)]) as f32 / 32_768.0
            } else {
                let v = i32::from_le_bytes([*p, *p.add(1), *p.add(2), *p.add(3)]);
                v as f32 / 2_147_483_648.0
            }
        }

        fn resample_into(&mut self, input: &[f32], out: &mut Vec<f32>) {
            let frames = input.len() / 2;
            if frames == 0 {
                return;
            }
            let ratio = RATE as f64 / self.rate as f64;
            let mut pos = self.resample_pos.max(0.0);
            while pos < frames as f64 {
                let i = pos as usize;
                if i >= frames {
                    break;
                }
                out.push(input[i * 2]);
                out.push(input[i * 2 + 1]);
                pos += ratio;
            }
            self.resample_pos = pos - frames as f64;
        }
    }

    impl Drop for Loopback {
        fn drop(&mut self) {
            unsafe {
                let _ = self.client.Stop();
            }
        }
    }

    fn parse_format(fmt: *const WAVEFORMATEX) -> Result<(usize, u32, usize, bool, u16), String> {
        unsafe {
            if fmt.is_null() {
                return Err("混音格式为空".to_string());
            }
            let wfx = std::ptr::read_unaligned(fmt);
            let mut tag = wfx.wFormatTag as u32;
            let mut bits = wfx.wBitsPerSample;
            if tag == WAVE_FORMAT_EXTENSIBLE {
                let ext = std::ptr::read_unaligned(fmt as *const WAVEFORMATEXTENSIBLE);
                let sub = ext.SubFormat;
                if sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT {
                    tag = WAVE_FORMAT_IEEE_FLOAT;
                } else if sub == KSDATAFORMAT_SUBTYPE_PCM {
                    tag = WAVE_FORMAT_PCM;
                }
                if bits == 0 {
                    bits = ext.Samples.wValidBitsPerSample;
                }
            }
            let float =
                tag == WAVE_FORMAT_IEEE_FLOAT || (tag == WAVE_FORMAT_EXTENSIBLE && bits == 32);
            if !float && tag != WAVE_FORMAT_PCM {
                return Err(format!("不支持的音频格式 tag={tag} bits={bits}"));
            }
            if !float && bits != 16 && bits != 32 {
                return Err(format!("不支持的 PCM 位深 {bits}"));
            }
            let channels = wfx.nChannels as usize;
            if channels == 0 {
                return Err("音频通道数为 0".to_string());
            }
            Ok((
                channels,
                wfx.nSamplesPerSec,
                wfx.nBlockAlign as usize,
                float,
                bits,
            ))
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
}
