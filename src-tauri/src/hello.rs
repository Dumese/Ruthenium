//! 启动时的 Windows Hello 系统级身份验证（Windows.Security.Credentials.UI）。
//!
//! 目的：在用户开启「启动时验证」后，应用在加载配置与会话**之前**要求一次
//! 系统身份验证（PIN / 指纹 / 人脸）。这里只做「是否是本人在操作」的存在性校验，
//! 不提供任何加密——主机配置本身仍是明文 `hosts.toml`，本功能挡的是「别人
//! 打开你正在运行的这台电脑就能直接看主机列表」，不是磁盘取证。
//!
//! 为什么用 `UserConsentVerifier` 而不是 Biometric Framework：
//! - 它走 Windows Hello 的官方 UI（与系统设置里配置的 PIN/生物识别共用凭据），
//!   不需要应用自己管理任何密钥材料；
//! - 返回码天然覆盖本功能要处理的全部分支：取消、重试耗尽、未配置、无设备、
//!   组策略禁用、设备忙——每一条都能给出明确提示，而不是一个笼统的失败。
//!
//! 线程模型：WinRT 异步操作用阻塞 `.get()` 等待，调用方必须是专门的阻塞线程
//! （见 `lib.rs` 的 `spawn_blocking`），不能占用 UI 线程与 tokio worker；
//! 首次使用时对线程做 MTA 初始化（重复调用返回错误一律忽略——同线程已是
//! 其它套间时 `RoInitialize` 会失败，但 WinRT 仍可正常使用）。

use serde::Serialize;

/// Windows Hello 的可用性（用于设置页提示与启动门禁的兜底分支）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum HelloAvailability {
    /// 已配置 PIN 或生物识别，可以发起验证。
    Available,
    /// 用户尚未在系统设置里配置 Windows Hello。
    NotConfigured,
    /// 当前设备没有可用的验证器（无 TPM / 无摄像头 / 无指纹）。
    NoDevice,
    /// 被组策略禁用。
    Policy,
    /// 验证器被其他程序占用。
    Busy,
    /// 非 Windows 平台或 WinRT 调用异常。
    Unsupported,
}

/// 一次验证的结果。前端按它决定提示与备用流程：
/// - `verified` → 解锁；
/// - `canceled` / `retriesExhausted` → 保持锁定，提示重试或退出；
/// - `notConfigured` / `noDevice` / `policy` / `busy` / `unsupported` / `error`
///   → 明确提示「当前环境不可用」，提供「退出」与需二次确认的「跳过本次」。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum HelloOutcome {
    Verified,
    Canceled,
    RetriesExhausted,
    NotConfigured,
    NoDevice,
    Policy,
    Busy,
    Unsupported,
    Error,
}

/// 启动门禁需要的两个事实（前端在挂载主界面之前查询）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HelloRequirement {
    /// 配置里是否开启了「启动时验证」。
    pub required: bool,
    /// 当前设备的可用性（无论是否开启都返回，供设置页提示）。
    pub availability: HelloAvailability,
}

fn availability_to_outcome(a: HelloAvailability) -> HelloOutcome {
    match a {
        HelloAvailability::Available => HelloOutcome::Verified, // 不会用到：可用即继续弹框
        HelloAvailability::NotConfigured => HelloOutcome::NotConfigured,
        HelloAvailability::NoDevice => HelloOutcome::NoDevice,
        HelloAvailability::Policy => HelloOutcome::Policy,
        HelloAvailability::Busy => HelloOutcome::Busy,
        HelloAvailability::Unsupported => HelloOutcome::Unsupported,
    }
}

/// 查询当前设备的 Windows Hello 可用性。
pub fn availability() -> HelloAvailability {
    imp::availability()
}

/// 发起一次 Windows Hello 验证并等待结果（阻塞调用线程，见模块注释）。
pub fn verify(message: &str) -> HelloOutcome {
    // 先查可用性：不可用时直接给出具体原因，避免弹一个注定失败的系统框，
    // 也让前端能走「环境不支持」的提示分支而不是看起来像用户取消。
    let avail = availability();
    if avail != HelloAvailability::Available {
        return availability_to_outcome(avail);
    }
    imp::verify(message)
}

/// 启动门禁查询：配置里是否开启验证 + 当前设备的可用性。
///
/// 故意用 `read_config_lenient`（只读、无副作用）：调用发生在正常启动流程之前，
/// 配置缺失或损坏时按「未开启」处理并让应用继续启动——门禁本身不能成为
/// 一张把自己锁在门外的锁（配置损坏的兜底流程已有 `configError` 提示）。
#[tauri::command]
pub async fn hello_requirement() -> HelloRequirement {
    let required = tauri::async_runtime::spawn_blocking(|| {
        crate::config::read_config_lenient()
            .map(|c| c.preferences.require_windows_hello)
            .unwrap_or(false)
    })
    .await
    .unwrap_or(false);
    let availability = tauri::async_runtime::spawn_blocking(availability)
        .await
        .unwrap_or(HelloAvailability::Unsupported);
    HelloRequirement {
        required,
        availability,
    }
}

/// 发起一次验证。阻塞 `.get()` 放在独立线程，不占用 UI 线程与 tokio worker。
///
/// `reason` 由前端按当前语言传入（显示在系统验证框里）；为空时用英文兜底。
#[tauri::command]
pub async fn verify_windows_hello(reason: Option<String>) -> HelloOutcome {
    let message = reason
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| "Verify your identity to open Ruthenium".to_string());
    tauri::async_runtime::spawn_blocking(move || verify(&message))
        .await
        .unwrap_or(HelloOutcome::Error)
}

#[cfg(target_os = "windows")]
mod imp {
    use super::HelloAvailability;
    use super::HelloOutcome;
    use std::time::{Duration, Instant};
    use windows::core::HSTRING;
    use windows::Security::Credentials::UI::{
        UserConsentVerificationResult, UserConsentVerifier, UserConsentVerifierAvailability,
    };
    use windows::Win32::Foundation::E_ABORT;
    use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};
    use windows_future::{AsyncStatus, IAsyncOperation};

    /// Hello 系统框最长等待：必须短于 SSH keepalive（默认 30s×3=90s）与屏幕
    /// 握手（120s）的时限，否则用户慢慢验证时连接会先被 keepalive 断开，
    /// 报出的原因还变成“网络错误”。
    const HELLO_TIMEOUT: Duration = Duration::from_secs(45);
    /// 可用性检查也加个上限（正常是毫秒级，卡住说明 WinRT 环境异常）。
    const AVAILABILITY_TIMEOUT: Duration = Duration::from_secs(10);

    /// 等待 WinRT 异步操作，超时主动 Cancel 并返回 `E_ABORT`。
    fn wait_op<T: windows::core::RuntimeType + 'static>(
        op: &IAsyncOperation<T>,
        timeout: Duration,
    ) -> windows::core::Result<T> {
        let start = Instant::now();
        loop {
            let status = op.Status()?;
            if status == AsyncStatus::Completed {
                return op.GetResults();
            }
            if status == AsyncStatus::Started {
                if start.elapsed() >= timeout {
                    let _ = op.Cancel();
                    return Err(windows::core::Error::from(E_ABORT));
                }
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            // Canceled / Error：GetResults 会返回对应错误
            return op.GetResults();
        }
    }

    /// 对当前线程做一次 MTA 初始化。返回 false 表示 WinRT 完全不可用。
    ///
    /// 重复调用（或线程已经是别的套间）会返回错误，这不是失败：只要 WinRT 能
    /// 正常工作就行。真正的可用性判断交给 `CheckAvailabilityAsync`。
    fn ensure_apartment() -> bool {
        unsafe { RoInitialize(RO_INIT_MULTITHREADED).is_ok() }
    }

    pub fn availability() -> HelloAvailability {
        // 已初始化过其它套间时这里会失败，但 WinRT 调用仍可能成功——忽略返回值，
        // 以 CheckAvailabilityAsync 的真实结果为准。
        let _ = ensure_apartment();
        let result = UserConsentVerifier::CheckAvailabilityAsync()
            .and_then(|op| wait_op(&op, AVAILABILITY_TIMEOUT));
        match result {
            Ok(UserConsentVerifierAvailability::Available) => HelloAvailability::Available,
            Ok(UserConsentVerifierAvailability::NotConfiguredForUser) => {
                HelloAvailability::NotConfigured
            }
            Ok(UserConsentVerifierAvailability::DeviceNotPresent) => HelloAvailability::NoDevice,
            Ok(UserConsentVerifierAvailability::DisabledByPolicy) => HelloAvailability::Policy,
            Ok(UserConsentVerifierAvailability::DeviceBusy) => HelloAvailability::Busy,
            // 枚举新增值或接口异常/超时：保守地按「不支持」处理，交给前端兜底流程。
            _ => HelloAvailability::Unsupported,
        }
    }

    pub fn verify(message: &str) -> HelloOutcome {
        let _ = ensure_apartment();
        let text = HSTRING::from(message);
        let op = match UserConsentVerifier::RequestVerificationAsync(&text) {
            Ok(op) => op,
            Err(_) => return HelloOutcome::Error,
        };
        let result = match wait_op(&op, HELLO_TIMEOUT) {
            Ok(v) => v,
            Err(e) if e.code() == E_ABORT => {
                applog!(
                    "[hello] Windows Hello 验证超时（{}s），已取消并放行连接超时处理",
                    HELLO_TIMEOUT.as_secs()
                );
                return HelloOutcome::Canceled;
            }
            Err(_) => return HelloOutcome::Error,
        };
        match result {
            UserConsentVerificationResult::Verified => HelloOutcome::Verified,
            UserConsentVerificationResult::Canceled => HelloOutcome::Canceled,
            UserConsentVerificationResult::RetriesExhausted => HelloOutcome::RetriesExhausted,
            UserConsentVerificationResult::DeviceNotPresent => HelloOutcome::NoDevice,
            UserConsentVerificationResult::NotConfiguredForUser => HelloOutcome::NotConfigured,
            UserConsentVerificationResult::DisabledByPolicy => HelloOutcome::Policy,
            UserConsentVerificationResult::DeviceBusy => HelloOutcome::Busy,
            _ => HelloOutcome::Error,
        }
    }
}

#[cfg(not(target_os = "windows"))]
mod imp {
    use super::HelloAvailability;
    use super::HelloOutcome;

    pub fn availability() -> HelloAvailability {
        HelloAvailability::Unsupported
    }

    pub fn verify(_message: &str) -> HelloOutcome {
        HelloOutcome::Unsupported
    }
}
