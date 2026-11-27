//! 采集健康状态：把「黑屏 / 冻结 / 点不动」翻译成可解释、可上报的状态。
//!
//! 边界：只做探测与判定，不采集、不编码、不弹 UI。Windows 探测失败一律按
//! 「无法确认」处理，调用方不能因为探测失败中断屏幕流；状态机在下一轮观测
//! 会自动回到 `Live`。

/// 上报给查看端的采集状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CaptureState {
    /// 正常出帧（静止画面也算正常：桌面没有变化不是故障）。
    Live,
    /// 采集正在重建（ACCESS_LOST / 设备重置）。
    Retrying,
    /// 当前输入桌面不是 Default：UAC / 锁屏 / 登录界面。
    SecureDesktop,
    /// 窗口共享的采集目标启用了 WDA 防截屏（整屏共享不受影响）。
    ProtectedWindow,
    /// 疑似独占全屏：前台是全屏 D3D 应用，且画面冻结/变黑/复制反复丢失。
    ExclusiveFullscreen,
    /// 画面持续全黑：可能是受保护内容（DRM）、独占全屏或显示器休眠。
    SuspectedBlackout,
}

impl CaptureState {
    /// 上报与日志用的稳定短名（与前端 `state` 字段一致）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Retrying => "retrying",
            Self::SecureDesktop => "secureDesktop",
            Self::ProtectedWindow => "protectedWindow",
            Self::ExclusiveFullscreen => "exclusiveFullscreen",
            Self::SuspectedBlackout => "suspectedBlackout",
        }
    }
}

/// 输入桌面探测结果分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DesktopKind {
    /// `WinSta0\Default`：普通用户桌面。
    #[default]
    Default,
    /// 非 Default 桌面（Winlogon / 锁屏 / 登录），或输入桌面拒绝访问。
    Secure,
    /// 能打开输入桌面但读不到名字：按「无法确认」处理，不触发状态切换。
    Unknown,
}

/// 一次桌面探测：`kind` 用于状态判定，`name` 只用于日志与详情。
#[derive(Debug, Clone, Default)]
pub struct DesktopProbe {
    pub kind: DesktopKind,
    pub name: String,
}

/// 本轮采集结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrabOutcome {
    /// 拿到了新帧。
    Frame,
    /// 画面没有变化（`AcquireNextFrame` 超时）。
    NoChange,
    /// 采集失效，需要重建。
    Lost,
    /// 其他采集失败。
    Failed,
}

/// 一次「探测 + 采集」的观测，喂给 [`HealthTracker::observe`]。
#[derive(Debug, Clone)]
pub struct Observation {
    pub desktop: DesktopProbe,
    /// 采集目标是否启用防截屏（仅窗口共享的目标；整屏共享恒为 false）。
    pub protected: bool,
    /// 前台是否为全屏 D3D 应用（`QUNS_RUNNING_D3D_FULL_SCREEN`）。
    pub d3d_fullscreen: bool,
    pub grab: GrabOutcome,
    /// 仅在 `grab == Frame` 时有效：画面是否接近全黑。
    pub frame_dark: bool,
}

/// 状态变化。只在变化时产生，避免每帧刷控制消息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateChange {
    pub from: CaptureState,
    pub to: CaptureState,
    pub detail: String,
}

/// 连续观测多少次才确认状态切换（一次观测约等于一个采集循环）。
const CONFIRM_TICKS: u32 = 2;
/// 独占全屏：复制连续丢失/画面连续全黑的阈值。
const FSE_LOST_TICKS: u32 = 2;
const FSE_DARK_TICKS: u32 = 2;
/// 桌面复制连续失败多少次按「重建中」上报。
const RETRY_LOST_TICKS: u32 = 3;
/// 画面持续全黑多少次按「疑似黑屏内容」上报。阈值偏大（约 2s），避免深色影片
/// 或深色主题应用一出现就提示。
const BLACKOUT_DARK_TICKS: u32 = 60;

/// 采集健康状态机（阈值与切换规则集中在 `observe` 里）。
#[derive(Debug)]
pub struct HealthTracker {
    state: CaptureState,
    secure_ticks: u32,
    protected_ticks: u32,
    lost_ticks: u32,
    dark_ticks: u32,
}

impl Default for HealthTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl HealthTracker {
    pub fn new() -> Self {
        Self {
            state: CaptureState::Live,
            secure_ticks: 0,
            protected_ticks: 0,
            lost_ticks: 0,
            dark_ticks: 0,
        }
    }

    pub fn state(&self) -> CaptureState {
        self.state
    }

    /// 喂一次观测；状态变化时返回变化内容，未变化返回 `None`。
    pub fn observe(&mut self, obs: &Observation) -> Option<StateChange> {
        self.update_counters(obs);
        let to = self.target(obs);
        if to == self.state {
            return None;
        }
        let from = self.state;
        self.state = to;
        Some(StateChange {
            from,
            to,
            detail: describe(to, obs).to_string(),
        })
    }

    fn update_counters(&mut self, obs: &Observation) {
        if obs.desktop.kind == DesktopKind::Secure {
            self.secure_ticks = self.secure_ticks.saturating_add(1);
        } else {
            self.secure_ticks = 0;
        }
        if obs.protected {
            self.protected_ticks = self.protected_ticks.saturating_add(1);
        } else {
            self.protected_ticks = 0;
        }
        match obs.grab {
            GrabOutcome::Lost | GrabOutcome::Failed => {
                self.lost_ticks = self.lost_ticks.saturating_add(1);
            }
            GrabOutcome::Frame | GrabOutcome::NoChange => self.lost_ticks = 0,
        }
        if obs.grab == GrabOutcome::Frame && obs.frame_dark {
            self.dark_ticks = self.dark_ticks.saturating_add(1);
        } else {
            self.dark_ticks = 0;
        }
    }

    /// 按优先级选择目标状态：安全桌面 > 受保护窗口 > 独占全屏 > 重建 > 黑屏内容。
    fn target(&self, obs: &Observation) -> CaptureState {
        if self.secure_ticks >= CONFIRM_TICKS {
            CaptureState::SecureDesktop
        } else if self.protected_ticks >= CONFIRM_TICKS {
            CaptureState::ProtectedWindow
        } else if obs.d3d_fullscreen
            && (self.lost_ticks >= FSE_LOST_TICKS || self.dark_ticks >= FSE_DARK_TICKS)
        {
            CaptureState::ExclusiveFullscreen
        } else if self.lost_ticks >= RETRY_LOST_TICKS {
            CaptureState::Retrying
        } else if self.dark_ticks >= BLACKOUT_DARK_TICKS {
            CaptureState::SuspectedBlackout
        } else {
            CaptureState::Live
        }
    }
}

fn describe(state: CaptureState, obs: &Observation) -> &'static str {
    match state {
        CaptureState::Live => "已恢复出帧",
        CaptureState::Retrying => "桌面复制持续失败，正在重建",
        CaptureState::SecureDesktop => {
            if obs.desktop.name.is_empty() {
                "输入桌面不可访问（UAC/锁屏/登录界面）"
            } else {
                "输入桌面不是 Default（UAC/锁屏/登录界面）"
            }
        }
        CaptureState::ProtectedWindow => "采集目标启用了防截屏（WDA）",
        CaptureState::ExclusiveFullscreen => {
            "疑似独占全屏：前台为全屏 D3D 应用，画面冻结/变黑或复制丢失"
        }
        CaptureState::SuspectedBlackout => "画面持续全黑：可能是受保护内容、独占全屏或显示器休眠",
    }
}

/// 桌面名归类：只有 `Default` 算普通桌面，其余（Winlogon/锁屏等）一律按安全桌面处理。
pub fn classify_desktop_name(name: &str) -> DesktopKind {
    if name.is_empty() {
        DesktopKind::Unknown
    } else if name.eq_ignore_ascii_case("Default") {
        DesktopKind::Default
    } else {
        DesktopKind::Secure
    }
}

/// 采样判断画面是否接近全黑：超过 98% 的采样像素三通道都低于 24。
///
/// 只用于状态提示（阈值偏保守），不用于编码或画质判断。
pub fn frame_is_dark(bgra: &[u8], stride: usize, width: u32, height: u32) -> bool {
    let (w, h) = (width as usize, height as usize);
    if w == 0 || h == 0 || stride < w.saturating_mul(4) {
        return false;
    }
    let pixels = w * h;
    // 约 1024 个采样点：1080p 下步长 ~2000，避免逐像素扫描。
    let step = (pixels / 1024).max(1);
    let mut sampled = 0usize;
    let mut dark = 0usize;
    let mut i = 0usize;
    while i < pixels {
        let (x, y) = (i % w, i / w);
        let off = y * stride + x * 4;
        if off + 2 < bgra.len() {
            if bgra[off] < 24 && bgra[off + 1] < 24 && bgra[off + 2] < 24 {
                dark += 1;
            }
            sampled += 1;
        }
        i += step;
    }
    sampled >= 64 && dark * 100 >= sampled * 98
}

/* ---------------- Windows 探测 ---------------- */

/// 探测当前输入桌面。打不开输入桌面时按安全桌面处理：普通进程此时通常是被
/// Winlogon 桌面的 DACL 拒绝（UAC/锁屏）。
#[cfg(windows)]
pub fn probe_desktop() -> DesktopProbe {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::StationsAndDesktops::{
        CloseDesktop, GetUserObjectInformationW, OpenInputDesktop, DESKTOP_READOBJECTS, UOI_NAME,
    };

    unsafe {
        let Ok(desk) = OpenInputDesktop(Default::default(), false, DESKTOP_READOBJECTS) else {
            return DesktopProbe {
                kind: DesktopKind::Secure,
                name: String::new(),
            };
        };
        let mut buf = [0u16; 64];
        let mut needed = 0u32;
        let read = GetUserObjectInformationW(
            HANDLE(desk.0),
            UOI_NAME,
            Some(buf.as_mut_ptr().cast()),
            (buf.len() * 2) as u32,
            Some(&mut needed),
        );
        let _ = CloseDesktop(desk);
        if read.is_err() {
            return DesktopProbe {
                kind: DesktopKind::Unknown,
                name: String::new(),
            };
        }
        let len = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
        let name = String::from_utf16_lossy(&buf[..len]);
        DesktopProbe {
            kind: classify_desktop_name(&name),
            name,
        }
    }
}

/// 指定窗口是否启用了 WDA 防截屏（仅窗口共享时用于判定与提示）。
#[cfg(windows)]
pub fn probe_window_protected(hwnd: u64) -> bool {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE, WDA_MONITOR,
    };

    if hwnd == 0 {
        return false;
    }
    let mut affinity = 0u32;
    let hwnd = HWND(hwnd as usize as *mut core::ffi::c_void);
    if unsafe { GetWindowDisplayAffinity(hwnd, &mut affinity) }.is_err() {
        return false;
    }
    affinity == WDA_EXCLUDEFROMCAPTURE.0 || affinity == WDA_MONITOR.0
}

/// 前台是否为全屏 D3D 应用（`SHQueryUserNotificationState` 的官方信号）。
#[cfg(windows)]
pub fn probe_d3d_fullscreen() -> bool {
    use windows::Win32::UI::Shell::{SHQueryUserNotificationState, QUNS_RUNNING_D3D_FULL_SCREEN};

    unsafe {
        SHQueryUserNotificationState()
            .map(|s| s.0 == QUNS_RUNNING_D3D_FULL_SCREEN.0)
            .unwrap_or(false)
    }
}

/* ---------------- 非 Windows 占位 ---------------- */

#[cfg(not(windows))]
pub fn probe_desktop() -> DesktopProbe {
    DesktopProbe {
        kind: DesktopKind::Unknown,
        name: String::new(),
    }
}

#[cfg(not(windows))]
pub fn probe_window_protected(_hwnd: u64) -> bool {
    false
}

#[cfg(not(windows))]
pub fn probe_d3d_fullscreen() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(kind: DesktopKind) -> DesktopProbe {
        DesktopProbe {
            kind,
            name: match kind {
                DesktopKind::Default => "Default".to_string(),
                DesktopKind::Secure => "Winlogon".to_string(),
                DesktopKind::Unknown => String::new(),
            },
        }
    }

    fn obs(
        desktop: DesktopKind,
        grab: GrabOutcome,
        dark: bool,
        protected: bool,
        d3d: bool,
    ) -> Observation {
        Observation {
            desktop: probe(desktop),
            protected,
            d3d_fullscreen: d3d,
            grab,
            frame_dark: dark,
        }
    }

    #[test]
    fn desktop_name_classification() {
        assert_eq!(classify_desktop_name("Default"), DesktopKind::Default);
        assert_eq!(classify_desktop_name("default"), DesktopKind::Default);
        assert_eq!(classify_desktop_name("Winlogon"), DesktopKind::Secure);
        assert_eq!(classify_desktop_name("ScreenSaver"), DesktopKind::Secure);
        assert_eq!(classify_desktop_name(""), DesktopKind::Unknown);
    }

    #[test]
    fn frame_dark_detection() {
        let mut black = vec![0u8; 64 * 64 * 4];
        assert!(frame_is_dark(&black, 64 * 4, 64, 64));
        let white = vec![255u8; 64 * 64 * 4];
        assert!(!frame_is_dark(&white, 64 * 4, 64, 64));
        // 一半黑一半白：不算全黑
        for y in 0..64 {
            for x in 0..32 {
                let off = y * 64 * 4 + x * 4;
                black[off..off + 4].fill(255);
            }
        }
        assert!(!frame_is_dark(&black, 64 * 4, 64, 64));
        // 尺寸异常时不能 panic
        assert!(!frame_is_dark(&[], 0, 0, 0));
    }

    #[test]
    fn secure_desktop_needs_confirmation_and_recovers() {
        let mut t = HealthTracker::new();
        let first = obs(DesktopKind::Secure, GrabOutcome::Lost, false, false, false);
        assert_eq!(t.observe(&first), None);
        let change = t.observe(&first).expect("第二次观测应确认安全桌面");
        assert_eq!(change.to, CaptureState::SecureDesktop);
        let back = obs(
            DesktopKind::Default,
            GrabOutcome::NoChange,
            false,
            false,
            false,
        );
        let change = t.observe(&back).expect("回到 Default 应恢复");
        assert_eq!(change.to, CaptureState::Live);
    }

    #[test]
    fn protected_window_reported() {
        let mut t = HealthTracker::new();
        let o = obs(DesktopKind::Default, GrabOutcome::Frame, false, true, false);
        assert_eq!(t.observe(&o), None);
        let change = t.observe(&o).expect("连续两次受保护应上报");
        assert_eq!(change.to, CaptureState::ProtectedWindow);
        let clear = obs(
            DesktopKind::Default,
            GrabOutcome::Frame,
            false,
            false,
            false,
        );
        assert_eq!(t.observe(&clear).map(|c| c.to), Some(CaptureState::Live));
    }

    #[test]
    fn exclusive_fullscreen_from_lost_frames() {
        let mut t = HealthTracker::new();
        let o = obs(DesktopKind::Default, GrabOutcome::Lost, false, false, true);
        assert_eq!(t.observe(&o), None);
        let change = t.observe(&o).expect("全屏 D3D + 连续丢失应判定独占全屏");
        assert_eq!(change.to, CaptureState::ExclusiveFullscreen);
        let live = obs(DesktopKind::Default, GrabOutcome::Frame, false, false, true);
        let change = t.observe(&live).expect("恢复出帧应回到 Live");
        assert_eq!(change.to, CaptureState::Live);
    }

    #[test]
    fn retrying_after_repeated_lost() {
        let mut t = HealthTracker::new();
        let o = obs(DesktopKind::Default, GrabOutcome::Lost, false, false, false);
        t.observe(&o);
        t.observe(&o);
        let change = t.observe(&o).expect("连续丢失应进入重建状态");
        assert_eq!(change.to, CaptureState::Retrying);
    }

    #[test]
    fn blackout_requires_sustained_dark_frames() {
        let mut t = HealthTracker::new();
        let o = obs(DesktopKind::Default, GrabOutcome::Frame, true, false, false);
        for _ in 0..(BLACKOUT_DARK_TICKS - 1) {
            assert_eq!(t.observe(&o), None);
        }
        let change = t.observe(&o).expect("持续全黑应上报");
        assert_eq!(change.to, CaptureState::SuspectedBlackout);
        // 非全黑帧立即恢复正常，不粘滞
        let lit = obs(
            DesktopKind::Default,
            GrabOutcome::Frame,
            false,
            false,
            false,
        );
        assert_eq!(t.observe(&lit).map(|c| c.to), Some(CaptureState::Live));
    }

    /// Windows 探测在无交互桌面/无前台窗口时也必须安全返回，不能 panic。
    #[cfg(windows)]
    #[test]
    fn windows_probes_do_not_panic() {
        let desktop = probe_desktop();
        assert!(matches!(
            desktop.kind,
            DesktopKind::Default | DesktopKind::Secure | DesktopKind::Unknown
        ));
        let _ = probe_window_protected(0);
        let _ = probe_d3d_fullscreen();
    }
}
