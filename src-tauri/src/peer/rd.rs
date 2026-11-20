//! 远程桌面（只看）：DXGI 采集 + GDI+ JPEG + 独立 SSH 帧流。
//!
//! 设计：
//! - **采集**：DXGI Desktop Duplication，每个显示器一个 duplication；`ACCESS_LOST`
//!   自动重建；`AcquireNextFrame` 超时 = 画面没变化 → **不发帧**（静止≈0 带宽）。
//! - **编码**：GDI+ JPEG（BGRA 直吃，内存流，零新增 crate）；独立线程编码，帧队列
//!   长度 1（满则丢最旧）。
//! - **传输**：独立 SSH 连接 + `subsystem("ruthenium-screen")`；每帧
//!   `RSCR + 版本 + 宽 + 高 + 长度 + JPEG`，控制消息 `RSCJ + 长度 + JSON`。
//! - **授权**：`grant_screen` + **每次本机确认**（30s 超时拒绝）+ 可选 Windows Hello；
//!   被查看端有常驻横幅，可「立即断开」。
//! - **只看**：服务端不接收任何输入注入（输入注入走独立子系统）。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::ipc::Channel;
use tauri::Emitter;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::peer::client;
use crate::peer::peers::PeerRecord;
use crate::state::AppState;

/// 帧魔数 / 控制帧魔数。
const FRAME_MAGIC: [u8; 4] = *b"RSCR";
const CTRL_MAGIC: [u8; 4] = *b"RSCJ";
/// 协议版本：2 = 28 字节帧头（含时间戳/编解码标志），支持 H.264。
const PROTO_VERSION: u8 = 2;
/// 本机确认等待上限（超时按拒绝）。
pub const SCREEN_CONSENT_TIMEOUT: Duration = Duration::from_secs(30);
/// 查看端等待握手回执的上限：必须覆盖本机确认（30s）+ 可选 Windows Hello
/// 的用户交互时间。取 5s 会让用户在确认弹窗上几乎来不及点「允许」，
/// 查看端先超时、服务端仍在等待，最终报「对端未响应屏幕请求」。
pub const SCREEN_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(120);
/// JPEG 默认帧率（H.264 会在 10–30fps 间自适应）。
const JPEG_DEFAULT_FPS: u32 = 15;
/// 静止画面强制补一帧的间隔（丢帧/客户端后加入时能恢复）。
const KEYFRAME_INTERVAL: Duration = Duration::from_secs(5);
/// 帧/控制消息的写超时：对端不读时（TCP 缓冲满）不能把任务永久卡住。
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/* ---------------- 显示器列表 ---------------- */

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MonitorInfo {
    pub index: usize,
    pub width: u32,
    pub height: u32,
    pub name: String,
}

/// 可共享的顶层窗口。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowInfo {
    pub hwnd: u64,
    pub title: String,
}

/// 光标形状（供前端 canvas 绘制）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CursorShapeInfo {
    pub width: u32,
    pub height: u32,
    pub hotspot_x: i32,
    pub hotspot_y: i32,
    /// RGBA，base64
    pub rgba: String,
}

/* ---------------- 受控端（被查看） ---------------- */

/// 查看请求的确认结果（可单独允许键鼠操作，默认只看）。
#[derive(Debug, Clone, Copy)]
pub struct ScreenDecision {
    pub allow: bool,
    pub allow_input: bool,
}

/// 受查看会话（横幅 / 审计 / 踢出）。
pub struct ViewSession {
    pub peer_name: String,
    pub peer_addr: String,
    pub quality: u8,
    pub monitor: usize,
    /// 本次查看是否允许对端注入键鼠（默认 false = 只看）
    pub allow_input: bool,
    pub started_ms: i64,
    pub cancel: CancellationToken,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewSessionView {
    pub session_id: String,
    pub peer_name: String,
    pub peer_addr: String,
    pub quality: u8,
    pub monitor: usize,
    pub started_ms: i64,
    pub elapsed_sec: u64,
}

/// 查看请求弹窗 payload（`peer://screen-request`）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScreenRequest {
    pub consent_id: String,
    pub peer_name: String,
    pub peer_fp: String,
    pub addr: String,
    pub hello_required: bool,
    /// 本机确认时可勾选「允许操作」（默认不勾）
    pub input_requested: bool,
    /// 仅请求操作权限（会话已建立，不是新的查看请求）
    #[serde(default)]
    pub input_only: bool,
}

/// 受查看会话开始/结束（`peer://screen-session`）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScreenSessionEvent {
    pub action: String,
    pub session_id: String,
    pub peer_name: String,
    pub reason: Option<String>,
}

/// 控制消息（客户端 → 服务端）。
#[derive(Debug, Deserialize)]
#[serde(tag = "cmd", rename_all = "camelCase")]
enum ClientCmd {
    Monitors,
    Monitor {
        index: usize,
    },
    Quality {
        q: u8,
    },
    /// 请求切换编码（`h264` / `jpeg`）
    Codec {
        codec: String,
    },
    /// 请求下一个 IDR（解码器重新同步）
    Keyframe,
    /// 请求可共享窗口列表
    Windows,
    /// 切到指定窗口
    Window {
        hwnd: u64,
        #[serde(default)]
        title: Option<String>,
    },
    /// 切回整个桌面
    Desktop,
    /// 在已建立的会话上申请操作（键鼠）权限
    RequestInput,
    /// 屏幕通道延迟探测
    Ping {
        t: i64,
    },
    /// 查看端回报 RTT（服务端自适应用）
    Stats {
        #[serde(rename = "rttMs")]
        rtt_ms: u64,
    },
}

/// 控制消息（服务端 → 客户端）。
#[derive(Debug, Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
enum ServerMsg<'a> {
    Monitors {
        items: &'a [MonitorInfo],
    },
    Auth {
        token: &'a str,
        allow_input: bool,
        clipboard: bool,
        audio: bool,
    },
}

/// 受查看端状态。
#[derive(Default)]
pub struct ScreenHub {
    consents: Mutex<HashMap<String, oneshot::Sender<ScreenDecision>>>,
    sessions: Mutex<HashMap<String, ViewSession>>,
    /// peerId → 最近一次弹确认框的时间（防已配对设备反复刷确认框）
    prompts: Mutex<HashMap<String, Instant>>,
}

impl ScreenHub {
    pub fn new() -> Self {
        Self::default()
    }

    /// 确认框频控：同一设备 5s 内只弹一次。
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

    pub async fn request_consent(&self, consent_id: String) -> oneshot::Receiver<ScreenDecision> {
        let (tx, rx) = oneshot::channel();
        self.consents.lock().await.insert(consent_id, tx);
        rx
    }

    pub async fn decide(&self, consent_id: &str, allow: bool, allow_input: bool) -> bool {
        match self.consents.lock().await.remove(consent_id) {
            Some(tx) => tx.send(ScreenDecision { allow, allow_input }).is_ok(),
            None => false,
        }
    }

    pub async fn cancel_consent(&self, consent_id: &str) {
        self.consents.lock().await.remove(consent_id);
    }

    pub async fn register(&self, session_id: String, s: ViewSession) {
        self.sessions.lock().await.insert(session_id, s);
    }

    pub async fn finish(&self, session_id: &str) -> Option<ViewSession> {
        let s = self.sessions.lock().await.remove(session_id);
        // 会话结束后让输入/音频/剪贴板通道也尽快退出，而不是等下一条事件。
        if let Some(ref s) = s {
            s.cancel.cancel();
        }
        s
    }

    pub async fn list(&self) -> Vec<ViewSessionView> {
        let now = now_ms();
        let mut out: Vec<ViewSessionView> = self
            .sessions
            .lock()
            .await
            .iter()
            .map(|(id, s)| ViewSessionView {
                session_id: id.clone(),
                peer_name: s.peer_name.clone(),
                peer_addr: s.peer_addr.clone(),
                quality: s.quality,
                monitor: s.monitor,
                started_ms: s.started_ms,
                elapsed_sec: ((now - s.started_ms).max(0) / 1000) as u64,
            })
            .collect();
        let _ = now;
        out.sort_by_key(|a| a.started_ms);
        out
    }

    /// 会话是否仍然存在（剪贴板/音频通道校验用）。
    pub async fn has_session(&self, session_id: &str) -> bool {
        self.sessions.lock().await.contains_key(session_id)
    }

    /// 取会话的取消令牌（音频通道跟随屏幕会话一起结束）。
    pub async fn session_cancel(&self, session_id: &str) -> Option<CancellationToken> {
        self.sessions
            .lock()
            .await
            .get(session_id)
            .map(|s| s.cancel.clone())
    }

    /// 输入子系统鉴权：令牌 = 屏幕会话 id；返回该会话当前显示器索引。
    pub async fn allows_input(&self, token: &str) -> Option<usize> {
        self.sessions
            .lock()
            .await
            .get(token)
            .filter(|s| s.allow_input && !s.cancel.is_cancelled())
            .map(|s| s.monitor)
    }

    /// 会话进行中用户同意「操作」后，打开该会话的输入授权。
    pub async fn set_allow_input(&self, session_id: &str, allow: bool) {
        if let Some(s) = self.sessions.lock().await.get_mut(session_id) {
            s.allow_input = allow;
        }
    }

    pub async fn kick(&self, session_id: &str) -> bool {
        match self.sessions.lock().await.get(session_id) {
            Some(s) => {
                s.cancel.cancel();
                true
            }
            None => false,
        }
    }

    /// 停止局域网监听时结束全部受查看会话（屏幕/输入/音频/剪贴板都跟随该 token）。
    pub async fn cancel_all(&self) {
        for (_, s) in self.sessions.lock().await.drain() {
            s.cancel.cancel();
        }
    }
}

/* ---------------- 指针形状 ---------------- */

/// DXGI 光标形状（RGBA，已含透明度）。
#[derive(Debug, Clone)]
pub struct CursorShape {
    pub width: u32,
    pub height: u32,
    pub hotspot_x: i32,
    pub hotspot_y: i32,
    pub rgba: Vec<u8>,
}

/// 一帧对应的指针状态（位置为显示器本地像素）。
#[derive(Debug, Clone, Default)]
pub struct PointerUpdate {
    pub visible: bool,
    pub x: i32,
    pub y: i32,
    /// 仅形状变化时携带
    pub shape: Option<CursorShape>,
}

/// 采集结果：BGRA + 行宽 + 指针状态。
pub struct Grabbed {
    pub bgra: Vec<u8>,
    pub stride: u32,
    pub pointer: PointerUpdate,
}

/* ---------------- 采集 + 编码（Windows） ---------------- */

#[cfg(windows)]
mod platform {
    use super::*;
    use std::ffi::c_void;
    use std::sync::OnceLock;

    use windows::core::{Interface, BOOL, GUID};
    use windows::Graphics::Capture::{
        Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
    };
    use windows::Graphics::DirectX::Direct3D11::{IDirect3DDevice, IDirect3DSurface};
    use windows::Graphics::DirectX::DirectXPixelFormat;
    use windows::Win32::Foundation::{E_INVALIDARG, HGLOBAL, HMODULE, HWND, LPARAM, RECT};
    use windows::Win32::Graphics::Direct3D::{
        D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0,
    };
    use windows::Win32::Graphics::Direct3D11::{
        D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D,
        D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_FLAG,
        D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
        D3D11_USAGE_STAGING,
    };
    use windows::Win32::Graphics::Dxgi::Common::{
        DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_ROTATION, DXGI_MODE_ROTATION_ROTATE180,
        DXGI_MODE_ROTATION_ROTATE270, DXGI_MODE_ROTATION_ROTATE90, DXGI_SAMPLE_DESC,
    };
    use windows::Win32::Graphics::Dxgi::IDXGIDevice;
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, IDXGIAdapter, IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication,
        IDXGIResource, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_NOT_CURRENTLY_AVAILABLE,
        DXGI_ERROR_UNSUPPORTED, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
        DXGI_OUTDUPL_POINTER_SHAPE_INFO, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR,
        DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME, DXGI_OUTPUT_DESC,
    };
    use windows::Win32::Graphics::GdiPlus::{
        EncoderParameter, EncoderParameterValueTypeLong, EncoderParameters, EncoderQuality,
        GdipCreateBitmapFromScan0, GdipDisposeImage, GdipGetImageEncoders,
        GdipGetImageEncodersSize, GdipSaveImageToStream, GpBitmap, GpImage, ImageCodecInfo,
        Ok as GpOk, Status,
    };
    use windows::Win32::System::Com::StructuredStorage::CreateStreamOnHGlobal;
    use windows::Win32::System::Com::{
        CoInitializeEx, IStream, COINIT_MULTITHREADED, STREAM_SEEK_SET,
    };
    use windows::Win32::System::WinRT::Direct3D11::{
        CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
    };
    use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetForegroundWindow, GetWindowLongW, GetWindowRect, GetWindowTextLengthW,
        GetWindowTextW, IsWindow, IsWindowVisible, GWL_EXSTYLE, WS_EX_TOOLWINDOW,
    };

    fn status_ok(s: Status, what: &str) -> anyhow::Result<()> {
        if s == GpOk {
            Ok(())
        } else {
            anyhow::bail!("{what} 失败: Status({})", s.0)
        }
    }

    /// GDI+ 只初始化一次（进程生命周期内不关闭）。
    fn ensure_gdiplus() -> anyhow::Result<()> {
        static STARTED: OnceLock<Result<(), String>> = OnceLock::new();
        let r = STARTED.get_or_init(|| unsafe {
            let mut token = 0usize;
            let input = windows::Win32::Graphics::GdiPlus::GdiplusStartupInput {
                GdiplusVersion: 1,
                ..Default::default()
            };
            let st = windows::Win32::Graphics::GdiPlus::GdiplusStartup(
                &mut token,
                &input,
                std::ptr::null_mut(),
            );
            if st == GpOk {
                Ok(())
            } else {
                Err(format!("GdiplusStartup 失败: Status({})", st.0))
            }
        });
        r.clone().map_err(anyhow::Error::msg)
    }

    /// JPEG 编码器 CLSID（首次查询后缓存）。
    fn jpeg_codec() -> anyhow::Result<GUID> {
        static CLSID: OnceLock<Result<GUID, String>> = OnceLock::new();
        let r = CLSID.get_or_init(|| unsafe {
            let mut num = 0u32;
            let mut size = 0u32;
            if GdipGetImageEncodersSize(&mut num, &mut size) != GpOk || size == 0 {
                return Err("GdipGetImageEncodersSize 失败".to_string());
            }
            let mut buf = vec![0u8; size as usize];
            if GdipGetImageEncoders(num, size, buf.as_mut_ptr() as *mut ImageCodecInfo) != GpOk {
                return Err("GdipGetImageEncoders 失败".to_string());
            }
            let infos =
                std::slice::from_raw_parts(buf.as_ptr() as *const ImageCodecInfo, num as usize);
            infos
                .iter()
                .find(|i| {
                    i.MimeType
                        .to_string()
                        .map(|s| s.eq_ignore_ascii_case("image/jpeg"))
                        .unwrap_or(false)
                })
                .map(|i| i.Clsid)
                .ok_or_else(|| "找不到 JPEG 编码器".to_string())
        });
        r.clone().map_err(anyhow::Error::msg)
    }

    /// BGRA（stride=RowPitch）→ 内存 JPEG。
    pub fn encode_jpeg(
        bytes: &[u8],
        w: u32,
        h: u32,
        stride: u32,
        quality: u8,
    ) -> anyhow::Result<Vec<u8>> {
        ensure_gdiplus()?;
        unsafe {
            let mut bmp: *mut GpBitmap = std::ptr::null_mut();
            // PixelFormat32bppARGB = 0x26200A（windows-rs 未导出）
            status_ok(
                GdipCreateBitmapFromScan0(
                    w as i32,
                    h as i32,
                    stride as i32,
                    0x26200A,
                    Some(bytes.as_ptr()),
                    &mut bmp,
                ),
                "GdipCreateBitmapFromScan0",
            )?;
            let stream: IStream = CreateStreamOnHGlobal(HGLOBAL(std::ptr::null_mut()), true)?;
            let mut q: i32 = quality as i32;
            let params = EncoderParameters {
                Count: 1,
                Parameter: [EncoderParameter {
                    Guid: EncoderQuality,
                    NumberOfValues: 1,
                    Type: EncoderParameterValueTypeLong.0 as u32,
                    Value: &mut q as *mut i32 as *mut c_void,
                }],
            };
            let clsid = jpeg_codec()?;
            let st = GdipSaveImageToStream(bmp as *mut GpImage, &stream, &clsid, &params);
            let _ = GdipDisposeImage(bmp as *mut GpImage);
            status_ok(st, "GdipSaveImageToStream")?;

            stream.Seek(0, STREAM_SEEK_SET, None)?;
            let mut out = Vec::new();
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let mut read = 0u32;
                let hr = stream.Read(
                    buf.as_mut_ptr() as *mut c_void,
                    buf.len() as u32,
                    Some(&mut read),
                );
                if hr.is_err() {
                    anyhow::bail!("读取 JPEG 流失败: {hr:?}");
                }
                if read == 0 {
                    break;
                }
                out.extend_from_slice(&buf[..read as usize]);
            }
            if out.is_empty() {
                anyhow::bail!("JPEG 编码结果为空");
            }
            Ok(out)
        }
    }

    /// DXGI 桌面复制失败。区分“暂时不可用（休眠/锁屏）”和“该环境根本不支持”，
    /// 后者可以回退到 Windows.Graphics.Capture（例如 GPU-P 虚拟机、虚拟/间接显示器）。
    #[derive(Debug)]
    pub struct DupError {
        pub code: Option<windows::core::HRESULT>,
        pub detail: String,
    }

    impl std::fmt::Display for DupError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(&self.detail)
        }
    }

    impl std::error::Error for DupError {}

    impl DupError {
        fn new(code: Option<windows::core::HRESULT>, detail: impl Into<String>) -> Self {
            Self {
                code,
                detail: detail.into(),
            }
        }

        /// 这些错误说明桌面复制在这台机器/这个输出上不可用（不是锁屏之类的临时状态），
        /// 而 WGC 走 DWM 合成，通常仍然能采集。
        fn try_wgc(&self) -> bool {
            let unsupported = matches!(
                self.code,
                Some(c)
                    if c == E_INVALIDARG
                        || c == DXGI_ERROR_UNSUPPORTED
                        || c == DXGI_ERROR_NOT_CURRENTLY_AVAILABLE
            );
            unsupported && !secure_desktop()
        }
    }

    /// 锁屏 / UAC 安全桌面：本进程看不到前台窗口。此时 WGC 也只能拿到静止画面，
    /// 保持“暂不可用”的提示并按 2s 重试更合适，解锁后会自动恢复。
    fn secure_desktop() -> bool {
        unsafe { GetForegroundWindow().is_invalid() }
    }

    /// 输出是否真的在桌面上：休眠/断开的 DisplayPort、虚拟显示驱动的影子输出
    /// 也会被 EnumOutputs 枚举出来，但 DesktopCoordinates 为 0 或未附着到桌面，
    /// 对它们调用 DuplicateOutput 只会得到 E_INVALIDARG。
    fn output_is_active(desc: &DXGI_OUTPUT_DESC) -> bool {
        desc.AttachedToDesktop.as_bool()
            && desc.DesktopCoordinates.right > desc.DesktopCoordinates.left
            && desc.DesktopCoordinates.bottom > desc.DesktopCoordinates.top
    }

    fn device_name(desc: &DXGI_OUTPUT_DESC) -> String {
        String::from_utf16_lossy(
            &desc
                .DeviceName
                .iter()
                .take_while(|c| **c != 0)
                .copied()
                .collect::<Vec<u16>>(),
        )
    }

    /// 第 index 个活动输出（与 `list_monitors` / 输入注入使用同一套过滤和顺序）。
    fn active_output_desc(index: usize) -> Option<DXGI_OUTPUT_DESC> {
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
            let mut cur = 0usize;
            for ai in 0..8 {
                let Ok(adapter) = factory.EnumAdapters1(ai) else {
                    break;
                };
                for oi in 0..8 {
                    let Ok(output) = adapter.EnumOutputs(oi) else {
                        break;
                    };
                    let Ok(desc) = output.GetDesc() else { continue };
                    if !output_is_active(&desc) {
                        continue;
                    }
                    if cur == index {
                        return Some(desc);
                    }
                    cur += 1;
                }
            }
            None
        }
    }

    pub struct Capturer {
        /// 释放后为 None（重建前必须先释放：同一进程对同一输出只能有一份复制）。
        dup: Option<IDXGIOutputDuplication>,
        ctx: ID3D11DeviceContext,
        device: ID3D11Device,
        staging: Option<ID3D11Texture2D>,
        size: (u32, u32),
        /// 显示器旋转（竖屏）：复制出来的是**未旋转**像素，需要自己转正。
        rotation: DXGI_MODE_ROTATION,
    }

    /// 旋转后的画面尺寸（90/270 交换宽高）。
    pub(super) fn rotated_dims(w: u32, h: u32, rot: DXGI_MODE_ROTATION) -> (u32, u32) {
        if rot == DXGI_MODE_ROTATION_ROTATE90 || rot == DXGI_MODE_ROTATION_ROTATE270 {
            (h, w)
        } else {
            (w, h)
        }
    }

    /// 把未旋转的 BGRA 画面转正（DXGI 的 Rotation 表示“显示时顺时针旋转了多少”）。
    /// 返回输出行宽（字节）。`dst` 会被调整到旋转后的尺寸。
    pub(super) fn rotate_bgra(
        src: &[u8],
        src_stride: usize,
        w: usize,
        h: usize,
        rot: DXGI_MODE_ROTATION,
        dst: &mut Vec<u8>,
    ) -> usize {
        let (dw, dh) = if rot == DXGI_MODE_ROTATION_ROTATE90 || rot == DXGI_MODE_ROTATION_ROTATE270
        {
            (h, w)
        } else {
            (w, h)
        };
        let stride = dw * 4;
        dst.resize(stride * dh, 0);
        let copy = |dst: &mut [u8], sx: usize, sy: usize| {
            let si = sy * src_stride + sx * 4;
            if si + 3 < src.len() {
                dst.copy_from_slice(&src[si..si + 4]);
            }
        };
        for y in 0..dh {
            let row = &mut dst[y * stride..(y + 1) * stride];
            for x in 0..dw {
                let d = &mut row[x * 4..x * 4 + 4];
                match rot {
                    r if r == DXGI_MODE_ROTATION_ROTATE90 => copy(d, y, h - 1 - x),
                    r if r == DXGI_MODE_ROTATION_ROTATE180 => copy(d, w - 1 - x, h - 1 - y),
                    r if r == DXGI_MODE_ROTATION_ROTATE270 => copy(d, w - 1 - y, x),
                    // Identity / Unspecified：原样
                    _ => copy(d, x, y),
                }
            }
        }
        stride
    }

    /// AcquireNextFrame 成功后必须 ReleaseFrame，否则下一次 AcquireNextFrame
    /// 会因“上一帧未释放”返回 DXGI_ERROR_INVALID_CALL；用 Drop 覆盖所有提前返回。
    struct FrameGuard<'a>(&'a IDXGIOutputDuplication);

    impl Drop for FrameGuard<'_> {
        fn drop(&mut self) {
            unsafe {
                let _ = self.0.ReleaseFrame();
            }
        }
    }

    pub enum GrabError {
        /// 画面无变化（静止）
        NoChange,
        /// 需要重建（分辨率/显示器变化/驱动重置）
        Lost(String),
        Failed(String),
    }

    fn enumerate_outputs() -> anyhow::Result<Vec<MonitorInfo>> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
            let mut out = Vec::new();
            let mut index = 0usize;
            for ai in 0..8 {
                let Ok(adapter) = factory.EnumAdapters1(ai) else {
                    break;
                };
                for oi in 0..8 {
                    let Ok(output) = adapter.EnumOutputs(oi) else {
                        break;
                    };
                    let Ok(desc) = output.GetDesc() else { continue };
                    if !output_is_active(&desc) {
                        continue;
                    }
                    let r = desc.DesktopCoordinates;
                    out.push(MonitorInfo {
                        index,
                        width: (r.right - r.left) as u32,
                        height: (r.bottom - r.top) as u32,
                        name: device_name(&desc),
                    });
                    index += 1;
                }
            }
            Ok(out)
        }
    }

    pub fn list_monitors() -> anyhow::Result<Vec<MonitorInfo>> {
        enumerate_outputs()
    }

    /// 活动显示器矩形（虚拟桌面坐标）；索引与 `list_monitors` 一致。
    pub fn monitor_rects() -> Vec<(i32, i32, u32, u32)> {
        unsafe {
            let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() else {
                return Vec::new();
            };
            let mut out = Vec::new();
            for ai in 0..8 {
                let Ok(adapter) = factory.EnumAdapters1(ai) else {
                    break;
                };
                for oi in 0..8 {
                    let Ok(output) = adapter.EnumOutputs(oi) else {
                        break;
                    };
                    let Ok(desc) = output.GetDesc() else { continue };
                    if !output_is_active(&desc) {
                        continue;
                    }
                    let r = desc.DesktopCoordinates;
                    out.push((
                        r.left,
                        r.top,
                        (r.right - r.left) as u32,
                        (r.bottom - r.top) as u32,
                    ));
                }
            }
            out
        }
    }

    /// 读取当前指针位置/形状（形状仅在变化时由 DXGI 提供）。
    unsafe fn read_pointer(
        dup: &IDXGIOutputDuplication,
        info: &DXGI_OUTDUPL_FRAME_INFO,
    ) -> PointerUpdate {
        let mut p = PointerUpdate {
            visible: info.PointerPosition.Visible.as_bool(),
            x: info.PointerPosition.Position.x,
            y: info.PointerPosition.Position.y,
            shape: None,
        };
        if info.PointerShapeBufferSize > 0 {
            let mut buf = vec![0u8; info.PointerShapeBufferSize as usize];
            let mut required = 0u32;
            let mut si = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
            if dup
                .GetFramePointerShape(
                    buf.len() as u32,
                    buf.as_mut_ptr() as *mut c_void,
                    &mut required,
                    &mut si,
                )
                .is_ok()
            {
                p.shape = shape_to_rgba(&buf, &si);
            }
        }
        p
    }

    /// DXGI 形状（彩色 BGRA / 单色 AND+XOR）→ RGBA。
    fn shape_to_rgba(buf: &[u8], si: &DXGI_OUTDUPL_POINTER_SHAPE_INFO) -> Option<CursorShape> {
        let w = si.Width as usize;
        let h = si.Height as usize;
        let pitch = si.Pitch as usize;
        if w == 0 || h == 0 || pitch == 0 {
            return None;
        }
        let mut rgba = vec![0u8; w * h * 4];
        let ty = si.Type;
        if ty == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 as u32 {
            if buf.len() < pitch * h * 2 {
                return None;
            }
            for y in 0..h {
                for x in 0..w {
                    let ai = y * pitch + x / 8;
                    let xi = (h + y) * pitch + x / 8;
                    let and = (buf[ai] >> (7 - (x % 8))) & 1;
                    let xor = (buf[xi] >> (7 - (x % 8))) & 1;
                    if and == 1 && xor == 0 {
                        continue; // 透明
                    }
                    let v = if xor == 1 { 255 } else { 0 };
                    let i = (y * w + x) * 4;
                    rgba[i] = v;
                    rgba[i + 1] = v;
                    rgba[i + 2] = v;
                    rgba[i + 3] = 255;
                }
            }
        } else {
            if buf.len() < pitch * h {
                return None;
            }
            let masked = ty == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 as u32;
            for y in 0..h {
                for x in 0..w {
                    let s = y * pitch + x * 4;
                    if s + 3 >= buf.len() {
                        continue;
                    }
                    let i = (y * w + x) * 4;
                    rgba[i] = buf[s + 2];
                    rgba[i + 1] = buf[s + 1];
                    rgba[i + 2] = buf[s];
                    let a = buf[s + 3];
                    rgba[i + 3] = if masked && a == 0 { 0 } else { a };
                }
            }
        }
        Some(CursorShape {
            width: w as u32,
            height: h as u32,
            hotspot_x: si.HotSpot.x,
            hotspot_y: si.HotSpot.y,
            rgba,
        })
    }

    impl Capturer {
        pub fn new(index: usize) -> Result<Self, DupError> {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                let wanted = active_output_desc(index).ok_or_else(|| {
                    DupError::new(
                        None,
                        format!(
                            "没有第 {} 个可用显示器（显示器可能休眠/关闭，或只有虚拟显示）",
                            index + 1
                        ),
                    )
                })?;
                let wanted_name = device_name(&wanted);
                let factory: IDXGIFactory1 =
                    CreateDXGIFactory1().map_err(|e| DupError::new(Some(e.code()), e.message()))?;
                let mut last: Option<DupError> = None;
                // 混合显卡/显卡 SHIM 会让同一个输出出现在多个 adapter 上：
                // 按 DeviceName 找到该输出，逐个 adapter 尝试，直到复制成功。
                for ai in 0..8 {
                    let Ok(adapter) = factory.EnumAdapters1(ai) else {
                        break;
                    };
                    for oi in 0..8 {
                        let Ok(output) = adapter.EnumOutputs(oi) else {
                            break;
                        };
                        let Ok(desc) = output.GetDesc() else { continue };
                        if !output_is_active(&desc) || device_name(&desc) != wanted_name {
                            continue;
                        }
                        let base_adapter: IDXGIAdapter = match adapter.cast() {
                            Ok(a) => a,
                            Err(e) => {
                                last = Some(DupError::new(Some(e.code()), e.message()));
                                continue;
                            }
                        };
                        let mut device: Option<ID3D11Device> = None;
                        let mut ctx: Option<ID3D11DeviceContext> = None;
                        let create = D3D11CreateDevice(
                            &base_adapter,
                            D3D_DRIVER_TYPE_UNKNOWN,
                            HMODULE::default(),
                            D3D11_CREATE_DEVICE_FLAG(0),
                            Some(&[D3D_FEATURE_LEVEL_11_0]),
                            D3D11_SDK_VERSION,
                            Some(&mut device),
                            None,
                            Some(&mut ctx),
                        );
                        if let Err(e) = create {
                            last = Some(DupError::new(
                                Some(e.code()),
                                format!("D3D11CreateDevice(adapter {ai}): {e}"),
                            ));
                            continue;
                        }
                        let output1: IDXGIOutput1 = match output.cast() {
                            Ok(o) => o,
                            Err(e) => {
                                last = Some(DupError::new(Some(e.code()), e.message()));
                                continue;
                            }
                        };
                        let dev = match device {
                            Some(d) => d,
                            None => {
                                last = Some(DupError::new(None, "D3D11 设备缺失"));
                                continue;
                            }
                        };
                        match output1.DuplicateOutput(&dev) {
                            Ok(dup) => {
                                let od = dup.GetDesc();
                                return Ok(Self {
                                    dup: Some(dup),
                                    ctx: match ctx {
                                        Some(c) => c,
                                        None => {
                                            return Err(DupError::new(None, "D3D11 上下文缺失"))
                                        }
                                    },
                                    device: dev,
                                    staging: None,
                                    size: rotated_dims(
                                        od.ModeDesc.Width,
                                        od.ModeDesc.Height,
                                        desc.Rotation,
                                    ),
                                    rotation: desc.Rotation,
                                });
                            }
                            Err(e) => {
                                last = Some(DupError::new(
                                    Some(e.code()),
                                    format!("DuplicateOutput({wanted_name}): {e}"),
                                ))
                            }
                        }
                    }
                }
                Err(last.unwrap_or_else(|| DupError::new(None, "没有可用的显示器输出")))
            }
        }

        /// 立刻释放桌面复制接口。重建前必须调用：同一进程对同一输出
        /// 只能存在一份 IDXGIOutputDuplication，否则重建会返回 E_INVALIDARG。
        pub fn release(&mut self) {
            self.dup = None;
            self.staging = None;
        }

        pub fn size(&self) -> (u32, u32) {
            self.size
        }

        /// 抓一帧 BGRA（无变化时 `NoChange`）。调用方负责释放（内部已 ReleaseFrame）。
        pub fn grab(&mut self, timeout_ms: u32) -> Result<Grabbed, GrabError> {
            unsafe {
                let dup = match self.dup.as_ref() {
                    Some(d) => d,
                    None => return Err(GrabError::Lost("采集已释放".to_string())),
                };
                let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
                let mut resource: Option<IDXGIResource> = None;
                match dup.AcquireNextFrame(timeout_ms, &mut info, &mut resource) {
                    Ok(()) => {}
                    Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => {
                        return Err(GrabError::NoChange)
                    }
                    Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                        return Err(GrabError::Lost("ACCESS_LOST".to_string()))
                    }
                    Err(e) => return Err(GrabError::Failed(format!("AcquireNextFrame: {e}"))),
                }
                let _frame = FrameGuard(dup);
                let pointer = read_pointer(dup, &info);
                let tex: ID3D11Texture2D = match resource {
                    Some(r) => r
                        .cast()
                        .map_err(|e| GrabError::Failed(format!("resource cast: {e}")))?,
                    None => return Err(GrabError::NoChange),
                };
                // 以“实际拿到的纹理”为准，而不是 GetDesc().ModeDesc：
                // DuplicateOutput 总是把画面转成 32 位 BGRA，而 HDR 显示器的
                // ModeDesc.Format 是 R10G10B10A2/R16G16B16A16；用 ModeDesc 建
                // staging 纹理会让 CopyResource 因格式不匹配而静默失败（全黑）。
                let mut tdesc = D3D11_TEXTURE2D_DESC::default();
                tex.GetDesc(&mut tdesc);
                // DuplicateOutput 保证输出 32 位 BGRA；其它格式（例如将来改用
                // DuplicateOutput1 或驱动异常）不能直接喂给 BGRA 转码管线。
                if tdesc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
                    return Err(GrabError::Failed(format!(
                        "桌面复制返回了不支持的像素格式（{:?}）",
                        tdesc.Format
                    )));
                }
                let need_staging = match self.staging.as_ref() {
                    Some(s) => {
                        let mut sd = D3D11_TEXTURE2D_DESC::default();
                        s.GetDesc(&mut sd);
                        sd.Width != tdesc.Width
                            || sd.Height != tdesc.Height
                            || sd.Format != tdesc.Format
                    }
                    None => true,
                };
                if need_staging {
                    let desc = D3D11_TEXTURE2D_DESC {
                        Width: tdesc.Width,
                        Height: tdesc.Height,
                        MipLevels: 1,
                        ArraySize: 1,
                        Format: tdesc.Format,
                        SampleDesc: DXGI_SAMPLE_DESC {
                            Count: 1,
                            Quality: 0,
                        },
                        Usage: D3D11_USAGE_STAGING,
                        BindFlags: 0,
                        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                        MiscFlags: 0,
                    };
                    let mut staging: Option<ID3D11Texture2D> = None;
                    self.device
                        .CreateTexture2D(&desc, None, Some(&mut staging))
                        .map_err(|e| GrabError::Failed(format!("CreateTexture2D: {e}")))?;
                    self.staging = staging;
                    self.size = (tdesc.Width, tdesc.Height);
                }
                let staging = self
                    .staging
                    .as_ref()
                    .ok_or_else(|| GrabError::Failed("staging 缺失".into()))?;
                let src: ID3D11Resource = tex
                    .cast()
                    .map_err(|e| GrabError::Failed(format!("tex cast: {e}")))?;
                let dst: ID3D11Resource = staging
                    .cast()
                    .map_err(|e| GrabError::Failed(format!("staging cast: {e}")))?;
                self.ctx.CopyResource(&dst, &src);
                self.ctx.Flush();
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                self.ctx
                    .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                    .map_err(|e| GrabError::Failed(format!("Map: {e}")))?;
                let raw = std::slice::from_raw_parts(
                    mapped.pData as *const u8,
                    (mapped.RowPitch * tdesc.Height) as usize,
                );
                let (rw, rh) = rotated_dims(tdesc.Width, tdesc.Height, self.rotation);
                self.size = (rw, rh);
                let (bgra, stride) = if rw != tdesc.Width
                    || rh != tdesc.Height
                    || self.rotation == DXGI_MODE_ROTATION_ROTATE180
                {
                    // 竖屏/倒屏：把未旋转的复制结果转正（指针坐标是桌面方向，
                    // 与转正后的画面一致，归一化仍按 self.size）
                    let mut buf = Vec::new();
                    let stride = rotate_bgra(
                        raw,
                        mapped.RowPitch as usize,
                        tdesc.Width as usize,
                        tdesc.Height as usize,
                        self.rotation,
                        &mut buf,
                    );
                    (buf, stride as u32)
                } else {
                    (raw.to_vec(), mapped.RowPitch)
                };
                self.ctx.Unmap(staging, 0);
                Ok(Grabbed {
                    bgra,
                    stride,
                    pointer,
                })
            }
        }
    }

    /* ---------------- 指定窗口（WinRT Graphics Capture） ---------------- */

    /// 顶层窗口枚举（跳过不可见/工具窗口/过小窗口）。
    pub fn list_windows() -> Vec<WindowInfo> {
        unsafe {
            let mut out: Vec<WindowInfo> = Vec::new();
            let _ = EnumWindows(
                Some(enum_window),
                LPARAM(&mut out as *mut Vec<WindowInfo> as isize),
            );
            out
        }
    }

    unsafe extern "system" fn enum_window(hwnd: HWND, lparam: LPARAM) -> BOOL {
        unsafe {
            if !IsWindowVisible(hwnd).as_bool() {
                return BOOL::from(true);
            }
            let len = GetWindowTextLengthW(hwnd);
            if len <= 0 {
                return BOOL::from(true);
            }
            let mut buf = vec![0u16; len as usize + 1];
            let n = GetWindowTextW(hwnd, &mut buf);
            if n <= 0 {
                return BOOL::from(true);
            }
            let ex = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
            if ex & WS_EX_TOOLWINDOW.0 != 0 {
                return BOOL::from(true);
            }
            let mut rc = RECT::default();
            if GetWindowRect(hwnd, &mut rc).is_err() {
                return BOOL::from(true);
            }
            if (rc.right - rc.left) < 32 || (rc.bottom - rc.top) < 32 {
                return BOOL::from(true);
            }
            let title = String::from_utf16_lossy(&buf[..n as usize]);
            let out = &mut *(lparam.0 as *mut Vec<WindowInfo>);
            out.push(WindowInfo {
                hwnd: hwnd.0 as usize as u64,
                title,
            });
        }
        BOOL::from(true)
    }

    /// Windows.Graphics.Capture 采集面（窗口 / 显示器共用）。
    /// 走 DWM 合成，DXGI 桌面复制不可用时（GPU-P 虚拟机、虚拟/间接显示器、
    /// 已被其他进程占用的输出）通常仍能出画面。
    struct WgcSurface {
        device: ID3D11Device,
        ctx: ID3D11DeviceContext,
        winrt_device: IDirect3DDevice,
        pool: Direct3D11CaptureFramePool,
        session: GraphicsCaptureSession,
        staging: Option<ID3D11Texture2D>,
        size: (u32, u32),
        closed: bool,
    }

    impl WgcSurface {
        fn start(
            item: &GraphicsCaptureItem,
            capture_cursor: bool,
            border: bool,
        ) -> anyhow::Result<Self> {
            unsafe {
                let mut device: Option<ID3D11Device> = None;
                let mut ctx: Option<ID3D11DeviceContext> = None;
                D3D11CreateDevice(
                    None,
                    D3D_DRIVER_TYPE_HARDWARE,
                    HMODULE::default(),
                    D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    Some(&[D3D_FEATURE_LEVEL_11_0]),
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    Some(&mut ctx),
                )?;
                let device = device.ok_or_else(|| anyhow::anyhow!("D3D11 设备缺失"))?;
                let ctx = ctx.ok_or_else(|| anyhow::anyhow!("D3D11 上下文缺失"))?;
                let dxgi: IDXGIDevice = device.cast()?;
                let inspectable = CreateDirect3D11DeviceFromDXGIDevice(&dxgi)?;
                let winrt_device: IDirect3DDevice = inspectable.cast()?;
                let sz = item.Size()?;
                if sz.Width <= 0 || sz.Height <= 0 {
                    anyhow::bail!("采集区域尺寸无效");
                }
                let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
                    &winrt_device,
                    DirectXPixelFormat::B8G8R8A8UIntNormalized,
                    2,
                    sz,
                )?;
                let session = pool.CreateCaptureSession(item)?;
                let _ = session.SetIsCursorCaptureEnabled(capture_cursor);
                if border {
                    // Windows 11 默认会画黄色边框；窗口共享时显式保持，提示用户正在共享
                    let _ = session.SetIsBorderRequired(true);
                } else if GraphicsCaptureSession::IsSupported().unwrap_or(false) {
                    // 整屏共享不画边框（与常见远程桌面一致；查看方也能看到本机的“正在被查看”提示）
                    let _ = session.SetIsBorderRequired(false);
                }
                session.StartCapture()?;
                Ok(Self {
                    device,
                    ctx,
                    winrt_device,
                    pool,
                    session,
                    staging: None,
                    size: (sz.Width as u32, sz.Height as u32),
                    closed: false,
                })
            }
        }

        fn size(&self) -> (u32, u32) {
            self.size
        }

        fn close(&mut self) {
            if self.closed {
                return;
            }
            self.closed = true;
            let _ = self.session.Close();
            let _ = self.pool.Close();
        }

        fn grab(&mut self) -> Result<Grabbed, GrabError> {
            if self.closed {
                return Err(GrabError::Lost("采集已释放".to_string()));
            }
            let frame = match self.pool.TryGetNextFrame() {
                Ok(f) => f,
                Err(_) => return Err(GrabError::NoChange),
            };
            let res = self.copy_frame(&frame);
            let _ = frame.Close();
            res
        }

        fn copy_frame(
            &mut self,
            frame: &windows::Graphics::Capture::Direct3D11CaptureFrame,
        ) -> Result<Grabbed, GrabError> {
            unsafe {
                let cs = frame
                    .ContentSize()
                    .map_err(|e| GrabError::Failed(format!("读取采集尺寸失败: {e}")))?;
                if cs.Width > 0 && cs.Height > 0 && (cs.Width as u32, cs.Height as u32) != self.size
                {
                    let sz = windows::Graphics::SizeInt32 {
                        Width: cs.Width,
                        Height: cs.Height,
                    };
                    self.pool
                        .Recreate(
                            &self.winrt_device,
                            DirectXPixelFormat::B8G8R8A8UIntNormalized,
                            2,
                            sz,
                        )
                        .map_err(|e| GrabError::Failed(format!("重建帧池失败: {e}")))?;
                    self.size = (cs.Width as u32, cs.Height as u32);
                    self.staging = None;
                }
                let surface: IDirect3DSurface = frame
                    .Surface()
                    .map_err(|e| GrabError::Failed(format!("读取采集表面失败: {e}")))?;
                let access: IDirect3DDxgiInterfaceAccess = surface
                    .cast()
                    .map_err(|e| GrabError::Failed(format!("表面接口失败: {e}")))?;
                let tex: ID3D11Texture2D = access
                    .GetInterface()
                    .map_err(|e| GrabError::Failed(format!("获取纹理失败: {e}")))?;
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                tex.GetDesc(&mut desc);
                if self.staging.is_none() {
                    let sd = D3D11_TEXTURE2D_DESC {
                        Width: desc.Width,
                        Height: desc.Height,
                        MipLevels: 1,
                        ArraySize: 1,
                        Format: desc.Format,
                        SampleDesc: DXGI_SAMPLE_DESC {
                            Count: 1,
                            Quality: 0,
                        },
                        Usage: D3D11_USAGE_STAGING,
                        BindFlags: 0,
                        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                        MiscFlags: 0,
                    };
                    let mut staging: Option<ID3D11Texture2D> = None;
                    self.device
                        .CreateTexture2D(&sd, None, Some(&mut staging))
                        .map_err(|e| GrabError::Failed(format!("创建暂存纹理失败: {e}")))?;
                    self.staging = staging;
                }
                let staging = self
                    .staging
                    .as_ref()
                    .ok_or_else(|| GrabError::Failed("暂存纹理缺失".to_string()))?;
                let src: ID3D11Resource = tex
                    .cast()
                    .map_err(|e| GrabError::Failed(format!("纹理转换失败: {e}")))?;
                let dst: ID3D11Resource = staging
                    .cast()
                    .map_err(|e| GrabError::Failed(format!("暂存纹理转换失败: {e}")))?;
                self.ctx.CopyResource(&dst, &src);
                self.ctx.Flush();
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                self.ctx
                    .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                    .map_err(|e| GrabError::Failed(format!("映射纹理失败: {e}")))?;
                let bytes = std::slice::from_raw_parts(
                    mapped.pData as *const u8,
                    (mapped.RowPitch * desc.Height) as usize,
                )
                .to_vec();
                self.ctx.Unmap(staging, 0);
                Ok(Grabbed {
                    bgra: bytes,
                    stride: mapped.RowPitch,
                    pointer: PointerUpdate::default(),
                })
            }
        }
    }

    impl Drop for WgcSurface {
        fn drop(&mut self) {
            self.close();
        }
    }

    pub struct WindowCapturer {
        cap: WgcSurface,
        hwnd: HWND,
    }

    impl WindowCapturer {
        pub fn new(hwnd: u64) -> anyhow::Result<Self> {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                let hwnd = HWND(hwnd as usize as *mut c_void);
                if !IsWindow(Some(hwnd)).as_bool() {
                    anyhow::bail!("窗口已关闭");
                }
                let interop: IGraphicsCaptureItemInterop =
                    windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
                let item: GraphicsCaptureItem = interop.CreateForWindow(hwnd)?;
                Ok(Self {
                    cap: WgcSurface::start(&item, false, true)?,
                    hwnd,
                })
            }
        }

        pub fn size(&self) -> (u32, u32) {
            self.cap.size()
        }

        pub fn grab(&mut self, _timeout_ms: u32) -> Result<Grabbed, GrabError> {
            unsafe {
                if !IsWindow(Some(self.hwnd)).as_bool() {
                    return Err(GrabError::Lost("窗口已关闭".to_string()));
                }
            }
            self.cap.grab()
        }

        pub fn release(&mut self) {
            self.cap.close();
        }
    }

    /// 整屏采集的 WGC 回退路径（光标直接画进画面，避免两套光标绘制）。
    pub struct WgcDesktop {
        cap: WgcSurface,
    }

    impl WgcDesktop {
        pub fn new(index: usize) -> anyhow::Result<Self> {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                let desc = active_output_desc(index)
                    .ok_or_else(|| anyhow::anyhow!("没有第 {} 个可用显示器", index + 1))?;
                if desc.Monitor.is_invalid() {
                    anyhow::bail!("显示器句柄无效（{}）", device_name(&desc));
                }
                let interop: IGraphicsCaptureItemInterop =
                    windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
                let item: GraphicsCaptureItem = interop.CreateForMonitor(desc.Monitor)?;
                Ok(Self {
                    cap: WgcSurface::start(&item, true, false)?,
                })
            }
        }

        pub fn size(&self) -> (u32, u32) {
            self.cap.size()
        }

        pub fn grab(&mut self) -> Result<Grabbed, GrabError> {
            self.cap.grab()
        }

        pub fn release(&mut self) {
            self.cap.close();
        }
    }

    /// 桌面采集：优先 DXGI 复制（有光标形状、开销最低），失败时回退 WGC。
    pub enum DesktopSource {
        Dup(Capturer),
        Wgc(WgcDesktop),
    }

    impl DesktopSource {
        fn size(&self) -> (u32, u32) {
            match self {
                Self::Dup(c) => c.size(),
                Self::Wgc(w) => w.size(),
            }
        }

        fn grab(&mut self, timeout_ms: u32) -> Result<Grabbed, GrabError> {
            match self {
                Self::Dup(c) => c.grab(timeout_ms),
                Self::Wgc(w) => w.grab(),
            }
        }

        fn release(&mut self) {
            match self {
                Self::Dup(c) => c.release(),
                Self::Wgc(w) => w.release(),
            }
        }
    }

    /// 采集源：整个显示器或指定窗口。
    pub enum Source {
        Desktop(DesktopSource),
        Window(WindowCapturer),
    }

    impl Source {
        pub fn desktop(index: usize) -> anyhow::Result<Self> {
            match Capturer::new(index) {
                Ok(c) => Ok(Self::Desktop(DesktopSource::Dup(c))),
                Err(e) if e.try_wgc() => match WgcDesktop::new(index) {
                    Ok(w) => {
                        applog!("[peer] DXGI 桌面复制不可用（{e}），改用 WGC 整屏采集");
                        Ok(Self::Desktop(DesktopSource::Wgc(w)))
                    }
                    Err(e2) => Err(anyhow::anyhow!(
                        "桌面复制不可用（{e}）；WGC 整屏采集也失败（{e2}）"
                    )),
                },
                Err(e) => Err(anyhow::anyhow!("{e}")),
            }
        }

        pub fn window(hwnd: u64) -> anyhow::Result<Self> {
            Ok(Self::Window(WindowCapturer::new(hwnd)?))
        }

        /// 为重建释放底层句柄（DXGI 同一进程同一输出只允许一份复制）。
        pub fn release(&mut self) {
            match self {
                Self::Desktop(d) => d.release(),
                Self::Window(w) => w.release(),
            }
        }

        pub fn size(&self) -> (u32, u32) {
            match self {
                Self::Desktop(c) => c.size(),
                Self::Window(w) => w.size(),
            }
        }

        pub fn grab(&mut self, timeout_ms: u32) -> Result<Grabbed, GrabError> {
            match self {
                Self::Desktop(c) => c.grab(timeout_ms),
                Self::Window(w) => w.grab(timeout_ms),
            }
        }

        pub fn is_window(&self) -> bool {
            matches!(self, Self::Window(_))
        }
    }
}

#[cfg(not(windows))]
mod platform {
    use super::*;
    pub fn list_monitors() -> anyhow::Result<Vec<MonitorInfo>> {
        Ok(Vec::new())
    }
    pub fn encode_jpeg(_b: &[u8], _w: u32, _h: u32, _s: u32, _q: u8) -> anyhow::Result<Vec<u8>> {
        anyhow::bail!("远程桌面仅支持 Windows")
    }
    pub struct Capturer;
    pub enum GrabError {
        NoChange,
        Lost(String),
        Failed(String),
    }
    impl Capturer {
        pub fn new(_index: usize) -> anyhow::Result<Self> {
            anyhow::bail!("远程桌面仅支持 Windows")
        }
        pub fn size(&self) -> (u32, u32) {
            (0, 0)
        }
        pub fn grab(&mut self, _t: u32) -> Result<Grabbed, GrabError> {
            Err(GrabError::Failed("unsupported".into()))
        }
        pub fn release(&mut self) {}
    }

    pub fn list_windows() -> Vec<WindowInfo> {
        Vec::new()
    }

    pub struct WindowCapturer;

    impl WindowCapturer {
        pub fn new(_hwnd: u64) -> anyhow::Result<Self> {
            anyhow::bail!("窗口共享仅支持 Windows")
        }
        pub fn size(&self) -> (u32, u32) {
            (0, 0)
        }
        pub fn grab(&mut self, _t: u32) -> Result<Grabbed, GrabError> {
            Err(GrabError::Failed("unsupported".into()))
        }
        pub fn release(&mut self) {}
    }

    pub enum Source {
        Desktop(Capturer),
        Window(WindowCapturer),
    }

    impl Source {
        pub fn desktop(index: usize) -> anyhow::Result<Self> {
            Ok(Self::Desktop(Capturer::new(index)?))
        }
        pub fn window(hwnd: u64) -> anyhow::Result<Self> {
            Ok(Self::Window(WindowCapturer::new(hwnd)?))
        }
        pub fn release(&mut self) {
            match self {
                Self::Desktop(c) => c.release(),
                Self::Window(w) => w.release(),
            }
        }
        pub fn size(&self) -> (u32, u32) {
            match self {
                Self::Desktop(c) => c.size(),
                Self::Window(w) => w.size(),
            }
        }
        pub fn grab(&mut self, t: u32) -> Result<Grabbed, GrabError> {
            match self {
                Self::Desktop(c) => c.grab(t),
                Self::Window(w) => w.grab(t),
            }
        }
        pub fn is_window(&self) -> bool {
            matches!(self, Self::Window(_))
        }
    }
}

use platform::{GrabError, Source};

#[cfg(windows)]
pub use platform::monitor_rects;

/* ---------------- 受查看端：确认 + 流 ---------------- */

pub async fn approve(
    hub: &ScreenHub,
    app: &tauri::AppHandle,
    peer: &PeerRecord,
    peer_addr: Option<std::net::SocketAddr>,
    require_hello: bool,
    input_requested: bool,
    input_only: bool,
) -> Result<bool, String> {
    let addr_text = peer_addr
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|| "未知地址".to_string());
    // 频控：同一设备 5s 内只弹一次，避免已配对设备反复刷确认框
    if !hub.prompt_allowed(&peer.id).await {
        applog!("[peer] 屏幕查看请求过于频繁，已忽略: {}", peer.name);
        return Err("请求过于频繁，请稍后再试".to_string());
    }
    let consent_id = Uuid::new_v4().to_string();
    let rx = hub.request_consent(consent_id.clone()).await;
    let _ = app.emit(
        crate::events::PEER_SCREEN_REQUEST,
        ScreenRequest {
            consent_id: consent_id.clone(),
            peer_name: peer.name.clone(),
            peer_fp: peer.id.chars().skip(7).take(8).collect::<String>(),
            addr: addr_text.clone(),
            hello_required: require_hello,
            input_requested: input_requested || input_only,
            input_only,
        },
    );
    applog!(
        "[peer] 屏幕查看请求: {} from {addr_text}，等待本机确认",
        peer.name
    );
    let decision = match tokio::time::timeout(SCREEN_CONSENT_TIMEOUT, rx).await {
        Ok(Ok(v)) => v,
        Ok(Err(_)) => ScreenDecision {
            allow: false,
            allow_input: false,
        },
        Err(_) => {
            hub.cancel_consent(&consent_id).await;
            ScreenDecision {
                allow: false,
                allow_input: false,
            }
        }
    };
    if !decision.allow {
        let _ = app.emit(
            crate::events::PEER_DENIED,
            crate::peer::control::DeniedEvent {
                peer_name: peer.name.clone(),
                reason: "consent".to_string(),
                consent_id: Some(consent_id.clone()),
                kind: "screen".to_string(),
            },
        );
        applog!("[peer] 已拒绝 {} 的屏幕查看请求", peer.name);
        return Err("本机拒绝屏幕查看".to_string());
    }
    let allow_input = if input_only {
        true
    } else {
        decision.allow_input
    };
    if require_hello {
        let peer_name = peer.name.clone();
        let what = if input_only {
            "请求操作本机"
        } else {
            "请求查看本机屏幕"
        };
        let outcome = tauri::async_runtime::spawn_blocking(move || {
            crate::hello::verify(&format!("「{peer_name}」{what}"))
        })
        .await
        .unwrap_or(crate::hello::HelloOutcome::Error);
        if outcome != crate::hello::HelloOutcome::Verified {
            let _ = app.emit(
                crate::events::PEER_DENIED,
                crate::peer::control::DeniedEvent {
                    peer_name: peer.name.clone(),
                    reason: "hello".to_string(),
                    consent_id: Some(consent_id),
                    kind: "screen".to_string(),
                },
            );
            applog!("[peer] Windows Hello 未通过（{outcome:?}），拒绝屏幕查看");
            return Err("Windows Hello 未通过".to_string());
        }
    }
    Ok(allow_input)
}

/// 启动屏幕流（受查看端）；`allow_input` 决定本次查看是否接受键鼠注入。
#[allow(clippy::too_many_arguments)]
pub async fn start_stream(
    app: tauri::AppHandle,
    hub: Arc<ScreenHub>,
    peer: PeerRecord,
    peer_addr: Option<std::net::SocketAddr>,
    addr_text: String,
    quality: u8,
    allow_input: bool,
    require_hello: bool,
    stream: russh::ChannelStream<russh::server::Msg>,
    grant: crate::peer::server::GrantCheck,
) -> AppResult<()> {
    let session_id = Uuid::new_v4().to_string();
    let cancel = CancellationToken::new();
    let started_ms = now_ms();
    let peer_name = peer.name.clone();
    let sid = session_id.clone();
    let hub2 = hub.clone();
    let app2 = app.clone();
    let cancel2 = cancel.clone();
    let name2 = peer_name.clone();
    let addr2 = addr_text.clone();
    let quality = quality.clamp(10, 95);
    let peer_clipboard = peer.grant_clipboard;
    let peer_audio = peer.grant_audio;

    hub2.register(
        sid.clone(),
        ViewSession {
            peer_name: peer_name.clone(),
            peer_addr: addr_text.clone(),
            quality,
            monitor: 0,
            allow_input,
            started_ms,
            cancel: cancel.clone(),
        },
    )
    .await;
    let _ = app.emit(
        crate::events::PEER_SCREEN_SESSION,
        ScreenSessionEvent {
            action: "started".to_string(),
            session_id: sid.clone(),
            peer_name: peer_name.clone(),
            reason: None,
        },
    );
    applog!("[peer] 屏幕查看开始: {peer_name} from {addr_text}（质量 {quality}）");

    tokio::spawn(async move {
        run_stream(
            app2,
            hub2,
            sid.clone(),
            peer,
            peer_addr,
            require_hello,
            name2.clone(),
            addr2.clone(),
            quality,
            allow_input,
            peer_clipboard,
            peer_audio,
            stream,
            cancel2,
            grant,
        )
        .await;
        hub.finish(&sid).await;
        let _ = app.emit(
            crate::events::PEER_SCREEN_SESSION,
            ScreenSessionEvent {
                action: "ended".to_string(),
                session_id: sid.clone(),
                peer_name: name2.clone(),
                reason: None,
            },
        );
        applog!("[peer] 屏幕查看结束: {name2} from {addr2}");
    });
    Ok(())
}

/// 一帧待发送数据（JPEG 或 H.264 访问单元）。
struct FramePacket {
    width: u32,
    height: u32,
    ts_us: u64,
    key: bool,
    h264: bool,
    /// 源画面是否 HD（用于解码端选择 BT.709）——必须基于采集尺寸，
    /// 不能基于自适应缩放后的编码高度。
    hd: bool,
    data: Vec<u8>,
}

fn scaled_dim(v: u32, scale: f32) -> u32 {
    (((v as f32) * scale) as u32 & !1).max(2)
}

/// 画质档位 → 目标码率（再按缩放比例平方衰减）。
fn bitrate_for(q: u8, scale: f32) -> u32 {
    let base = if q <= 45 {
        1_500_000.0
    } else if q <= 65 {
        4_000_000.0
    } else {
        8_000_000.0
    };
    (base * scale * scale).max(500_000.0) as u32
}

/// 自适应一步：先降帧率，再降码率，最后降分辨率；恢复时反向逐级。
/// `drops` = 上一个窗口内因发送队列满丢弃的帧数；`rtt` = 查看端回报的毫秒值（0 = 未知）。
fn next_adaptive(
    fps: u32,
    bitrate: u32,
    scale: f32,
    h264: bool,
    quality: u8,
    drops: u64,
    rtt: u64,
) -> (u32, u32, f32) {
    let congested = drops >= 3 || rtt > 200;
    let healthy = drops == 0 && rtt > 0 && rtt < 80;
    let mut fps = fps;
    let mut bitrate = bitrate;
    let mut scale = scale;
    if congested {
        fps = fps.saturating_sub(5).max(10);
        if h264 {
            bitrate = (bitrate * 3 / 4).max(600_000);
            if (rtt > 350 || drops >= 10) && scale > 0.5 {
                scale = if scale > 0.75 { 0.75 } else { 0.5 };
            }
        }
    } else if healthy {
        let max_fps = if h264 { 30 } else { JPEG_DEFAULT_FPS };
        if fps < max_fps {
            fps = (fps + 5).min(max_fps);
        } else if h264 && scale < 1.0 {
            scale = if scale < 0.75 { 0.75 } else { 1.0 };
        }
        if h264 {
            bitrate = (bitrate + bitrate / 4).min(bitrate_for(quality, 1.0));
        }
    }
    (fps, bitrate, scale)
}

fn try_encoder(w: u32, h: u32, quality: u8, scale: f32) -> Option<crate::peer::h264::H264Encoder> {
    let dw = scaled_dim(w, scale);
    let dh = scaled_dim(h, scale);
    let bitrate = bitrate_for(quality, scale);
    match crate::peer::h264::H264Encoder::new(dw, dh, 30, bitrate) {
        Ok(e) => {
            applog!("[peer-screen] H.264 编码器就绪: {dw}x{dh} @ {bitrate}bps（缩放 {scale:.2}）");
            Some(e)
        }
        Err(e) => {
            applog!("[peer-screen] H.264 不可用，继续 JPEG: {e}");
            None
        }
    }
}

fn stream_msg_json(
    codec: &str,
    fps: u32,
    bitrate: u32,
    width: u32,
    height: u32,
    reason: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({
        "type": "stream",
        "codec": codec,
        "fps": fps,
        "bitrate": bitrate,
        "width": width,
        "height": height,
        "reason": reason,
    })
}

fn emit_cursor(
    tx: &mpsc::Sender<serde_json::Value>,
    p: &PointerUpdate,
    src_w: u32,
    src_h: u32,
    shape_hash: &mut u64,
    last_visible: &mut bool,
    last_pos: &mut (i32, i32),
) {
    let mut shape_changed = false;
    if let Some(s) = &p.shape {
        let h = hash_samples(&s.rgba)
            ^ ((s.width as u64) << 32)
            ^ ((s.height as u64) << 16)
            ^ (s.hotspot_x as u32 as u64)
            ^ ((s.hotspot_y as u32 as u64) << 8);
        if h != *shape_hash {
            *shape_hash = h;
            shape_changed = true;
        }
    }
    if !shape_changed && *last_visible == p.visible && *last_pos == (p.x, p.y) {
        return;
    }
    *last_visible = p.visible;
    *last_pos = (p.x, p.y);
    let nx = if src_w > 0 {
        p.x as f64 / src_w as f64
    } else {
        0.0
    };
    let ny = if src_h > 0 {
        p.y as f64 / src_h as f64
    } else {
        0.0
    };
    let mut msg = serde_json::json!({
        "type": "cursor",
        "visible": p.visible,
        "nx": nx.clamp(0.0, 1.0),
        "ny": ny.clamp(0.0, 1.0),
        "srcWidth": src_w,
        "srcHeight": src_h,
    });
    if shape_changed {
        if let Some(s) = &p.shape {
            msg["shape"] = serde_json::json!({
                "width": s.width,
                "height": s.height,
                "hotspotX": s.hotspot_x,
                "hotspotY": s.hotspot_y,
                "rgba": base64_encode(&s.rgba),
            });
        }
    }
    let _ = tx.try_send(msg);
}

/// 无论正常结束还是提前 return，都取消采集线程的令牌。
struct CancelOnDrop(CancellationToken);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_stream(
    app: tauri::AppHandle,
    hub: Arc<ScreenHub>,
    session_id: String,
    peer: PeerRecord,
    peer_addr: Option<std::net::SocketAddr>,
    require_hello: bool,
    peer_name: String,
    addr: String,
    quality: u8,
    allow_input: bool,
    allow_clipboard: bool,
    allow_audio: bool,
    stream: russh::ChannelStream<russh::server::Msg>,
    cancel: CancellationToken,
    grant: crate::peer::server::GrantCheck,
) {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    // 采集线程是 detached std::thread：任何返回路径都必须取消它。
    let _cancel_guard = CancelOnDrop(cancel.clone());
    let client_gone = Arc::new(AtomicBool::new(false));
    let (mut rd, mut wr) = tokio::io::split(stream);
    applog!("[peer-screen] 流启动（协议 v{PROTO_VERSION}）");
    // 命令通道：reader → 采集线程
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let cmd_tx = Arc::new(std::sync::Mutex::new(cmd_tx));
    // 帧通道：H.264 必须保序，容量给小一点并让采集线程阻塞等待；
    // JPEG 仍可在拥塞时丢弃（每帧独立）。
    let (frame_tx, mut frame_rx) = mpsc::channel::<FramePacket>(8);
    // 采集线程 / reader → writer 的控制消息（pong / stream / error）
    let (ctrl_tx, mut ctrl_rx) = mpsc::channel::<serde_json::Value>(16);
    let rtt_ms = Arc::new(AtomicU64::new(0));
    // 授权撤销标记：后台每秒复核 peers.toml，撤销后优雅结束整条流
    // （采集线程是阻塞 std::thread，用线程安全的 CancellationToken 传信号）
    let revoked = CancellationToken::new();
    {
        let revoked = revoked.clone();
        let cancel_watch = cancel.clone();
        let grant = grant.clone();
        let ctrl_tx = ctrl_tx.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(5));
            loop {
                tokio::select! {
                    _ = revoked.cancelled() => break,
                    _ = cancel_watch.cancelled() => break,
                    _ = tick.tick() => {
                        if !grant.still_granted(|p| p.grant_screen).await {
                            applog!("[peer-screen] 屏幕授权已撤销，结束流");
                            // fatal：writer 发出这条错误后立即停止写帧。否则撤销后的
                            // 最后一两帧会把查看端刚显示的错误清掉（screenBus 收到新帧
                            // 会清 error），用户只看到冻结画面而没有断开原因。
                            let msg = serde_json::json!({
                                "type": "error",
                                "fatal": true,
                                "detail": "对端已撤销屏幕查看授权，连接已断开",
                            });
                            match tokio::time::timeout(Duration::from_secs(1), ctrl_tx.send(msg))
                                .await
                            {
                                Ok(Ok(())) => {}
                                _ => applog!("[peer-screen] 撤销通知入队失败或超时"),
                            }
                            // 给 writer 一点时间把错误发给查看端
                            tokio::time::sleep(Duration::from_millis(150)).await;
                            revoked.cancel();
                            break;
                        }
                    }
                }
            }
        });
    }

    // 读命令：ping 直接回 pong；stats 记录 RTT；其余转给采集线程
    {
        let cancel = cancel.clone();
        let revoked = revoked.clone();
        let cmd_tx = cmd_tx.clone();
        let ctrl_tx = ctrl_tx.clone();
        let rtt_ms = rtt_ms.clone();
        let grant_input = grant.clone();
        let sid_read = session_id.clone();
        let client_gone = client_gone.clone();
        tokio::spawn(async move {
            let mut len_buf = [0u8; 4];
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = revoked.cancelled() => break,
                    r = rd.read_exact(&mut len_buf) => {
                        if r.is_err() {
                            // 查看端通道已断：立即取消采集/编码/writer，而不是等下一帧写失败。
                            client_gone.store(true, Ordering::Relaxed);
                            cancel.cancel();
                            break;
                        }
                        let n = u32::from_be_bytes(len_buf) as usize;
                        if n == 0 || n > 64 * 1024 {
                            client_gone.store(true, Ordering::Relaxed);
                            cancel.cancel();
                            break;
                        }
                        let mut buf = vec![0u8; n];
                        if rd.read_exact(&mut buf).await.is_err() {
                            client_gone.store(true, Ordering::Relaxed);
                            cancel.cancel();
                            break;
                        }
                        match serde_json::from_slice::<ClientCmd>(&buf) {
                            Ok(ClientCmd::Ping { t }) => {
                                let _ = ctrl_tx
                                    .send(serde_json::json!({ "type": "pong", "t": t }))
                                    .await;
                            }
                            Ok(ClientCmd::Stats { rtt_ms: rtt }) => {
                                rtt_ms.store(rtt, Ordering::Relaxed);
                            }
                            Ok(ClientCmd::RequestInput) => {
                                // 会话进行中申请操作权限：本机确认 + 复核控制授权
                                let app = app.clone();
                                let hub = hub.clone();
                                let peer = peer.clone();
                                let ctrl = ctrl_tx.clone();
                                let sid = sid_read.clone();
                                let grant = grant_input.clone();
                                let allow_cb = allow_clipboard;
                                let allow_au = allow_audio;
                                tokio::spawn(async move {
                                    if !grant.still_granted(|p| p.grant_control).await {
                                        let _ = ctrl.try_send(serde_json::json!({
                                            "type": "error",
                                            "detail": "对端未授予控制权限，无法操作",
                                        }));
                                        return;
                                    }
                                    match approve(
                                        &hub,
                                        &app,
                                        &peer,
                                        peer_addr,
                                        require_hello,
                                        true,
                                        true,
                                    )
                                    .await
                                    {
                                        Ok(_) => {
                                            hub.set_allow_input(&sid, true).await;
                                            let _ = ctrl
                                                .send(serde_json::json!({
                                                    "type": "auth",
                                                    "token": sid,
                                                    "allowInput": true,
                                                    "clipboard": allow_cb,
                                                    "audio": allow_au,
                                                }))
                                                .await;
                                        }
                                        Err(e) => {
                                            applog!("[peer-screen] 操作权限请求失败: {e}");
                                            let _ = ctrl.try_send(serde_json::json!({
                                                "type": "error",
                                                "detail": format!("操作权限请求未通过: {e}"),
                                            }));
                                        }
                                    }
                                });
                            }
                            _ => {
                                if let Ok(tx) = cmd_tx.lock() {
                                    let _ = tx.send(buf);
                                }
                            }
                        }
                    }
                }
            }
        });
    }

    // 采集 + 编码线程
    {
        let cancel = cancel.clone();
        let revoked = revoked.clone();
        let rtt_ms = rtt_ms.clone();
        let ctrl_tx = ctrl_tx.clone();
        let mut quality = quality;
        std::thread::spawn(move || {
            let mut monitor = 0usize;
            // 显示器休眠 / 锁屏时 DXGI 会返回 ACCESS_DENIED：保持会话并每 2s 重试，
            // 桌面恢复后自动继续（不要把整条查看会话结束掉）。
            let mut source = loop {
                if cancel.is_cancelled() || revoked.is_cancelled() {
                    return;
                }
                match Source::desktop(monitor) {
                    Ok(c) => break c,
                    Err(e) => {
                        applog!("[peer] 屏幕采集初始化失败（2s 后重试）: {e}");
                        let _ = ctrl_tx.try_send(serde_json::json!({
                            "type": "error",
                            "detail": format!(
                                "屏幕采集暂不可用（显示器可能休眠/锁屏，或被其他采集程序占用）: {e}"
                            ),
                        }));
                        std::thread::sleep(Duration::from_secs(2));
                        if cancel.is_cancelled() || revoked.is_cancelled() {
                            return;
                        }
                    }
                }
            };

            let mut codec_is_h264 = false;
            let mut encoder: Option<crate::peer::h264::H264Encoder> = None;
            let mut scale = 1.0f32;
            let mut fps = JPEG_DEFAULT_FPS;
            let mut bitrate = 0u32;
            let mut drops_in_window = 0u64;
            let mut adapt_at = Instant::now();
            let mut last_sent = Instant::now() - KEYFRAME_INTERVAL;
            let mut last_hash = 0u64;
            let mut force_key = false;
            let mut seq = 0u64;
            let mut cursor_hash = 0u64;
            let mut cursor_visible = false;
            let mut cursor_pos = (i32::MIN, i32::MIN);

            let (w0, h0) = source.size();
            let _ = ctrl_tx.try_send(stream_msg_json("jpeg", fps, 0, w0, h0, None));

            loop {
                if cancel.is_cancelled() || revoked.is_cancelled() {
                    break;
                }
                // 处理命令（不阻塞采集）
                while let Ok(msg) = cmd_rx.try_recv() {
                    match serde_json::from_slice::<ClientCmd>(&msg) {
                        Ok(ClientCmd::Quality { q }) => {
                            quality = q.clamp(10, 95);
                            if codec_is_h264 {
                                bitrate = bitrate_for(quality, scale);
                                if let Some(e) = encoder.as_mut() {
                                    e.set_bitrate(bitrate);
                                }
                                let (w, h) = source.size();
                                let _ = ctrl_tx.try_send(stream_msg_json(
                                    "h264",
                                    fps,
                                    bitrate,
                                    scaled_dim(w, scale),
                                    scaled_dim(h, scale),
                                    None,
                                ));
                            }
                        }
                        Ok(ClientCmd::Monitor { index }) => {
                            monitor = index;
                            // 同一进程对同一显示器只允许一份桌面复制：
                            // 必须先释放旧采集，否则重建会拿到 E_INVALIDARG。
                            source.release();
                            match Source::desktop(monitor) {
                                Ok(c) => {
                                    source = c;
                                    last_hash = 0;
                                    force_key = true;
                                    let _ = ctrl_tx.try_send(serde_json::json!({
                                        "type": "source",
                                        "source": "desktop",
                                    }));
                                }
                                Err(e) => applog!("[peer-screen] 切换显示器失败: {e}"),
                            }
                            if codec_is_h264 {
                                let (w, h) = source.size();
                                encoder = try_encoder(w, h, quality, scale);
                                if encoder.is_none() {
                                    codec_is_h264 = false;
                                    fps = JPEG_DEFAULT_FPS;
                                    let _ = ctrl_tx.try_send(stream_msg_json(
                                        "jpeg",
                                        fps,
                                        0,
                                        w,
                                        h,
                                        Some("H.264 编码器重建失败"),
                                    ));
                                }
                            }
                        }
                        Ok(ClientCmd::Codec { codec }) => {
                            if codec.eq_ignore_ascii_case("h264") {
                                let (w, h) = source.size();
                                match try_encoder(w, h, quality, scale) {
                                    Some(e) => {
                                        encoder = Some(e);
                                        codec_is_h264 = true;
                                        fps = 30;
                                        bitrate = bitrate_for(quality, scale);
                                        scale = 1.0;
                                        force_key = true;
                                        last_hash = 0;
                                        let (w, h) = source.size();
                                        let _ = ctrl_tx.try_send(stream_msg_json(
                                            "h264",
                                            fps,
                                            bitrate,
                                            scaled_dim(w, scale),
                                            scaled_dim(h, scale),
                                            None,
                                        ));
                                    }
                                    None => {
                                        codec_is_h264 = false;
                                        encoder = None;
                                        fps = JPEG_DEFAULT_FPS;
                                        let _ = ctrl_tx.try_send(stream_msg_json(
                                            "jpeg",
                                            fps,
                                            0,
                                            w,
                                            h,
                                            Some("本机没有可用的 H.264 编码器"),
                                        ));
                                    }
                                }
                            } else {
                                codec_is_h264 = false;
                                encoder = None;
                                fps = JPEG_DEFAULT_FPS;
                                let (w, h) = source.size();
                                let _ =
                                    ctrl_tx.try_send(stream_msg_json("jpeg", fps, 0, w, h, None));
                            }
                        }
                        Ok(ClientCmd::Keyframe) => {
                            force_key = true;
                            if let Some(e) = encoder.as_mut() {
                                e.request_keyframe();
                            }
                        }
                        Ok(ClientCmd::Monitors) => {}
                        Ok(ClientCmd::Windows) => {
                            let items = platform::list_windows();
                            let _ = ctrl_tx.try_send(serde_json::json!({
                                "type": "windows",
                                "items": items,
                            }));
                        }
                        Ok(ClientCmd::Window { hwnd, title }) => match Source::window(hwnd) {
                            Ok(s) => {
                                source = s;
                                last_hash = 0;
                                force_key = true;
                                cursor_hash = 0;
                                let _ = ctrl_tx.try_send(serde_json::json!({
                                    "type": "source",
                                    "source": "window",
                                    "title": title,
                                }));
                                let _ = ctrl_tx.try_send(
                                    serde_json::json!({"type": "cursor", "visible": false}),
                                );
                                if codec_is_h264 {
                                    let (w, h) = source.size();
                                    encoder = try_encoder(w, h, quality, scale);
                                    if encoder.is_none() {
                                        codec_is_h264 = false;
                                        fps = JPEG_DEFAULT_FPS;
                                        let _ = ctrl_tx.try_send(stream_msg_json(
                                            "jpeg",
                                            fps,
                                            0,
                                            w,
                                            h,
                                            Some("H.264 编码器重建失败"),
                                        ));
                                    }
                                }
                            }
                            Err(e) => {
                                applog!("[peer-screen] 共享窗口失败: {e}");
                                let _ = ctrl_tx.try_send(serde_json::json!({
                                    "type": "error",
                                    "detail": format!("共享窗口失败: {e}"),
                                }));
                            }
                        },
                        Ok(ClientCmd::Desktop) => {
                            source.release();
                            match Source::desktop(monitor) {
                                Ok(s) => {
                                    source = s;
                                    last_hash = 0;
                                    force_key = true;
                                    cursor_hash = 0;
                                    let _ = ctrl_tx.try_send(serde_json::json!({
                                        "type": "source",
                                        "source": "desktop",
                                    }));
                                    if codec_is_h264 {
                                        let (w, h) = source.size();
                                        encoder = try_encoder(w, h, quality, scale);
                                        if encoder.is_none() {
                                            codec_is_h264 = false;
                                            fps = JPEG_DEFAULT_FPS;
                                            let _ = ctrl_tx.try_send(stream_msg_json(
                                                "jpeg",
                                                fps,
                                                0,
                                                w,
                                                h,
                                                Some("H.264 编码器重建失败"),
                                            ));
                                        }
                                    }
                                }
                                Err(e) => applog!("[peer-screen] 切回桌面失败: {e}"),
                            }
                        }
                        // 在 reader 任务里处理（需要本机确认），不会转发到这里
                        Ok(ClientCmd::RequestInput) => {}
                        Ok(ClientCmd::Ping { .. }) | Ok(ClientCmd::Stats { .. }) => {}
                        Err(_) => {}
                    }
                }

                // 自适应：每 3s 看一次积压/RTT（先降帧率 → 码率 → 分辨率）
                if adapt_at.elapsed() >= Duration::from_secs(3) {
                    let rtt = rtt_ms.load(Ordering::Relaxed);
                    let drops = drops_in_window;
                    drops_in_window = 0;
                    let old_fps = fps;
                    let old_bitrate = bitrate;
                    let (nfps, nbitrate, nscale) =
                        next_adaptive(fps, bitrate, scale, codec_is_h264, quality, drops, rtt);
                    fps = nfps;
                    bitrate = nbitrate;
                    let scale_changed = (nscale - scale).abs() > f32::EPSILON;
                    scale = nscale;
                    let mut changed = fps != old_fps || bitrate != old_bitrate || scale_changed;
                    if codec_is_h264 {
                        if let Some(e) = encoder.as_mut() {
                            e.set_bitrate(bitrate);
                        }
                        if scale_changed {
                            let (w, h) = source.size();
                            encoder = try_encoder(w, h, quality, scale);
                            if encoder.is_none() {
                                codec_is_h264 = false;
                                fps = JPEG_DEFAULT_FPS;
                                changed = true;
                            }
                        }
                    }
                    if changed {
                        let (w, h) = source.size();
                        let _ = ctrl_tx.try_send(stream_msg_json(
                            if codec_is_h264 { "h264" } else { "jpeg" },
                            fps,
                            bitrate,
                            if codec_is_h264 {
                                scaled_dim(w, scale)
                            } else {
                                w
                            },
                            if codec_is_h264 {
                                scaled_dim(h, scale)
                            } else {
                                h
                            },
                            if codec_is_h264 {
                                None
                            } else {
                                Some("JPEG（无 H.264 或已回退）")
                            },
                        ));
                    }
                    adapt_at = Instant::now();
                }

                // 按目标帧率节流“采集”，让 DXGI 把期间的更新合并进下一帧
                let interval = Duration::from_millis(1000 / fps.max(1) as u64);
                if let Some(rem) = interval.checked_sub(last_sent.elapsed()) {
                    std::thread::sleep(rem.min(Duration::from_millis(40)));
                }

                match source.grab(20) {
                    Ok(g) => {
                        let (w, h) = source.size();
                        if codec_is_h264 {
                            let want = (scaled_dim(w, scale), scaled_dim(h, scale));
                            let mismatch = encoder
                                .as_ref()
                                .map(|e| (e.width(), e.height()) != want)
                                .unwrap_or(true);
                            if mismatch {
                                encoder = try_encoder(w, h, quality, scale);
                                if encoder.is_none() {
                                    codec_is_h264 = false;
                                    fps = JPEG_DEFAULT_FPS;
                                    let _ = ctrl_tx.try_send(stream_msg_json(
                                        "jpeg",
                                        fps,
                                        0,
                                        w,
                                        h,
                                        Some("H.264 编码器重建失败"),
                                    ));
                                }
                            }
                        }
                        emit_cursor(
                            &ctrl_tx,
                            &g.pointer,
                            w,
                            h,
                            &mut cursor_hash,
                            &mut cursor_visible,
                            &mut cursor_pos,
                        );
                        let bgra = &g.bgra;
                        let stride = g.stride;
                        let hash = hash_samples(bgra);
                        let stale = last_sent.elapsed() >= KEYFRAME_INTERVAL;
                        if hash == last_hash && !stale && !force_key {
                            std::thread::sleep(Duration::from_millis(5));
                            continue;
                        }
                        let send_key = force_key || stale;
                        last_hash = hash;
                        last_sent = Instant::now();
                        if codec_is_h264 {
                            let mut fallback = None;
                            if let Some(enc) = encoder.as_mut() {
                                match enc.encode(bgra, stride, w, h, send_key) {
                                    Ok(frames) => {
                                        let (ew, eh) = (enc.width(), enc.height());
                                        for f in frames {
                                            seq = seq.wrapping_add(1);
                                            let ts = if f.ts_us > 0 { f.ts_us } else { seq * 1000 };
                                            let packet = FramePacket {
                                                width: ew,
                                                height: eh,
                                                ts_us: ts,
                                                key: f.key,
                                                h264: true,
                                                hd: h >= 720,
                                                data: f.data,
                                            };
                                            // H.264 的 P 帧引用前一帧，不能像 JPEG 一样
                                            // 随意丢；拥塞时让采集线程背压等待。
                                            if frame_tx.blocking_send(packet).is_err() {
                                                return;
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        applog!("[peer-screen] H.264 编码失败，回退 JPEG: {e}");
                                        fallback = Some(e);
                                    }
                                }
                            }
                            if let Some(reason) = fallback {
                                codec_is_h264 = false;
                                encoder = None;
                                fps = JPEG_DEFAULT_FPS;
                                let _ = ctrl_tx.try_send(stream_msg_json(
                                    "jpeg",
                                    fps,
                                    0,
                                    w,
                                    h,
                                    Some(&reason),
                                ));
                            }
                        } else {
                            match platform::encode_jpeg(bgra, w, h, stride, quality) {
                                Ok(jpeg) => {
                                    seq = seq.wrapping_add(1);
                                    let packet = FramePacket {
                                        width: w,
                                        height: h,
                                        ts_us: seq * 1000,
                                        key: true,
                                        h264: false,
                                        hd: false,
                                        data: jpeg,
                                    };
                                    if frame_tx.try_send(packet).is_err() {
                                        drops_in_window += 1;
                                    }
                                }
                                Err(e) => applog!("[peer] 屏幕 JPEG 编码失败: {e}"),
                            }
                        }
                        force_key = false;
                    }
                    Err(GrabError::NoChange) => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(GrabError::Lost(why)) | Err(GrabError::Failed(why)) => {
                        if why != "采集已释放" {
                            applog!("[peer] 屏幕采集重建（{why}）");
                        }
                        std::thread::sleep(Duration::from_millis(300));
                        if source.is_window() {
                            // 窗口关闭/捕获结束：回退整个桌面，避免整条会话中断
                            source.release();
                            match Source::desktop(monitor) {
                                Ok(c) => {
                                    source = c;
                                    last_hash = 0;
                                    force_key = true;
                                    let _ = ctrl_tx.try_send(serde_json::json!({
                                        "type": "source",
                                        "source": "desktop",
                                        "reason": why,
                                    }));
                                }
                                Err(e) => {
                                    applog!("[peer] 回退桌面失败（1s 后重试）: {e}");
                                    std::thread::sleep(Duration::from_secs(1));
                                }
                            }
                        } else {
                            // 先释放旧复制再重建，否则同一输出上的新复制会 E_INVALIDARG
                            source.release();
                            match Source::desktop(monitor) {
                                Ok(c) => {
                                    source = c;
                                    last_hash = 0;
                                    force_key = true;
                                }
                                Err(e) => {
                                    applog!("[peer] 屏幕采集重建失败（1s 后重试）: {e}");
                                    std::thread::sleep(Duration::from_secs(1));
                                }
                            }
                        }
                    }
                }
            }
        });
    }

    // 先发显示器列表 + 鉴权令牌
    match platform::list_monitors() {
        Ok(items) => {
            applog!("[peer-screen] 显示器 {} 个", items.len());
            if let Ok(json) = serde_json::to_vec(&ServerMsg::Monitors { items: &items }) {
                if write_ctrl(&mut wr, &json).await.is_err() {
                    return;
                }
            }
        }
        Err(e) => {
            applog!("[peer-screen] 显示器枚举失败: {e}");
            let _ = ctrl_tx.try_send(serde_json::json!({
                "type": "error",
                "detail": format!("显示器枚举失败: {e}"),
            }));
        }
    }
    // 令牌必须无条件发送：发送失败时查看端会一直停在“连接中”
    if let Ok(json) = serde_json::to_vec(&ServerMsg::Auth {
        token: &session_id,
        allow_input,
        clipboard: allow_clipboard,
        audio: allow_audio,
    }) {
        if write_ctrl(&mut wr, &json).await.is_err() {
            return;
        }
    }

    // writer（同时累计审计：帧数/字节数）
    let mut sent_frames = 0u64;
    let mut sent_bytes = 0u64;
    let mut last_w = 0u32;
    let mut last_h = 0u32;
    let started = Instant::now();
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            // 控制消息优先：帧通道关闭（None）或撤销时，已排队的错误/状态必须能发出去
            msg = ctrl_rx.recv() => {
                if let Some(v) = msg {
                    let fatal = v.get("fatal").and_then(|b| b.as_bool()).unwrap_or(false);
                    if let Ok(bytes) = serde_json::to_vec(&v) {
                        let ok = matches!(
                            tokio::time::timeout(WRITE_TIMEOUT, write_ctrl(&mut wr, &bytes)).await,
                            Ok(Ok(()))
                        );
                        if !ok {
                            break;
                        }
                    }
                    // 终止性错误（如授权撤销）必须是最后一条消息：写入后再补一帧
                    // 会把查看端刚显示的 error 清掉，只剩冻结画面。
                    if fatal {
                        break;
                    }
                }
            }
            // 授权撤销：错误消息发出后立即结束，不能再补一帧（会把查看端刚显示的
            // “授权已撤销”错误清掉）。撤销时 revoked 与 ctrl 同时就绪，ctrl 在上。
            _ = revoked.cancelled() => break,
            frame = frame_rx.recv() => match frame {
                Some(p) => {
                    sent_frames += 1;
                    sent_bytes += p.data.len() as u64;
                    last_w = p.width;
                    last_h = p.height;
                    // 写操作必须可超时：对端不读时不能把任务永久卡在 write 上
                    let ok = matches!(
                        tokio::time::timeout(
                            WRITE_TIMEOUT,
                            write_frame(&mut wr, p.width, p.height, p.ts_us, p.key, p.h264, p.hd, &p.data),
                        )
                        .await,
                        Ok(Ok(()))
                    );
                    if !ok {
                        applog!("[peer-screen] 写帧失败或超时");
                        break;
                    }
                }
                None => break,
            },
        }
    }
    let _ = tokio::time::timeout(Duration::from_secs(2), wr.shutdown()).await;
    revoked.cancel();
    crate::peer::audit::append(&crate::peer::audit::AuditEntry {
        at: crate::peer::audit::stamp(),
        kind: "screen".to_string(),
        direction: "incoming".to_string(),
        peer_name: peer_name.clone(),
        peer_addr: addr.clone(),
        quality: Some(quality),
        width: (last_w > 0).then_some(last_w),
        height: (last_h > 0).then_some(last_h),
        frames: Some(sent_frames),
        bytes: Some(sent_bytes),
        seconds: started.elapsed().as_secs(),
        reason: Some(
            if client_gone.load(Ordering::Relaxed) {
                "closed"
            } else if cancel.is_cancelled() {
                "kicked"
            } else if revoked.is_cancelled() {
                "revoked"
            } else {
                "closed"
            }
            .to_string(),
        ),
    });
    applog!("[peer-screen] 流结束: {sent_frames} 帧 / {sent_bytes} 字节 / 质量 {quality}");
}

fn hash_samples(bytes: &[u8]) -> u64 {
    // FNV-1a，步长取样：用于“静止画面”判定（不追求密码学强度）。
    let mut h = 0xcbf29ce484222325u64;
    let mut i = 0;
    while i < bytes.len() {
        h ^= bytes[i] as u64;
        h = h.wrapping_mul(0x100000001b3);
        i += 97;
    }
    h ^ bytes.len() as u64
}

// 帧头字段天然较多（尺寸/时间戳/关键帧/编码/HD），拆结构体只会多一层搬运。
#[allow(clippy::too_many_arguments)]
async fn write_frame<W: AsyncWriteExt + Unpin>(
    w: &mut W,
    width: u32,
    height: u32,
    ts_us: u64,
    key: bool,
    h264: bool,
    hd: bool,
    data: &[u8],
) -> std::io::Result<()> {
    let mut head = [0u8; 28];
    head[0..4].copy_from_slice(&FRAME_MAGIC);
    head[4] = PROTO_VERSION;
    head[5] = 0; // kind=frame
    let mut flags = 0u16;
    if key {
        flags |= 1;
    }
    if h264 {
        flags |= 2;
    }
    if hd {
        flags |= 4;
    }
    head[6..8].copy_from_slice(&flags.to_be_bytes());
    head[8..12].copy_from_slice(&width.to_be_bytes());
    head[12..16].copy_from_slice(&height.to_be_bytes());
    head[16..24].copy_from_slice(&ts_us.to_be_bytes());
    head[24..28].copy_from_slice(&(data.len() as u32).to_be_bytes());
    w.write_all(&head).await?;
    w.write_all(data).await
}

async fn write_ctrl<W: AsyncWriteExt + Unpin>(w: &mut W, json: &[u8]) -> std::io::Result<()> {
    let mut head = [0u8; 28];
    head[0..4].copy_from_slice(&CTRL_MAGIC);
    head[4] = PROTO_VERSION;
    head[5] = 1; // kind=ctrl
    head[24..28].copy_from_slice(&(json.len() as u32).to_be_bytes());
    w.write_all(&head).await?;
    w.write_all(json).await
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/* ---------------- 控制方（查看端） ---------------- */

/// 发送给前端的帧/状态消息（ipc Channel）。
#[derive(Debug, Clone, Serialize)]
#[serde(
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    tag = "kind"
)]
pub enum ScreenFrame {
    Frame {
        session_id: String,
        width: u32,
        height: u32,
        data: String,
        bytes: usize,
        /// `jpeg` 或 `h264`
        codec: String,
        /// H.264 IDR / JPEG 恒为 true
        key: bool,
        /// 源画面是否 HD（H.264 解码端据此声明 BT.709；JPEG 忽略）
        hd: bool,
        /// 服务端时间戳（微秒；WebCodecs 用）
        ts_us: u64,
    },
    Monitors {
        session_id: String,
        items: Vec<MonitorInfo>,
    },
    Info {
        session_id: String,
        monitor: usize,
        width: u32,
        height: u32,
    },
    Error {
        session_id: String,
        detail: String,
    },
    /// 输入鉴权令牌（允许操作时前端凭它开输入子系统）
    Auth {
        session_id: String,
        token: String,
        allow_input: bool,
        clipboard: bool,
        audio: bool,
        /// 对端是否支持「查看并操作」（对端版本较旧时为 false）
        input_supported: bool,
        /// 查看端是否请求了操作权限
        input_requested: bool,
    },
    /// 屏幕通道延迟（查看端每 2s ping 一次）
    Pong {
        session_id: String,
        t: i64,
    },
    /// 光标位置/形状（桌面采集；窗口共享时为不可见）
    Cursor {
        session_id: String,
        visible: bool,
        nx: f64,
        ny: f64,
        src_width: u32,
        src_height: u32,
        shape: Option<CursorShapeInfo>,
    },
    /// 可共享窗口列表
    Windows {
        session_id: String,
        items: Vec<WindowInfo>,
    },
    /// 当前采集源（整个桌面 / 指定窗口）
    Source {
        session_id: String,
        source: String,
        title: Option<String>,
        reason: Option<String>,
    },
    /// 编码/自适应状态（服务端主动下发）
    Stream {
        session_id: String,
        codec: String,
        fps: u32,
        bitrate: u32,
        width: u32,
        height: u32,
        reason: Option<String>,
    },
}

/// 查看端会话（命令转发 + 取消）。
pub struct ViewerSession {
    cmd_tx: mpsc::Sender<Vec<u8>>,
    /// 对端设备 id（输入通道复用）
    pub peer_id: String,
    pub cancel: CancellationToken,
    pub monitors: Vec<MonitorInfo>,
    pub quality: u8,
    pub monitor: usize,
}

#[derive(Default, Clone)]
pub struct ViewerManager {
    sessions: Arc<Mutex<HashMap<String, ViewerSession>>>,
}

impl ViewerManager {
    pub fn new() -> Self {
        Self::default()
    }

    async fn insert(&self, id: String, v: ViewerSession) {
        self.sessions.lock().await.insert(id, v);
    }

    pub async fn peer_id_of(&self, id: &str) -> Option<String> {
        let map = self.sessions.lock().await;
        let r = map.get(id).map(|v| v.peer_id.clone());
        r
    }

    async fn get(&self, id: &str) -> Option<Arc<mpsc::Sender<Vec<u8>>>> {
        self.sessions
            .lock()
            .await
            .get(id)
            .map(|v| Arc::new(v.cmd_tx.clone()))
    }

    async fn remove(&self, id: &str) -> Option<CancellationToken> {
        self.sessions.lock().await.remove(id).map(|v| {
            v.cancel.cancel();
            v.cancel
        })
    }

    pub async fn command(&self, id: &str, cmd: &serde_json::Value) -> bool {
        let Some(tx) = self.get(id).await else {
            return false;
        };
        let Ok(bytes) = serde_json::to_vec(cmd) else {
            return false;
        };
        let mut frame = Vec::with_capacity(4 + bytes.len());
        frame.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        frame.extend_from_slice(&bytes);
        tx.send(frame).await.is_ok()
    }

    pub async fn close(&self, id: &str) -> bool {
        self.remove(id).await.is_some()
    }

    pub async fn close_all(&self) {
        let mut map = self.sessions.lock().await;
        for (_, v) in map.drain() {
            v.cancel.cancel();
        }
    }
}

/// 查看端：连接 + 授权预检 + 帧解析 → ipc Channel。
pub async fn open_viewer(
    state: &AppState,
    peer_id: &str,
    quality: u8,
    request_input: bool,
    on_frame: Channel<ScreenFrame>,
) -> AppResult<String> {
    let peer = state
        .peer
        .peer_record(peer_id)
        .await
        .ok_or_else(|| AppError::EPeerNotPaired("该设备未配对".to_string()))?;
    let need_hello = state.config.read().await.preferences.require_windows_hello;
    if need_hello {
        let peer_name = peer.name.clone();
        let outcome = tauri::async_runtime::spawn_blocking(move || {
            crate::hello::verify(&format!("查看「{peer_name}」的屏幕"))
        })
        .await
        .map_err(|e| AppError::EPeerHelloDenied(format!("Windows Hello 线程失败: {e}")))?;
        if outcome != crate::hello::HelloOutcome::Verified {
            return Err(AppError::EPeerHelloDenied(format!(
                "Windows Hello 未验证通过（{outcome:?}），已取消查看"
            )));
        }
    }

    let identity = state.peer.identity_handle().await?;
    let mut handle = client::connect(state, &peer, "屏幕查看").await?;
    client::authenticate(&mut handle, &identity).await?;

    // ruthenium-info 预检 grantScreen
    let info = crate::peer::control::query_info(&handle, None).await?;
    state.peer.sync_peer_allows(&peer.id, &info).await;
    if !info.grant_screen {
        return Err(AppError::EPeerNotAllowed(format!(
            "{} 未允许查看屏幕（可能已撤销授权）",
            peer.name
        )));
    }
    // 对端不认 `ruthenium-screen+input` 时按仅查看打开，前端会提示需升级对端
    let input_supported = info.screen_input;
    let effective_input = request_input && input_supported;

    // 打开子系统（等回执，被拒立刻报错）
    let mut channel = handle
        .channel_open_session()
        .await
        .map_err(|e| AppError::ESshChannel(format!("打开屏幕通道失败: {e}")))?;
    let subsystem = if effective_input {
        "ruthenium-screen+input"
    } else {
        "ruthenium-screen"
    };
    channel
        .request_subsystem(true, subsystem)
        .await
        .map_err(|e| AppError::EPeerNotAllowed(format!("对端拒绝屏幕查看: {e}")))?;
    match tokio::time::timeout(SCREEN_HANDSHAKE_TIMEOUT, channel.wait()).await {
        Ok(Some(russh::ChannelMsg::Success)) => {}
        Ok(Some(russh::ChannelMsg::Failure)) => {
            return Err(AppError::EPeerNotAllowed(
                "对端拒绝屏幕查看：未授权，或未在 30 秒内确认".to_string(),
            ))
        }
        Ok(None) => return Err(AppError::EPeerNotAllowed("对端关闭了屏幕通道".to_string())),
        Err(_) => {
            return Err(AppError::EPeerConsentTimeout(
                "等待对端确认屏幕查看超时（需在对方设备上点「允许」）".to_string(),
            ))
        }
        Ok(Some(_)) => {}
    }
    let stream = channel.into_stream();
    let session_id = Uuid::new_v4().to_string();
    let (mut rd, wr) = tokio::io::split(stream);
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<Vec<u8>>(8);
    let cancel = CancellationToken::new();

    // 命令 writer
    {
        let cancel = cancel.clone();
        let mut wr = wr;
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

    // 帧 reader → Channel
    let quality = quality.clamp(10, 95);
    {
        let cancel = cancel.clone();
        let hub = state.screen.clone();
        let sid = session_id.clone();
        let peer_name = peer.name.clone();
        let handle_keepalive = handle;
        tokio::spawn(async move {
            // 保证连接在本任务存活期间不被关闭
            let _keep = handle_keepalive;
            let mut head = [0u8; 28];
            let mut saw_error = false;
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    r = rd.read_exact(&mut head) => {
                        if r.is_err() { break; }
                        let magic = &head[0..4];
                        if head[4] != PROTO_VERSION {
                            // 两端协议版本不同时不能静默断流：给出可定位的错误
                            let _ = on_frame.send(ScreenFrame::Error {
                                session_id: sid.clone(),
                                detail: format!(
                                    "屏幕协议版本不匹配：本机 v{PROTO_VERSION}，对端 v{}；请把两端升级到同一版本后重试",
                                    head[4]
                                ),
                            });
                            break;
                        }
                        let flags = u16::from_be_bytes(head[6..8].try_into().unwrap_or([0;2]));
                        let w = u32::from_be_bytes(head[8..12].try_into().unwrap_or([0;4]));
                        let h = u32::from_be_bytes(head[12..16].try_into().unwrap_or([0;4]));
                        let ts_us = u64::from_be_bytes(head[16..24].try_into().unwrap_or([0;8]));
                        let n = u32::from_be_bytes(head[24..28].try_into().unwrap_or([0;4])) as usize;
                        if n > 8 * 1024 * 1024 { break; }
                        let mut body = vec![0u8; n];
                        if rd.read_exact(&mut body).await.is_err() { break; }
                        if magic == FRAME_MAGIC {
                            let _ = on_frame.send(ScreenFrame::Frame {
                                session_id: sid.clone(),
                                width: w,
                                height: h,
                                data: base64_encode(&body),
                                bytes: body.len(),
                                codec: if flags & 2 != 0 { "h264".to_string() } else { "jpeg".to_string() },
                                key: flags & 1 != 0,
                                hd: flags & 4 != 0,
                                ts_us,
                            });
                        } else if magic == CTRL_MAGIC {
                            if let Ok(serde_json::Value::Object(obj)) = serde_json::from_slice::<serde_json::Value>(&body) {
                                match obj.get("type").and_then(|v| v.as_str()) {
                                    Some("monitors") => {
                                        if let Ok(items) = serde_json::from_value::<Vec<MonitorInfo>>(
                                            obj.get("items").cloned().unwrap_or_default(),
                                        ) {
                                            if let Some(v) = hub.sessions.lock().await.get_mut(&sid) {
                                                v.monitors = items.clone();
                                            }
                                            let _ = on_frame.send(ScreenFrame::Monitors {
                                                session_id: sid.clone(),
                                                items,
                                            });
                                        }
                                    }
                                    Some("monitor") => {
                                        let idx = obj.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                                        if let Some(v) = hub.sessions.lock().await.get_mut(&sid) {
                                            v.monitor = idx;
                                        }
                                        let _ = on_frame.send(ScreenFrame::Info {
                                            session_id: sid.clone(),
                                            monitor: idx,
                                            width: w,
                                            height: h,
                                        });
                                    }
                                    Some("error") => {
                                        let detail = obj.get("detail").and_then(|v| v.as_str()).unwrap_or("屏幕流错误").to_string();
                                        saw_error = true;
                                        let _ = on_frame.send(ScreenFrame::Error { session_id: sid.clone(), detail });
                                    }
                                    Some("auth") => {
                                        let token = obj
                                            .get("token")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("")
                                            .to_string();
                                        let allow_input = obj
                                            .get("allowInput")
                                            .and_then(|v| v.as_bool())
                                            .unwrap_or(false);
                                        let clipboard = obj
                                            .get("clipboard")
                                            .and_then(|v| v.as_bool())
                                            .unwrap_or(false);
                                        let audio = obj
                                            .get("audio")
                                            .and_then(|v| v.as_bool())
                                            .unwrap_or(false);
                                        let _ = on_frame.send(ScreenFrame::Auth {
                                            session_id: sid.clone(),
                                            token,
                                            allow_input,
                                            clipboard,
                                            audio,
                                            input_supported,
                                            input_requested: request_input,
                                        });
                                    }
                                    Some("pong") => {
                                        let t = obj.get("t").and_then(|v| v.as_i64()).unwrap_or(0);
                                        let _ = on_frame.send(ScreenFrame::Pong {
                                            session_id: sid.clone(),
                                            t,
                                        });
                                    }
                                    Some("stream") => {
                                        let codec = obj
                                            .get("codec")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("jpeg")
                                            .to_string();
                                        let fps = obj.get("fps").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                                        let bitrate =
                                            obj.get("bitrate").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                                        let width = obj.get("width").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                                        let height =
                                            obj.get("height").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                                        let reason = obj
                                            .get("reason")
                                            .and_then(|v| v.as_str())
                                            .map(|s| s.to_string());
                                        let _ = on_frame.send(ScreenFrame::Stream {
                                            session_id: sid.clone(),
                                            codec,
                                            fps,
                                            bitrate,
                                            width,
                                            height,
                                            reason,
                                        });
                                    }
                                    Some("cursor") => {
                                        let shape = obj.get("shape").and_then(|v| v.as_object()).map(|s| {
                                            CursorShapeInfo {
                                                width: s.get("width").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
                                                height: s.get("height").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
                                                hotspot_x: s.get("hotspotX").and_then(|x| x.as_i64()).unwrap_or(0) as i32,
                                                hotspot_y: s.get("hotspotY").and_then(|x| x.as_i64()).unwrap_or(0) as i32,
                                                rgba: s.get("rgba").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                                            }
                                        });
                                        let _ = on_frame.send(ScreenFrame::Cursor {
                                            session_id: sid.clone(),
                                            visible: obj.get("visible").and_then(|v| v.as_bool()).unwrap_or(false),
                                            nx: obj.get("nx").and_then(|v| v.as_f64()).unwrap_or(0.0),
                                            ny: obj.get("ny").and_then(|v| v.as_f64()).unwrap_or(0.0),
                                            src_width: obj.get("srcWidth").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                                            src_height: obj.get("srcHeight").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                                            shape,
                                        });
                                    }
                                    Some("windows") => {
                                        if let Ok(items) = serde_json::from_value::<Vec<WindowInfo>>(
                                            obj.get("items").cloned().unwrap_or_default(),
                                        ) {
                                            let _ = on_frame.send(ScreenFrame::Windows {
                                                session_id: sid.clone(),
                                                items,
                                            });
                                        }
                                    }
                                    Some("source") => {
                                        let _ = on_frame.send(ScreenFrame::Source {
                                            session_id: sid.clone(),
                                            source: obj
                                                .get("source")
                                                .and_then(|v| v.as_str())
                                                .unwrap_or("desktop")
                                                .to_string(),
                                            title: obj
                                                .get("title")
                                                .and_then(|v| v.as_str())
                                                .map(|s| s.to_string()),
                                            reason: obj
                                                .get("reason")
                                                .and_then(|v| v.as_str())
                                                .map(|s| s.to_string()),
                                        });
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                }
            }
            // 已收到明确错误（如“授权已撤销”）或用户主动关闭时不要再覆盖成“连接已断开”
            if !saw_error && !cancel.is_cancelled() {
                let _ = on_frame.send(ScreenFrame::Error {
                    session_id: sid.clone(),
                    detail: "连接已断开（对端或网络中断）".to_string(),
                });
            }
            hub.remove(&sid).await;
            applog!("[peer] 屏幕查看连接结束: {peer_name}");
        });
    }

    state
        .screen
        .insert(
            session_id.clone(),
            ViewerSession {
                cmd_tx: cmd_tx.clone(),
                peer_id: peer.id.clone(),
                cancel: cancel.clone(),
                monitors: Vec::new(),
                quality,
                monitor: 0,
            },
        )
        .await;
    // 初始：拉显示器列表 + 设置质量（控制消息与命令接口一样：u32 大端长度 + JSON）
    for msg in [
        serde_json::json!({ "cmd": "monitors" }),
        serde_json::json!({ "cmd": "quality", "q": quality }),
    ] {
        let bytes = serde_json::to_vec(&msg).unwrap_or_default();
        let mut framed = Vec::with_capacity(4 + bytes.len());
        framed.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        framed.extend_from_slice(&bytes);
        let _ = cmd_tx.send(framed).await;
    }
    Ok(session_id)
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    STANDARD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_guard_cancels_on_any_return() {
        let token = CancellationToken::new();
        {
            let _guard = CancelOnDrop(token.clone());
            assert!(!token.is_cancelled());
        }
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn screen_finish_cancels_downstream_channels() {
        let hub = ScreenHub::new();
        let token = CancellationToken::new();
        hub.register(
            "s1".to_string(),
            ViewSession {
                peer_name: "peer".to_string(),
                peer_addr: "127.0.0.1:1".to_string(),
                quality: 65,
                monitor: 0,
                allow_input: true,
                started_ms: 0,
                cancel: token.clone(),
            },
        )
        .await;
        assert!(hub.finish("s1").await.is_some());
        assert!(token.is_cancelled(), "屏幕结束后输入/音频通道必须收到取消");
    }

    #[test]
    fn adaptive_degrades_fps_then_bitrate_then_scale() {
        // 丢帧：先降 fps + 码率
        let (fps, br, sc) = next_adaptive(30, 4_000_000, 1.0, true, 65, 4, 50);
        assert_eq!(fps, 25);
        assert_eq!(br, 3_000_000);
        assert!((sc - 1.0).abs() < f32::EPSILON);
        // 严重拥塞：再降分辨率
        let (_, _, sc) = next_adaptive(25, 3_000_000, 1.0, true, 65, 12, 400);
        assert!((sc - 0.75).abs() < f32::EPSILON);
        let (_, _, sc) = next_adaptive(20, 2_000_000, 0.75, true, 65, 0, 500);
        assert!((sc - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn adaptive_recovers_after_stable_window() {
        // 健康：先恢复帧率，再恢复分辨率与码率
        let (fps, _, _) = next_adaptive(15, 1_000_000, 0.5, true, 65, 0, 20);
        assert_eq!(fps, 20);
        let (fps, _, sc) = next_adaptive(30, 2_000_000, 0.5, true, 65, 0, 20);
        assert_eq!(fps, 30);
        assert!((sc - 0.75).abs() < f32::EPSILON);
        // JPEG 帧率上限 15
        let (fps, br, _) = next_adaptive(15, 0, 1.0, false, 65, 0, 20);
        assert_eq!(fps, 15);
        assert_eq!(br, 0);
    }

    /// 枚举上的 rename_all 不覆盖变体字段（PairEvent 同样如此）。
    #[test]
    fn screen_frame_fields_are_camel_case() {
        let v = serde_json::to_value(ScreenFrame::Frame {
            session_id: "s1".into(),
            width: 100,
            height: 50,
            data: "abc".into(),
            bytes: 3,
            codec: "h264".into(),
            key: true,
            hd: true,
            ts_us: 42,
        })
        .expect("serialize screen frame");
        assert_eq!(v["kind"], "frame");
        assert_eq!(v["sessionId"], "s1");
        assert_eq!(v["codec"], "h264");
        assert_eq!(v["key"], true);
        assert_eq!(v["tsUs"], 42);
        assert_eq!(v["width"], 100);
        assert!(v.get("session_id").is_none());

        let m = serde_json::to_value(ScreenFrame::Monitors {
            session_id: "s1".into(),
            items: vec![MonitorInfo {
                index: 0,
                width: 10,
                height: 10,
                name: "\\\\.\\DISPLAY1".into(),
            }],
        })
        .expect("serialize monitors");
        assert_eq!(m["kind"], "monitors");
        assert_eq!(m["items"][0]["width"], 10);
    }

    /// 静止画面判定用的采样哈希：同一内容稳定，关键采样点变化必须变。
    #[test]
    fn hash_samples_is_stable_and_sensitive() {
        let a = vec![7u8; 1000];
        assert_eq!(hash_samples(&a), hash_samples(&a));
        let mut b = a.clone();
        b[485] = 9; // 485 = 97 * 5（被采样）
        assert_ne!(hash_samples(&a), hash_samples(&b));
    }

    /// 竖屏/倒屏：复制结果是未旋转像素，必须按 Rotation 转正（否则画面横倒）。
    #[cfg(windows)]
    #[test]
    fn rotate_bgra_handles_all_orientations() {
        use super::platform::{rotate_bgra, rotated_dims};
        use windows::Win32::Graphics::Dxgi::Common::{
            DXGI_MODE_ROTATION_IDENTITY, DXGI_MODE_ROTATION_ROTATE180,
            DXGI_MODE_ROTATION_ROTATE270, DXGI_MODE_ROTATION_ROTATE90,
        };

        // 2x2，每个像素用 B 通道区分：A B / C D
        let mut src = Vec::new();
        for v in [1u8, 2, 3, 4] {
            src.extend_from_slice(&[v, 0, 0, 255]);
        }
        let stride = 2 * 4;
        let first = |buf: &[u8], i: usize| buf[i * 4];

        assert_eq!(rotated_dims(2, 2, DXGI_MODE_ROTATION_ROTATE90), (2, 2));
        assert_eq!(rotated_dims(4, 2, DXGI_MODE_ROTATION_ROTATE90), (2, 4));

        // 90° 顺时针：C A / D B
        let mut out = Vec::new();
        let s = rotate_bgra(&src, stride, 2, 2, DXGI_MODE_ROTATION_ROTATE90, &mut out);
        assert_eq!(s, 8);
        assert_eq!(
            [
                first(&out, 0),
                first(&out, 1),
                first(&out, 2),
                first(&out, 3)
            ],
            [3, 1, 4, 2]
        );

        // 180°：D C / B A
        rotate_bgra(&src, stride, 2, 2, DXGI_MODE_ROTATION_ROTATE180, &mut out);
        assert_eq!(
            [
                first(&out, 0),
                first(&out, 1),
                first(&out, 2),
                first(&out, 3)
            ],
            [4, 3, 2, 1]
        );

        // 270°（逆时针 90°）：B D / A C
        rotate_bgra(&src, stride, 2, 2, DXGI_MODE_ROTATION_ROTATE270, &mut out);
        assert_eq!(
            [
                first(&out, 0),
                first(&out, 1),
                first(&out, 2),
                first(&out, 3)
            ],
            [2, 4, 1, 3]
        );

        // Identity 原样
        rotate_bgra(&src, stride, 2, 2, DXGI_MODE_ROTATION_IDENTITY, &mut out);
        assert_eq!(
            [
                first(&out, 0),
                first(&out, 1),
                first(&out, 2),
                first(&out, 3)
            ],
            [1, 2, 3, 4]
        );
    }
}
