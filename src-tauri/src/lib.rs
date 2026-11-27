/// 应用日志：`#[macro_use]` 必须放在其它模块之前，`applog!` 才对全 crate 可见。
/// release（windows 子系统）没有控制台，诊断信息只有落到 `logs/app.log` 才可回收。
#[macro_use]
pub mod applog;
pub mod commands;
pub mod config;
pub mod encoding;
pub mod error;
pub mod events;
pub mod hello;
pub mod local;
pub mod metrics;
pub mod models;
pub mod peer;
pub mod perf;
pub mod secret;
pub mod session_log;
pub mod sftp;
pub mod snap;
pub mod ssh;
pub mod state;
pub mod tunnel;

use tauri::Manager;

use std::sync::OnceLock;

use state::AppState;

/// 启动耗时诊断用（[perf] 日志）。
static START: OnceLock<std::time::Instant> = OnceLock::new();

/// 默认窗口逻辑尺寸（与 tauri.conf.json 的 1280×800 一致）。
const WINDOW_DEFAULT_WIDTH: f64 = 1280.0;
const WINDOW_DEFAULT_HEIGHT: f64 = 800.0;
/// 最小窗口逻辑尺寸（与 tauri.conf.json 的 560×400 一致；可用区不足时会被钳制）。
///
/// 为什么不是更「安全」的 900：Windows 11 只有在这个最小尺寸放得进目标分区时才真的
/// 贴靠（浮层照弹、窗口不动 = 看起来半残），而分区尺寸是**可用区的 1/n**，所以最小值
/// 越大约束越多。按常见的三种屏幕算（可用区扣掉任务栏）：
///
/// - 2560 屏 @150% → 1706×986：1/3、1/2、2/3、2×2 全放得下；
/// - 1920 屏 @100% → 1920×1040：1/3、1/2、2×2 全放得下；
/// - 1920 屏 @125% → 1536×832：1/2 与 2×2 需要 ≤416 高，1/3 需要 ≤512 宽。
///
/// 因此宽 560（保证 1/3 在 1680+ 逻辑宽上有余量）、高 400（保证 125% 笔记本的上下
/// 半分与 2×2 放得下）。560×400 是可用的下限：SFTP 双栏各约 148px、标签条退化为
/// 内部横向滚动，页面本身没有横向溢出（scripts/find-narrow.mjs 覆盖到这个尺寸）。
/// 1/3 分区在 1536 逻辑宽（1920@125%）上仍放不下——那需要 ~500 宽，本应用会挤坏，
/// 属于已知取舍。
const WINDOW_MIN_WIDTH: f64 = 560.0;
const WINDOW_MIN_HEIGHT: f64 = 400.0;
/// 高 DPI 下可用区兜底下限，避免预留比例把窗口压得过小。
const WINDOW_AVAIL_MIN_WIDTH: f64 = 640.0;
const WINDOW_AVAIL_MIN_HEIGHT: f64 = 420.0;
/// 为任务栏与边框预留的显示器逻辑可用区比例。
const WINDOW_AVAIL_WIDTH_RATIO: f64 = 0.92;
const WINDOW_AVAIL_HEIGHT_RATIO: f64 = 0.88;

/// 启动 Tauri 应用：注册插件与命令、初始化 AppState，并在首帧后同步 WebView2 视口。
pub fn run() {
    let _ = START.set(std::time::Instant::now());
    // 测试/开发：RUTHENIUM_DATA_DIR 存在时允许第二个实例（用于双实例端到端验证）。
    // 普通用户不会设置该变量。
    let multi_instance = std::env::var_os("RUTHENIUM_DATA_DIR").is_some();
    let mut builder = tauri::Builder::default();
    if !multi_instance {
        builder = builder.plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            // 二次启动：把已运行实例的主窗口带回前台。
            // 最小化时先还原，再显示 + 抢焦点；窗口不存在（异常状态）时静默忽略。
            if let Some(win) = app.get_webview_window("main") {
                let _ = win.unminimize();
                let _ = win.show();
                let _ = win.set_focus();
            }
        }));
    }
    builder
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        // 只恢复尺寸/位置/最大化/全屏，**不恢复 VISIBLE**：
        // 该插件默认会把上次的「可见」状态写回窗口，导致 visible:false 失效 →
        // 启动瞬间先出现一块白底窗口（WebView2 还没画首帧），随后再「闪」成正常界面。
        .plugin(
            tauri_plugin_window_state::Builder::default()
                .with_state_flags(
                    tauri_plugin_window_state::StateFlags::SIZE
                        | tauri_plugin_window_state::StateFlags::POSITION
                        | tauri_plugin_window_state::StateFlags::MAXIMIZED
                        | tauri_plugin_window_state::StateFlags::FULLSCREEN,
                )
                .build(),
        )
        // 启动观感：窗口初始隐藏（tauri.conf visible:false），**由前端在「字体就绪 + 首帧
        // 已绘制」后调用 ui_ready 再显示**（见 src/main.tsx）。这样用户第一眼看到的就是
        // 字形正确的完整界面：内嵌全量 CJK 字体约 13MB，若在字体解析完成前显示窗口，
        // 会先看到一块没有文字的空白界面再突然出字。
        // 这里只记录耗时，不在 page_load 时直接 show（会和字体等待冲突）；
        // 前端异常没调 ui_ready 时，setup 里的 1.5s 兜底线程仍会显示窗口。
        .on_page_load(|_webview, payload| {
            #[cfg(debug_assertions)]
            applog!(
                "[perf] on_page_load {:?} {}ms",
                payload.event(),
                START.get().map(|s| s.elapsed().as_millis()).unwrap_or(0)
            );
            let _ = payload;
        })
        .setup(|app| {
            #[cfg(debug_assertions)]
            applog!(
                "[perf] setup start {}ms",
                START.get().map(|s| s.elapsed().as_millis()).unwrap_or(0)
            );
            let state = AppState::new(app.handle().clone())
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error>)?;
            app.manage(state);
            // 局域网对等层：常开开关打开时随应用启动监听（默认关 → 不做任何事）
            {
                let handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    let st = handle.state::<AppState>();
                    let prefs = st.config.read().await.preferences.clone();
                    if prefs.lan_visible {
                        let cfg = crate::peer::PeerConfig::from_prefs(&prefs);
                        if let Err(e) = st.peer.start(&cfg, None).await {
                            applog!("[peer] 启动时开启监听失败: {e}");
                        }
                    }
                });
            }
            // WebView2 视口同步兜底：窗口创建在高 DPI 显示器上时，首次 bounds
            // 可能未按显示器缩放同步（表现为内容裁切/底部留白）。启动后一次性
            // 以当前物理尺寸重设窗口，触发 controller bounds 重新计算。
            if let Some(win) = app.get_webview_window("main") {
                // 分辨率 / 缩放适配：200% 缩放 + 1080p 时逻辑分辨率仅约 960×540，
                // 配置里的 1280×800 默认尺寸与 560×400 最小尺寸都会超出屏幕。
                // 这里按当前显示器的逻辑可用区钳制窗口尺寸与最小值。
                if let Ok(Some(mon)) = win.current_monitor().or_else(|_| win.primary_monitor()) {
                    let sf = mon.scale_factor();
                    let msize = mon.size();
                    // 预留任务栏与边框：宽 92%、高 88%
                    let avail_w = ((msize.width as f64 / sf) * WINDOW_AVAIL_WIDTH_RATIO)
                        .max(WINDOW_AVAIL_MIN_WIDTH);
                    let avail_h = ((msize.height as f64 / sf) * WINDOW_AVAIL_HEIGHT_RATIO)
                        .max(WINDOW_AVAIL_MIN_HEIGHT);
                    let min_w = WINDOW_MIN_WIDTH.min(avail_w);
                    let min_h = WINDOW_MIN_HEIGHT.min(avail_h);
                    let _ = win.set_min_size(Some(tauri::Size::Logical(tauri::LogicalSize::new(
                        min_w, min_h,
                    ))));
                    if let Ok(cur) = win.inner_size() {
                        let cw = cur.width as f64 / sf;
                        let ch = cur.height as f64 / sf;
                        if cw > avail_w || ch > avail_h {
                            let want_w = WINDOW_DEFAULT_WIDTH.min(avail_w);
                            let want_h = WINDOW_DEFAULT_HEIGHT.min(avail_h);
                            let _ = win.set_size(tauri::LogicalSize::new(want_w, want_h));
                            let _ = win.center();
                        }
                    }
                }
                // 兜底：前端异常没调用 ui_ready 时，1.5s 后强制显示窗口，
                // 避免用户面对一个永远不出现的进程（正常路径见 ui_ready）。
                let fallback = win.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(1500));
                    if !fallback.is_visible().unwrap_or(true) {
                        let _ = fallback.show();
                    }
                });
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            // 配置
            commands::config::load_config,
            commands::config::save_preferences,
            // 安全（启动门禁）
            hello::hello_requirement,
            hello::verify_windows_hello,
            // 局域网对等层（可见性/监听/设备表）
            commands::peer::peer_status,
            commands::peer::peer_set_visible,
            commands::peer::peer_open_window,
            commands::peer::peer_close_window,
            commands::peer::peer_list,
            commands::peer::peer_discover,
            commands::peer::peer_pair_start,
            commands::peer::peer_pair_confirm,
            commands::peer::peer_pair_cancel,
            commands::peer::peer_remove,
            commands::peer::peer_update_grants,
            commands::peer::peer_refresh_grants,
            commands::peer::peer_open_sftp,
            commands::peer::peer_send_config,
            commands::peer::peer_inbox,
            commands::peer::peer_import_inbox,
            commands::peer::peer_delete_inbox,
            commands::peer::peer_open_terminal,
            commands::peer::peer_control_consent,
            commands::peer::peer_control_sessions,
            commands::peer::peer_kick_session,
            commands::peer::peer_open_screen,
            commands::peer::peer_screen_command,
            commands::peer::peer_close_screen,
            commands::peer::peer_screen_consent,
            commands::peer::peer_screen_sessions,
            commands::peer::peer_screen_kick,
            commands::peer::peer_encoder_diagnostics,
            commands::peer::peer_input_start,
            commands::peer::peer_input_send,
            commands::peer::peer_input_stop,
            commands::peer::peer_audio_start,
            commands::peer::peer_audio_stop,
            commands::peer::peer_audio_set_mic,
            commands::peer::peer_mic_consent,
            commands::peer::peer_mic_stop,
            commands::peer::peer_mic_sessions,
            commands::peer::peer_clipboard_start,
            commands::peer::peer_clipboard_get,
            commands::peer::peer_clipboard_set,
            commands::peer::peer_clipboard_get_rich,
            commands::peer::peer_clipboard_set_rich,
            commands::peer::peer_clipboard_stop,
            commands::peer::peer_clipboard_local_get,
            commands::peer::peer_clipboard_local_set,
            commands::peer::peer_clipboard_local_get_rich,
            commands::peer::peer_clipboard_local_set_rich,
            commands::peer::peer_audit_recent,
            commands::peer::peer_audit_clear,
            commands::config::import_ssh_config,
            commands::config::import_ssh_config_default,
            commands::config::commit_import,
            commands::config::export_backup,
            commands::config::import_backup,
            // 原生文件对话框（父窗口 = 主窗口，保证弹在前台；失败可回传错误）
            commands::config::pick_backup_save_path,
            commands::config::pick_backup_open_path,
            commands::config::pick_folder,
            // 主机
            commands::hosts::list_hosts,
            commands::hosts::upsert_host,
            commands::hosts::delete_host,
            // 分组
            commands::config::upsert_folder,
            commands::config::rename_folder,
            commands::config::delete_folder,
            // 秘密
            commands::secret::set_password,
            commands::secret::has_password,
            commands::secret::delete_password,
            // 会话
            commands::session::connect,
            commands::session::connect_transient,
            commands::session::connect_local,
            // 关闭 connecting 标签时中断在途握手/认证
            commands::session::cancel_connect,
            commands::session::write_terminal,
            commands::session::resize_terminal,
            commands::session::close_session,
            commands::session::reconnect,
            commands::session::list_sessions,
            commands::session::set_session_encoding,
            // 指标
            metrics::start_metrics,
            metrics::stop_metrics,
            metrics::sample_metrics,
            // 指纹
            commands::hostkey::trust_host_key,
            commands::hostkey::reject_host_key,
            // 本地 FS
            commands::sftp::local_list,
            commands::sftp::local_mkdir,
            commands::sftp::local_delete,
            commands::sftp::local_rename,
            // SFTP
            commands::sftp::sftp_home,
            commands::sftp::sftp_list,
            commands::sftp::sftp_upload,
            commands::sftp::sftp_download,
            commands::sftp::sftp_mkdir,
            commands::sftp::sftp_delete,
            commands::sftp::sftp_rename,
            commands::sftp::sftp_cancel,
            // 片段
            commands::hosts::list_snippets,
            commands::hosts::upsert_snippet,
            commands::hosts::delete_snippet,
            // 转发
            commands::tunnel::list_tunnels,
            commands::tunnel::upsert_tunnel,
            commands::tunnel::delete_tunnel,
            commands::tunnel::toggle_tunnel,
            // 系统
            commands::settings::get_app_info,
            commands::settings::ui_ready,
            // Windows 11 贴靠布局：最大化按钮上的原生覆盖层（其它平台空实现）
            snap::snap_overlay_rect,
            // 会话日志清理（明文日志的一键销毁入口）
            commands::settings::clear_session_logs,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
