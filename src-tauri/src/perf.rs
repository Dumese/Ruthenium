//! 设备性能分档：给「低配自动降级」提供单一事实来源。
//!
//! Windows 10 官方最低配是 2 核 / 2GB 内存。这类机器上 WebView2 的硬件加速进程与
//! 每标签 WebGL 上下文开销过大（基线 376MB、每标签 +28MB，而 DOM 渲染器只有
//! 141MB、+2.7MB），所以在**创建 WebView2 之前**就要决定关掉硬件加速。
//!
//! 判定：逻辑核数 ≤ 2 视为低配；可用环境变量 RUTHENIUM_LOW_END=1/0 强制覆盖
//! （便于在高端机上走低配路径）。

/// 是否低配设备（可用于自动关闭硬件加速、缩小回滚缓冲）。
pub fn is_low_end_device() -> bool {
    if let Ok(v) = std::env::var("RUTHENIUM_LOW_END") {
        let v = v.trim().to_ascii_lowercase();
        if v == "1" || v == "true" {
            return true;
        }
        if v == "0" || v == "false" {
            return false;
        }
    }
    std::thread::available_parallelism()
        .map(|n| n.get() <= 2)
        .unwrap_or(false)
}
