//! 应用日志。
//!
//! release 构建是 `windows_subsystem = "windows"`，进程没有控制台，`eprintln!` 的输出
//! 进入虚无：用户报障时（连不上、转发失效、指纹超时…）开发侧拿不到任何现场。
//! 这里提供 `applog!`：与 `eprintln!` 同一调用形态，在写 stderr 的同时**落盘**到
//! `<config_dir>/logs/app.log`，超过阈值滚动为 `app.log.1`（只保留一份备份）。
//!
//! 约束：
//! - 日志永远不能影响主流程：文件打开/写入失败只是关掉句柄，绝不 panic；
//! - 单行截断，避免误把大 payload（终端数据等）整段写进日志；
//! - 单测构建（`cfg(test)`）只写 stderr，**不污染用户真实的 `%APPDATA%` 日志**；
//! - 会话日志（`session_log.rs`）记录的是终端字节流，与本模块的应用诊断日志互不影响。

use std::io::Write;

/// 单行最大字符数：超过即截断（防止把整个 payload 打进日志）。
const MAX_LINE_CHARS: usize = 4000;

/// 时间戳 `YYYY-MM-DD HH:MM:SS`。复用 `session_log` 的日历换算（UTC）；
/// 日志时间戳只用于排序与对照，不需要精确时区。
fn stamp() -> String {
    // 与 session_log 一致：Windows 取本地时间（GetLocalTime），其它平台退回 UTC。
    let t = crate::session_log::LogStamp::now();
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        t.year, t.month, t.day, t.hour, t.minute, t.second
    )
}

fn truncate(message: &str) -> String {
    if message.chars().count() <= MAX_LINE_CHARS {
        return message.to_string();
    }
    let mut s: String = message.chars().take(MAX_LINE_CHARS).collect();
    s.push_str(" …(truncated)");
    s
}

/// 单行日志契约——内嵌换行/回车必须转义。TOML 解析错误等多行消息会破坏
/// 「一行一条、便于 grep」的格式，外部字符串里的换行还能伪造带时间戳的假行。
fn escape_newlines(message: &str) -> String {
    message.replace('\n', "\\n").replace('\r', "\\r")
}

/// 格式化一条日志行（时间戳 + 换行转义 + 截断），便于单测。
fn format_line(message: &str) -> String {
    format!("[{}] {}\n", stamp(), truncate(&escape_newlines(message)))
}

/// 写一条应用日志（stderr + 日志文件）。由 [`applog!`] 宏调用。
pub fn write_line(message: &str) {
    let line = format_line(message);
    // 有控制台时保持原有调试可见性；没有控制台时 write_all 返回 Err，忽略即可
    // （不要用 eprint!：std 在写 stderr 失败时会 panic）。
    let _ = std::io::stderr().write_all(line.as_bytes());
    // 单测构建不写用户 %APPDATA% 日志（避免反复 `cargo test` 污染真实日志文件）。
    #[cfg(not(test))]
    file_sink::write(line.as_bytes());
}

#[cfg(not(test))]
mod file_sink {
    use std::fs::{File, OpenOptions};
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Mutex, OnceLock};

    /// 日志文件滚动阈值：超过就把当前文件改成 `app.log.1`。
    const MAX_LOG_BYTES: u64 = 2 * 1024 * 1024;

    struct SinkState {
        file: Option<File>,
        path: std::path::PathBuf,
    }

    static SINK: OnceLock<Mutex<Option<SinkState>>> = OnceLock::new();
    /// 当前文件已写字节数（含启动前已有内容）：写入路径用它判断是否需要滚动，
    /// 避免“只在进程首次打开时检查一次、长驻进程日志无限增长”。
    static WRITTEN: AtomicU64 = AtomicU64::new(0);

    fn log_path() -> Option<std::path::PathBuf> {
        let dir = crate::session_log::logs_dir().ok()?;
        std::fs::create_dir_all(&dir).ok()?;
        Some(dir.join("app.log"))
    }

    /// 打开日志文件；超过阈值先滚动成 `app.log.1`（只保留一份备份）。
    fn open_state() -> Option<SinkState> {
        let path = log_path()?;
        if std::fs::metadata(&path)
            .map(|m| m.len() > MAX_LOG_BYTES)
            .unwrap_or(false)
        {
            let _ = std::fs::rename(&path, path.with_file_name("app.log.1"));
        }
        let written = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        WRITTEN.store(written, Ordering::Relaxed);
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok()?;
        Some(SinkState {
            file: Some(file),
            path,
        })
    }

    /// 写一行：懒加载 / 写入失败关句柄 / 超阈值滚动。滚动失败只记录 stderr，不重试。
    pub fn write(line: &[u8]) {
        let mut guard = SINK
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if guard.is_none() {
            *guard = open_state();
        }
        let Some(state) = guard.as_mut() else {
            return;
        };
        let write_ok = match state.file.as_mut() {
            Some(file) => file.write_all(line).is_ok(),
            None => false,
        };
        if !write_ok {
            // 磁盘满 / 权限变化：关掉句柄，后续日志只走 stderr，不再逐条重试。
            *guard = None;
            return;
        }
        let total = WRITTEN.fetch_add(line.len() as u64, Ordering::Relaxed) + line.len() as u64;
        if total <= MAX_LOG_BYTES {
            return;
        }
        // 达到阈值：关闭当前文件 → 滚动 → 重新打开（失败则降级为仅 stderr）。
        let path = state.path.clone();
        *guard = None;
        let rotated = std::fs::rename(&path, path.with_file_name("app.log.1"));
        if rotated.is_err() {
            let _ = std::io::stderr().write_all(b"[applog] rotate app.log failed, log disabled\n");
        }
        *guard = open_state();
    }
}

/// 应用日志宏：与 `eprintln!` 同一调用形态，额外落盘到 `<config_dir>/logs/app.log`。
///
/// 在 `lib.rs` 顶部以 `#[macro_use] pub mod applog;` 引入，全 crate 可用。
macro_rules! applog {
    ($($arg:tt)*) => {
        $crate::applog::write_line(&format!($($arg)*))
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 日志行固定形态：`[YYYY-MM-DD HH:MM:SS] <message>\n`，单行、带时间戳。
    #[test]
    fn format_line_has_timestamp_and_single_newline() {
        let line = format_line("连接 1.2.3.4:22 失败");
        assert!(line.starts_with('['), "{line}");
        assert_eq!(line.matches("] ").count(), 1, "{line}");
        assert!(line.ends_with('\n'));
        assert_eq!(line.matches('\n').count(), 1, "必须是单行日志");
        assert!(line.contains("连接 1.2.3.4:22 失败"), "{line}");
        // 时间戳形如 2026-11-15 09:00:00（19 字符）
        let ts = &line[1..20];
        assert_eq!(ts.len(), 19, "时间戳格式: {ts}");
        assert_eq!(ts.as_bytes()[4], b'-', "{ts}");
        assert_eq!(ts.as_bytes()[10], b' ', "{ts}");
        assert_eq!(ts.as_bytes()[13], b':', "{ts}");
    }

    /// 超长消息必须截断（避免终端数据/大 payload 整段写进日志），且仍是单行。
    #[test]
    fn format_line_truncates_long_messages() {
        let long = "x".repeat(MAX_LINE_CHARS + 500);
        let line = format_line(&long);
        assert!(line.ends_with(" …(truncated)\n"), "应标记截断");
        // 上限 = 时间戳前缀（[ + 19 + "] " ≈ 22）+ MAX_LINE_CHARS + " …(truncated)"（13）+ 换行
        assert!(
            line.chars().count() <= MAX_LINE_CHARS + 40,
            "截断后长度异常: {}",
            line.chars().count()
        );
        assert_eq!(line.matches('\n').count(), 1);
    }

    /// 多行消息（TOML 解析错误自带源码行/插入符）必须转义成单行，
    /// 不得产生伪造的“带时间戳”行。
    #[test]
    fn format_line_escapes_embedded_newlines() {
        let msg = "TOML parse error:\nline 2\n  |\n2 | x =\n  | ^";
        let line = format_line(msg);
        assert_eq!(line.matches('\n').count(), 1, "必须是单行: {line}");
        assert!(
            line.contains("TOML parse error:\\nline 2"),
            "换行应转义: {line}"
        );
        assert!(!line.contains("\nline 2\n"), "不得出现真实换行: {line}");
    }

    /// 单测构建下 write_line 不落盘（只写 stderr），不得 panic。
    #[test]
    fn write_line_does_not_touch_user_log_in_tests() {
        write_line("applog self-test");
    }
}
