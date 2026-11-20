//! 会话日志：把会话的**远端原始字节流** tee 到本地文件。
//!
//! 语义：
//! - 记录**解码前**的原始字节（ANSI 序列原样保留），不做编码转换、不做行处理；
//! - 文件：`<config_dir>\logs\<host名或id>-<yyyyMMdd-HHmmss>.log`，目录按需创建；
//! - 落盘：`BufWriter` + 每 [`FLUSH_INTERVAL`] 或累计 [`FLUSH_THRESHOLD_BYTES`] flush 一次；
//!   会话关闭 / 断线时 flush 并关闭（reader task 结束时 drop 本对象即可）；
//! - 写入失败**不打断会话**：记一次 stderr 后关闭该日志句柄。
//! - 不做留存 / 清理 / 检索（用户手动管理文件），也不进备份（备份不含会话内容）。
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// 周期性 flush 间隔：避免每来一批字节就一次 syscall（2s）。
pub const FLUSH_INTERVAL: Duration = Duration::from_secs(2);
/// 累计未 flush 字节数达到该阈值时立即 flush（也用作 BufWriter 容量，64 KiB）。
pub const FLUSH_THRESHOLD_BYTES: usize = 64 * 1024;
/// 文件名里主机名部分的最大字符数，避免超出 Windows 路径长度。
const MAX_NAME_CHARS: usize = 60;

/// 日志文件名里的本地时间戳（`yyyyMMdd-HHmmss`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogStamp {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

impl LogStamp {
    /// 当前**本地**时间。Windows 上直接取系统本地时间（不做时区换算）；
    /// 其他平台按 UTC 近似（本应用只发布 Windows）。
    pub fn now() -> Self {
        #[cfg(windows)]
        {
            let mut raw = WinSystemTime::default();
            // SAFETY: GetLocalTime 只写入调用方提供的 SYSTEMTIME，不涉及所有权。
            unsafe { GetLocalTime(&mut raw) };
            if raw.year > 0 {
                return Self {
                    year: raw.year as i32,
                    month: raw.month as u32,
                    day: raw.day as u32,
                    hour: raw.hour as u32,
                    minute: raw.minute as u32,
                    second: raw.second as u32,
                };
            }
        }
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        Self::from_unix(secs)
    }

    /// 由 Unix 秒构造（UTC）。供非 Windows 回退与单测使用。
    pub fn from_unix(secs: i64) -> Self {
        let days = secs.div_euclid(86_400);
        let secs_of_day = secs.rem_euclid(86_400);
        let (year, month, day) = civil_from_days(days);
        Self {
            year,
            month,
            day,
            hour: (secs_of_day / 3_600) as u32,
            minute: ((secs_of_day % 3_600) / 60) as u32,
            second: (secs_of_day % 60) as u32,
        }
    }

    /// `<名称>-YYYYMMDD-HHMMSS`（不含 `.log`）。
    pub fn file_stem(&self, host_name: &str) -> String {
        format!(
            "{}-{:04}{:02}{:02}-{:02}{:02}{:02}",
            sanitize_host_name(host_name),
            self.year,
            self.month,
            self.day,
            self.hour,
            self.minute,
            self.second
        )
    }

    /// `<名称>-YYYYMMDD-HHMMSS.log`。
    pub fn file_name(&self, host_name: &str) -> String {
        format!("{}.log", self.file_stem(host_name))
    }
}

#[cfg(windows)]
#[repr(C)]
#[derive(Default)]
struct WinSystemTime {
    year: u16,
    month: u16,
    day_of_week: u16,
    day: u16,
    hour: u16,
    minute: u16,
    second: u16,
    milliseconds: u16,
}

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn GetLocalTime(lp_system_time: *mut WinSystemTime);
}

/// Howard Hinnant 的 civil_from_days：Unix 天数 → (year, month, day)。
fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = y + if m <= 2 { 1 } else { 0 };
    (year as i32, m as u32, d as u32)
}

/// 主机名 → 文件名片段：替换 Windows 非法字符与控制字符，空名回退 `session`。
fn sanitize_host_name(name: &str) -> String {
    let replaced: String = name
        .trim()
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .take(MAX_NAME_CHARS)
        .collect();
    let cleaned = replaced.trim().trim_end_matches(&[' ', '.'][..]).trim();
    if cleaned.is_empty() {
        "session".to_string()
    } else {
        cleaned.to_string()
    }
}

/// 会话日志目录：`%LOCALAPPDATA%\Ruthenium\logs`。
///
/// 明文终端日志不应进 Roaming（域漫游/文件夹重定向会把它同步到网络共享或服务器）；
/// 配置 hosts.toml 仍留在 Roaming（便于漫游），只把日志迁到 Local。
/// 首次调用时把旧 Roaming logs 目录里的文件**迁移**过来（移动后移除旧目录）。
pub fn logs_dir() -> io::Result<PathBuf> {
    let base =
        dirs::data_local_dir().ok_or_else(|| io::Error::other("无法确定本机 Local 数据目录"))?;
    let dir = base.join("Ruthenium").join("logs");
    // 单测构建不迁移用户真实日志目录（避免 cargo test 搬动开发机数据）
    #[cfg(not(test))]
    {
        if let Ok(old) = crate::config::config_dir() {
            migrate_logs_from(&old.join("logs"), &dir);
        }
    }
    Ok(dir)
}

/// 迁移：旧目录存在而新目录尚无任何文件时，把旧目录的全部文件移动过去；
/// 新目录已有文件（迁移已完成/用户已使用）则跳过。
/// 迁移是 best-effort：单个文件移动失败只记日志（旧文件保留，用户可手动搬）。
fn migrate_logs_from(old_dir: &Path, new_dir: &Path) {
    if old_dir == new_dir {
        return;
    }
    let Ok(entries) = fs::read_dir(old_dir) else {
        return; // 旧目录不存在：无需迁移
    };
    let files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    if files.is_empty() {
        return;
    }
    // 新目录已有日志（迁移已发生过 / 用户已在新位置写入）：不重复迁移
    if fs::read_dir(new_dir)
        .map(|mut it| it.next().is_some())
        .unwrap_or(false)
    {
        return;
    }
    if let Err(e) = fs::create_dir_all(new_dir) {
        applog!(
            "[session-log] 创建日志目录 {} 失败（跳过迁移）: {e}",
            new_dir.display()
        );
        return;
    }
    for file in files {
        let Some(name) = file.file_name() else {
            continue;
        };
        if let Err(e) = fs::rename(&file, new_dir.join(name)) {
            applog!(
                "[session-log] 迁移旧日志 {} 失败（保留原位，请手动搬移）: {e}",
                file.display()
            );
        }
    }
    // 旧目录空了就删掉；删除失败不影响任何功能
    let _ = fs::remove_dir(old_dir);
}

/// 专用写入线程的指令。
enum LogMsg {
    Bytes(Vec<u8>),
    /// 立即 flush（`flush()`/`close()` 用）。
    Flush,
}

/// 一个会话的日志句柄：`append` 只把字节压进 channel（**从不阻塞调用方**），
/// 真正的磁盘写入由专用写入线程完成（reader 任务在 tokio worker 里同步做
/// `write_all`/`flush` 时，磁盘或网络盘抖动会拖慢其它会话的终端输出）。
/// 写入失败只记一次日志并停写；drop 即 flush + 关闭（join 有界等待）。
pub struct SessionLogger {
    sender: Mutex<Option<std::sync::mpsc::SyncSender<LogMsg>>>,
    writer: Mutex<Option<std::thread::JoinHandle<()>>>,
    path: PathBuf,
}

impl SessionLogger {
    /// 在指定目录创建日志文件（目录按需创建）。同名同秒已存在时追加 `-2`、`-3`…，
    /// 保证断线重连后新会话不覆盖旧日志。
    pub fn create(dir: &Path, host_name: &str, stamp: LogStamp) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        let stem = stamp.file_stem(host_name);
        let (file, path) = create_unique_file(dir, &stem)?;

        // 有界队列：磁盘/网络盘卡住时不能无限吃内存；满了直接丢弃新日志块
        //（日志是旁路功能，不能反向拖垮会话）。
        let (tx, rx) = std::sync::mpsc::sync_channel::<LogMsg>(256);
        let thread_name = format!("ruthenium-slog-{}", sanitize_host_name(host_name));
        let handle = std::thread::Builder::new()
            .name(thread_name)
            .spawn(move || writer_loop(file, rx))
            .map_err(|e| io::Error::other(format!("启动会话日志线程失败: {e}")))?;

        Ok(Self {
            sender: Mutex::new(Some(tx)),
            writer: Mutex::new(Some(handle)),
            path,
        })
    }

    /// 生产入口：`<logs_dir>\<host名或id>-<时间戳>.log`。
    pub fn open(host_name: &str) -> io::Result<Self> {
        let dir = logs_dir()?;
        Self::create(&dir, host_name, LogStamp::now())
    }

    /// 日志文件的完整路径。
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn send(&self, msg: LogMsg) {
        let guard = self.sender.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(tx) = guard.as_ref() {
            // 有界队列：满时 try_send 返回 Full，直接丢弃该块/该次 flush 请求，
            // 绝不让日志反向阻塞终端会话。
            let _ = tx.try_send(msg);
        }
    }

    /// 追加**原始**字节（解码之前调用）。非阻塞：只入队，落盘由写入线程完成。
    pub fn append(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        self.send(LogMsg::Bytes(bytes.to_vec()));
    }

    /// 请求写入线程立即 flush（异步生效；close 会同步等待完成）。
    pub fn flush(&self) {
        self.send(LogMsg::Flush);
    }

    /// 通知写入线程收尾并**同步等待**其 flush + 关闭（有界：5s）。
    /// 之后的 append 为 no-op。可重复调用。
    pub fn close(&self) {
        // 先丢弃 sender（让写入线程在排空队列后退出），再 join。
        let handle = {
            let mut sender = self.sender.lock().unwrap_or_else(|p| p.into_inner());
            *sender = None; // drop Sender → 写入线程 recv 返回 Disconnected
            let mut writer = self.writer.lock().unwrap_or_else(|p| p.into_inner());
            writer.take()
        };
        if let Some(handle) = handle {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while !handle.is_finished() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            // 仍没结束也不 join 卡死：写入线程最终会自己退出（recv 断开 + flush）
            if handle.is_finished() {
                let _ = handle.join();
            }
        }
    }
}

impl Drop for SessionLogger {
    fn drop(&mut self) {
        self.close();
    }
}

/// 写入线程主体：排空 channel → 阈值/周期 flush → 通道断开后终态 flush 并关闭文件。
fn writer_loop(file: File, rx: std::sync::mpsc::Receiver<LogMsg>) {
    let mut writer = BufWriter::with_capacity(FLUSH_THRESHOLD_BYTES, file);
    let mut unflushed = 0usize;
    let mut last_flush = std::time::Instant::now();
    let mut dead = false;
    while !dead {
        match rx.recv_timeout(FLUSH_INTERVAL) {
            Ok(LogMsg::Bytes(bytes)) => {
                if let Err(e) = writer.write_all(&bytes) {
                    applog!("[session-log] 写入会话日志失败，已停止记录该日志: {e}");
                    dead = true; // 句柄坏掉：停止写入，但仍做一次 close 尝试
                } else {
                    unflushed += bytes.len();
                    // 双保险：达到字节阈值立即 flush（否则等周期或断开）
                    if unflushed >= FLUSH_THRESHOLD_BYTES {
                        if let Err(e) = writer.flush() {
                            applog!("[session-log] flush 会话日志失败: {e}");
                            dead = true;
                        } else {
                            unflushed = 0;
                            last_flush = std::time::Instant::now();
                        }
                    }
                }
            }
            Ok(LogMsg::Flush) => {
                if let Err(e) = writer.flush() {
                    applog!("[session-log] flush 会话日志失败: {e}");
                    dead = true;
                } else {
                    unflushed = 0;
                    last_flush = std::time::Instant::now();
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // 周期性 flush：保证落盘延迟不超过 FLUSH_INTERVAL
                if last_flush.elapsed() >= FLUSH_INTERVAL {
                    if let Err(e) = writer.flush() {
                        applog!("[session-log] flush 会话日志失败: {e}");
                        dead = true;
                    } else {
                        unflushed = 0;
                        last_flush = std::time::Instant::now();
                    }
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                dead = true; // 所有 sender 已 drop：终态 flush + 关闭
            }
        }
    }
    // 终态：尽力 flush + 关闭句柄（close 会释放缓冲并关闭文件）
    if let Err(e) = writer.flush() {
        applog!("[session-log] 终态 flush 会话日志失败: {e}");
    }
    let _ = writer.into_inner().map(|f| f.sync_all());
}

/// 清空会话日志：删除目录下的 `*.log`（**保留**应用诊断日志 `app.log*` 与非日志文件）。
/// 返回删除的文件数；目录不存在视为 0（不报错）。
///
/// 为什么需要：会话日志是明文终端内容（可能含路径/配置/密钥输出），且本应用不做
/// 自动留存。提供一键清理入口，让用户能主动销毁这些数据。
pub fn clear_logs(dir: &Path) -> io::Result<usize> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    let mut removed = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        // 只删会话日志（`<主机名>-<时间戳>.log`），应用诊断日志 app.log / app.log.1 保留。
        if !name.ends_with(".log") || name.starts_with("app.log") {
            continue;
        }
        if fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}

/// 独占创建 `<stem>.log`；同秒重复时用 `-2`、`-3`… 后缀，绝不覆盖已有文件。
fn create_unique_file(dir: &Path, stem: &str) -> io::Result<(File, PathBuf)> {
    for attempt in 1..=100u32 {
        let name = if attempt == 1 {
            format!("{stem}.log")
        } else {
            format!("{stem}-{attempt}.log")
        };
        let path = dir.join(name);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((file, path)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::other(format!(
        "无法创建会话日志 {}: 同一秒内同名文件过多",
        dir.join(stem).display()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp() -> LogStamp {
        LogStamp {
            year: 2026,
            month: 11,
            day: 13,
            hour: 21,
            minute: 5,
            second: 9,
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "ruthenium-session-log-{tag}-{}",
            uuid::Uuid::new_v4()
        ))
    }

    /// 文件名格式固定为 `<名称>-YYYYMMDD-HHMMSS.log`；非法字符替换、空名回退。
    #[test]
    fn file_name_format_is_name_timestamp_log() {
        assert_eq!(stamp().file_name("web-01"), "web-01-20261113-210509.log");
        assert_eq!(
            stamp().file_name(r#"prod\web:01*"#),
            "prod_web_01_-20261113-210509.log"
        );
        assert_eq!(stamp().file_name("   "), "session-20261113-210509.log");
    }

    /// 原始字节（含 ANSI 转义、中文 UTF-8、控制字节）写入 → 关闭（同步 flush）→ 读回完全一致。
    #[test]
    fn raw_bytes_roundtrip_including_ansi() {
        let dir = temp_dir("raw");
        let log = SessionLogger::create(&dir, "web-01", stamp()).expect("创建日志失败");
        let path = log.path().to_path_buf();
        assert_eq!(
            path.file_name().unwrap().to_string_lossy(),
            "web-01-20261113-210509.log"
        );

        let raw: Vec<u8> =
            b"\x1b[31mred\x1b[0m \xe4\xb8\xad\xe6\x96\x87\r\n\x00\xff\xfe\x1b]0;title\x07".to_vec();
        log.append(&raw[..9]);
        log.append(&raw[9..]);
        // 写入线程异步落盘，drop 同步等待 flush 完成
        drop(log);

        let got = fs::read(&path).expect("读回日志失败");
        assert_eq!(
            got, raw,
            "日志必须是写入字节的完整副本（ANSI 原样、不转码）"
        );
        assert!(got.starts_with(b"\x1b[31m"), "ANSI 序列必须原样保留");

        let _ = fs::remove_dir_all(&dir);
    }

    /// 累计 64 KiB 立即落盘，不必等 2s 定时器（关闭后读回验证）。
    #[test]
    fn flush_threshold_writes_through() {
        let dir = temp_dir("threshold");
        let log = SessionLogger::create(&dir, "h", stamp()).expect("创建日志失败");
        let path = log.path().to_path_buf();
        let chunk = vec![b'x'; FLUSH_THRESHOLD_BYTES];
        log.append(&chunk);
        drop(log);
        let got = fs::read(&path).expect("读回日志失败");
        assert_eq!(
            got.len(),
            FLUSH_THRESHOLD_BYTES,
            "达到阈值应已写入并在关闭时 flush"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// 同一秒的同名会话不得互相覆盖（断线重连产生独立文件）。
    #[test]
    fn same_second_sessions_do_not_overwrite() {
        let dir = temp_dir("collision");
        let a = SessionLogger::create(&dir, "h", stamp()).expect("创建日志失败");
        let b = SessionLogger::create(&dir, "h", stamp()).expect("创建日志失败");
        assert_ne!(a.path(), b.path());
        assert!(a
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with("h-20261113-210509.log"));
        assert!(b
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with("-2.log"));

        let (pa, pb) = (a.path().to_path_buf(), b.path().to_path_buf());
        a.append(b"A");
        b.append(b"B");
        drop(a);
        drop(b);
        assert_eq!(fs::read(pa).unwrap(), b"A");
        assert_eq!(fs::read(pb).unwrap(), b"B");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 2);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn local_stamp_falls_back_to_utc_math() {
        assert_eq!(
            LogStamp::from_unix(0),
            LogStamp {
                year: 1970,
                month: 1,
                day: 1,
                hour: 0,
                minute: 0,
                second: 0
            }
        );
        assert_eq!(
            LogStamp::from_unix(1_700_000_000),
            LogStamp {
                year: 2023,
                month: 11,
                day: 14,
                hour: 22,
                minute: 13,
                second: 20
            }
        );
    }

    #[test]
    fn now_is_plausible() {
        let now = LogStamp::now();
        assert!((1970..=3000).contains(&now.year), "年份异常: {now:?}");
        assert!((1..=12).contains(&now.month), "月份异常: {now:?}");
        assert!((1..=31).contains(&now.day), "日期异常: {now:?}");
        assert!((0..=23).contains(&now.hour), "小时异常: {now:?}");
    }

    /// 生产路径必须是 `%LOCALAPPDATA%\Ruthenium\logs`（明文日志不进 Roaming）。
    #[test]
    fn logs_dir_is_under_local_data_dir() {
        let local = dirs::data_local_dir().expect("本机 Local 数据目录应可用");
        assert_eq!(
            logs_dir().expect("logs 目录"),
            local.join("Ruthenium").join("logs")
        );
    }

    /// 迁移：旧 Roaming logs 里的文件应移动到新 Local 目录，旧目录清空移除；
    /// 新目录已有文件时不重复迁移。
    #[test]
    fn migrate_moves_legacy_logs_once() {
        let root = std::env::temp_dir().join(format!(
            "ruthenium-logmig-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        // 直接测 migrate_logs_from（logs_dir 里的调用在 cfg(test) 下被禁用，
        // 避免单测搬动开发机真实数据）
        let new_dir = root.join("local").join("logs");
        let old_dir = root.join("roaming").join("logs");
        std::fs::create_dir_all(&old_dir).unwrap();
        std::fs::write(old_dir.join("web-01-20260101-000000.log"), b"old").unwrap();
        std::fs::write(old_dir.join("local-20260101-000001.log"), b"old2").unwrap();

        migrate_logs_from(&old_dir, &new_dir);
        assert!(
            new_dir.join("web-01-20260101-000000.log").exists(),
            "旧日志应迁入新目录"
        );
        assert!(new_dir.join("local-20260101-000001.log").exists());
        assert!(!old_dir.exists(), "旧目录清空后应移除");

        // 新目录已有文件（迁移已完成）：再放回旧目录文件也不重复迁移
        std::fs::create_dir_all(&old_dir).unwrap();
        std::fs::write(old_dir.join("another.log"), b"x").unwrap();
        migrate_logs_from(&old_dir, &new_dir);
        assert!(
            !new_dir.join("another.log").exists(),
            "新目录已有内容时不得重复迁移"
        );
        assert!(old_dir.join("another.log").exists(), "旧文件保留原位");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 定时 flush：append 少量字节后**不显式 flush**，等待一个 FLUSH_INTERVAL
    /// 周期，文件应已有内容（不能只在关闭标签时才落盘）。
    #[test]
    fn periodic_flush_writes_without_close() {
        let dir = std::env::temp_dir().join(format!("ruthenium-log-flush-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let logger = SessionLogger::create(&dir, "flush-test", LogStamp::now()).expect("创建日志");
        let path = logger.path().to_path_buf();
        logger.append(b"hello-periodic-flush\n");
        // 文件此刻可能仍是 0（还在 BufWriter 里）
        std::thread::sleep(FLUSH_INTERVAL + std::time::Duration::from_millis(700));
        let got = std::fs::read(&path).unwrap_or_default();
        assert_eq!(
            String::from_utf8_lossy(&got),
            "hello-periodic-flush\n",
            "一个 flush 周期后应已落盘（当前 {} 字节）",
            got.len()
        );
        logger.close();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 清空会话日志只删 `*.log` 会话文件，保留 `app.log*` 诊断日志与非日志文件。
    #[test]
    fn clear_logs_removes_session_logs_and_keeps_app_log() {
        let dir = temp_dir("clear");
        std::fs::create_dir_all(&dir).unwrap();
        for name in [
            "web-01-20261113-120000.log",
            "local-20261113-120001.log",
            "app.log",
            "app.log.1",
            "notes.txt",
        ] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }

        let removed = clear_logs(&dir).expect("清空会话日志失败");
        assert_eq!(removed, 2, "只应删除两个会话日志文件");
        assert!(!dir.join("web-01-20261113-120000.log").exists());
        assert!(!dir.join("local-20261113-120001.log").exists());
        assert!(dir.join("app.log").exists(), "应用诊断日志必须保留");
        assert!(dir.join("app.log.1").exists(), "诊断日志滚动备份必须保留");
        assert!(dir.join("notes.txt").exists(), "非 .log 文件不得删除");

        // 目录不存在：返回 0 而不是报错（首次启动还没建目录）
        assert_eq!(clear_logs(&dir.join("does-not-exist")).unwrap(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
