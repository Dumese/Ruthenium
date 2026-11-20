#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // 图形加速：WebView2 只在**创建时**读取这些开关，所以必须在 run() 之前注入，
    // 且读取配置要在启动早期完成（配置损坏时按默认 system 处理，不影响启动）。
    apply_gpu_preference();
    ruthenium_lib::run()
}

/// 读取 preferences.gpuMode；为 off 时给 WebView2 追加禁用硬件加速的参数。
/// `system`（默认）不注入任何参数，交给 WebView2 自己判断可用性并自动回退。
fn apply_gpu_preference() {
    // 只读加载（无写副作用）：此时尚未走到单实例插件与 AppState 初始化，
    // 不能用 load_config_from_disk——文件缺失时它会先写默认配置到盘上。
    // 用户显式关闭，或本机是低配设备（≤2 核）→ 都注入禁用硬件加速的参数。
    // 配置读取失败时也不能跳过低配判断（那种情况按 system 处理，但低配仍要降级）。
    let pref_off = ruthenium_lib::config::read_config_lenient()
        .map(|c| matches!(c.preferences.gpu_mode, ruthenium_lib::models::GpuMode::Off))
        .unwrap_or(false);
    let low_end = ruthenium_lib::perf::is_low_end_device();
    if !pref_off && !low_end {
        return;
    }
    if low_end && !pref_off {
        // 此时 stderr 在 release（windows 子系统）下不可见，统一走 applog 落盘。
        ruthenium_lib::applog::write_line(
            "[gpu] 低配设备（≤2 核）：自动关闭 WebView2 硬件加速以节省内存",
        );
    }
    let disable = "--disable-gpu --disable-gpu-compositing --disable-gpu-rasterization";
    match std::env::var("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS") {
        Ok(existing) if !existing.trim().is_empty() => {
            std::env::set_var(
                "WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS",
                format!("{existing} {disable}"),
            );
        }
        _ => std::env::set_var("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS", disable),
    }
}
