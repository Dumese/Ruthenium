//! 系统信息命令。
use crate::config;
use crate::error::{AppError, AppResult};
use crate::models::AppInfo;
use crate::state::AppState;

/// 返回应用版本、配置/日志目录与启动时的配置错误详情。
#[tauri::command]
pub async fn get_app_info(state: tauri::State<'_, AppState>) -> AppResult<AppInfo> {
    let version = state
        .app
        .config()
        .version
        .clone()
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());
    Ok(AppInfo {
        version,
        config_dir: config::config_dir()?.to_string_lossy().to_string(),
        // 日志目录由后端给出，前端不再自己拼 `config_dir + "\logs"`。
        logs_dir: crate::session_log::logs_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default(),
        os: std::env::consts::OS.to_string(),
        config_error: state.config_error.clone(),
        low_end: crate::perf::is_low_end_device(),
    })
}

/// 清空会话日志（只删 `*.log` 会话文件；应用诊断日志 `app.log*` 保留）。
/// 返回删除的文件数，供前端提示「已清空 N 个」。
#[tauri::command]
pub async fn clear_session_logs() -> AppResult<usize> {
    let dir = crate::session_log::logs_dir()
        .map_err(|e| AppError::EConfigIo(format!("无法确定日志目录: {e}")))?;
    crate::session_log::clear_logs(&dir)
        .map_err(|e| AppError::EConfigIo(format!("清空会话日志失败: {e}")))
}

/// 前端首帧渲染完成后调用：此时才显示窗口，用户看到的就是已经画好的界面，
/// 避免「窗口先出现、内容后补上」的空白闪烁（启动观感优化）。
#[tauri::command]
pub async fn ui_ready(app: tauri::AppHandle) -> AppResult<()> {
    use tauri::Manager;
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.show();
        let _ = win.set_focus();
    }
    Ok(())
}
