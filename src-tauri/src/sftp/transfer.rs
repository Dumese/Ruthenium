//! 分块传输（64 KiB）+ CancellationToken 取消 + sftp://progress 事件 + 完成 sftp://changed。
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use russh_sftp::client::SftpSession;
use russh_sftp::protocol::OpenFlags;
use tauri::AppHandle;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

use crate::error::{AppError, AppResult};
use crate::events::{self, SftpChangedPayload, SftpProgressPayload};

/// 传输分块大小：64 KiB（兼顾吞吐与取消响应延迟）。
pub const CHUNK: usize = 64 * 1024;

/// running 进度帧的最小发送间隔：大文件逐 chunk emit 会把事件总线打满，
/// 150ms 足够让前端进度条平滑且不产生洪水。
const PROGRESS_INTERVAL: Duration = Duration::from_millis(150);

fn emit_progress(app: &AppHandle, id: &str, done: u64, total: u64, state: &str) {
    events::emit(
        app,
        events::SFTP_PROGRESS,
        SftpProgressPayload {
            transfer_id: id.to_string(),
            bytes_done: done,
            bytes_total: total,
            state: state.to_string(),
        },
    );
}

/// 进度节流：数据 chunk 的 running 帧仅在距上次发送 ≥150ms 或跨过 1% 里程碑时发出。
/// 终态帧（done / error / cancelled）始终发送，不经过这里。
fn maybe_emit_running(
    app: &AppHandle,
    id: &str,
    done: u64,
    total: u64,
    last_emit: &mut Instant,
    last_milestone: &mut u64,
) {
    // 里程碑百分比：total=0 时视为 0；saturating_mul 防 done*100 溢出（超大文件）。
    let milestone = done.saturating_mul(100).checked_div(total).unwrap_or(0);
    if last_emit.elapsed() < PROGRESS_INTERVAL && milestone <= *last_milestone {
        return;
    }
    *last_emit = Instant::now();
    *last_milestone = milestone;
    emit_progress(app, id, done, total, "running");
}

/// 同名冲突候选名：第 `n` 个候选在扩展名前追加 " (n)"，n 从 1 开始。
/// 例：`/d/app.log` → `/d/app (1).log`；`/d/a.tar.gz` → `/d/a.tar (1).gz`。
///
/// 注意调用方必须**循环探测**直到名字空闲：只生成一个固定候选 " (1)" 时，
/// 第三次同名上传会 `create`（CREATE|TRUNCATE）覆盖第二次已写入的 " (1)" 文件，
/// 造成静默数据丢失。
pub fn dedup_name(path: &str, n: u32) -> String {
    let suffix = format!(" ({n})");
    match path.rfind('/') {
        Some(idx) if idx + 1 < path.len() => {
            let (dir, file) = path.split_at(idx + 1);
            match file.rfind('.') {
                Some(d) if d > 0 => format!("{dir}{}{suffix}{}", &file[..d], &file[d..]),
                _ => format!("{dir}{file}{suffix}"),
            }
        }
        // 无目录分隔符（Windows 本地路径 `C:\dir\a.txt` 走这里）：整个路径当文件名。
        _ => format!("{path}{suffix}"),
    }
}

/// 冲突候选名最多探测多少次：病态目录下不无限循环。
const MAX_DEDUP_TRIES: u32 = 200;

/// 并发传输的目标名占主表（进程内）。
///
/// `exists → create` 不是原子操作：两个同名传输会探测到同一个空闲名并先后以
/// TRUNCATE 打开同一文件，互相截断。占主表把“名字选择”做成互斥：探测空闲名之后
/// 还要在表里占住它，占不到就试下一个候选。表项在传输结束（成功/失败/取消）时释放。
/// 用 `Vec` 而非 `HashSet`：`Vec::new` 是 const（`HashSet` 不是），并发传输量级小，
/// 线性查找足够。
static REMOTE_RESERVED: StdMutex<Vec<String>> = StdMutex::new(Vec::new());
static LOCAL_RESERVED: StdMutex<Vec<String>> = StdMutex::new(Vec::new());

fn try_reserve(set: &StdMutex<Vec<String>>, path: &str) -> bool {
    let mut guard = set.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if guard.iter().any(|p| p == path) {
        return false;
    }
    guard.push(path.to_string());
    true
}

fn release_reserved(set: &StdMutex<Vec<String>>, path: &str) {
    let mut guard = set.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    guard.retain(|p| p != path);
}

/// 远端目标占主句柄：drop（传输结束/出错/取消）时自动释放，覆盖所有 return 路径。
struct ReservedRemote {
    path: String,
}

impl Drop for ReservedRemote {
    fn drop(&mut self) {
        release_reserved(&REMOTE_RESERVED, &self.path);
    }
}

/// 本地目标占主句柄（语义同 [`ReservedRemote`]）。
struct ReservedLocal {
    path: String,
}

impl Drop for ReservedLocal {
    fn drop(&mut self) {
        release_reserved(&LOCAL_RESERVED, &self.path);
    }
}

/// 选一个**未被磁盘占用且未被并发传输占主**的远端目标路径。
/// 依次尝试原名 → " (1)" → " (2)"…；探测失败（无法确认是否存在）直接取消——
/// 把"检查失败"当成"不存在"会以 TRUNCATE 打开真实文件。
async fn reserve_free_remote_path(sftp: &SftpSession, base: &str) -> AppResult<ReservedRemote> {
    for n in 0..=MAX_DEDUP_TRIES {
        let candidate = if n == 0 {
            base.to_string()
        } else {
            dedup_name(base, n)
        };
        match sftp.try_exists(&candidate).await {
            Ok(true) => continue, // 磁盘已占用
            Ok(false) => {
                if try_reserve(&REMOTE_RESERVED, &candidate) {
                    return Ok(ReservedRemote { path: candidate });
                }
                // 被并发传输占主：试下一个候选
            }
            Err(e) => {
                return Err(AppError::ESftpIo(format!(
                    "无法确认目标 {candidate} 是否存在，已取消以避免覆盖: {e}"
                )))
            }
        }
    }
    Err(AppError::ESftpIo(format!(
        "同名文件过多（>{MAX_DEDUP_TRIES}），请先整理目标目录: {base}"
    )))
}

/// 本地版 [`reserve_free_remote_path`]（下载用）。
async fn reserve_free_local_path(base: &str) -> AppResult<ReservedLocal> {
    for n in 0..=MAX_DEDUP_TRIES {
        let candidate = if n == 0 {
            base.to_string()
        } else {
            dedup_name(base, n)
        };
        match tokio::fs::try_exists(&candidate).await {
            Ok(true) => continue,
            Ok(false) => {
                if try_reserve(&LOCAL_RESERVED, &candidate) {
                    return Ok(ReservedLocal { path: candidate });
                }
            }
            Err(e) => {
                return Err(AppError::ESftpIo(format!(
                    "无法确认目标 {candidate} 是否存在，已取消以避免覆盖: {e}"
                )))
            }
        }
    }
    Err(AppError::ESftpIo(format!(
        "同名文件过多（>{MAX_DEDUP_TRIES}），请先整理目标目录: {base}"
    )))
}

/// 取消/失败时清理远端半成品。清理失败**必须留痕**（不能静默忽略），
/// 用户以为“取消即清理干净”，实际目录里堆积不可辨识残留。
async fn cleanup_remote_partial(sftp: &SftpSession, path: &str) {
    if let Err(e) = sftp.remove_file(path).await {
        applog!("[sftp] 清理远端半成品 {path} 失败（可能残留，请手动删除）: {e}");
    }
}

/// 本地版 [`cleanup_remote_partial`]。
async fn cleanup_local_partial(path: &str) {
    if let Err(e) = tokio::fs::remove_file(path).await {
        applog!("[sftp] 清理本地半成品 {path} 失败（可能残留，请手动删除）: {e}");
    }
}

/// 取路径最后一段文件名（同时接受 POSIX `/` 与 Windows `\` 分隔符）。
fn base_name(path: &str) -> Option<&str> {
    path.rsplit(['/', '\\']).find(|s| !s.is_empty())
}

/// 远端 POSIX 目录拼接：`/` 结尾不重复加斜杠。
fn join_remote(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// 上传目标解析：UI/拖拽传的是**目标目录**，而本模块内部需要**最终文件路径**。
/// 目标是已存在目录时拼上源文件名；目标是完整文件路径时保持原样（兼容脚本直接传路径）。
async fn resolve_upload_target(
    sftp: &SftpSession,
    remote_path: &str,
    local_path: &str,
) -> AppResult<String> {
    let is_dir = match sftp.metadata(remote_path).await {
        Ok(meta) => matches!(meta.file_type(), russh_sftp::protocol::FileType::Dir),
        // 不存在 / 不可读：按完整文件路径处理，交给随后的 exists 探测与去重
        Err(_) => false,
    };
    if !is_dir {
        return Ok(remote_path.to_string());
    }
    let name = base_name(local_path)
        .ok_or_else(|| AppError::ESftpIo(format!("无法从本地路径解析文件名: {local_path}")))?;
    Ok(join_remote(remote_path, name))
}

/// 下载目标解析：目标是已存在本地目录时拼上远端文件名；否则按完整文件路径处理。
fn resolve_download_target(local_path: &str, remote_path: &str) -> AppResult<String> {
    if !std::path::Path::new(local_path).is_dir() {
        return Ok(local_path.to_string());
    }
    let name = base_name(remote_path)
        .ok_or_else(|| AppError::ESftpIo(format!("无法从远端路径解析文件名: {remote_path}")))?;
    Ok(std::path::Path::new(local_path)
        .join(name)
        .to_string_lossy()
        .to_string())
}

/// 上传：localPath → remotePath。
pub async fn upload(
    app: AppHandle,
    sftp: Arc<SftpSession>,
    transfer_id: String,
    session_id: String,
    local_path: String,
    remote_path: String,
    cancel: CancellationToken,
) -> AppResult<()> {
    // 打开本地失败：此刻还没发过任何进度帧，补发一帧 error 让前端把该传输标为失败。
    let mut local = match tokio::fs::File::open(&local_path).await {
        Ok(f) => f,
        Err(e) => {
            emit_progress(&app, &transfer_id, 0, 0, "error");
            return Err(AppError::ESftpIo(format!(
                "打开本地文件 {local_path} 失败: {e}"
            )));
        }
    };
    let total = local.metadata().await.map(|m| m.len()).unwrap_or(0);

    // 同名追加 " (n)"：exists 检查失败时直接取消——
    // 把「检查失败」当成「不存在」，随后的 create 会以 O_TRUNC
    // 覆盖一个不确定是否存在的文件，宁可拒绝也不冒覆盖风险。
    // 冲突时必须**循环**找空闲名：只试 " (1)" 会在第三次同名上传时覆盖第二次的文件。
    // 目标可能是目录（UI 语义），先解析成“目录/源文件名”的最终路径。
    // 选定过程同时做并发占主（exists→create 非原子，同名并发会互相截断）。
    let target = match resolve_upload_target(&sftp, &remote_path, &local_path).await {
        Ok(p) => p,
        Err(e) => {
            emit_progress(&app, &transfer_id, 0, total, "error");
            return Err(e);
        }
    };
    let reservation = match reserve_free_remote_path(&sftp, &target).await {
        Ok(r) => r,
        Err(e) => {
            emit_progress(&app, &transfer_id, 0, total, "error");
            return Err(e);
        }
    };
    let final_path = reservation.path.clone();
    // EXCL：占主表只防本进程并发；外部进程仍可能在 exists→create 之间创建
    // 同名文件，必须让服务器端以“不存在才创建”为原子条件，避免静默覆盖。
    let mut remote = match sftp
        .open_with_flags(
            &final_path,
            OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::EXCLUDE,
        )
        .await
    {
        Ok(f) => f,
        Err(e) => {
            emit_progress(&app, &transfer_id, 0, total, "error");
            return Err(AppError::ESftpIo(format!(
                "创建远端文件 {final_path} 失败（目标可能刚被其他程序占用）: {e}"
            )));
        }
    };

    let mut done: u64 = 0;
    let mut buf = vec![0u8; CHUNK];
    emit_progress(&app, &transfer_id, 0, total, "running");
    let mut last_emit = Instant::now();
    let mut last_milestone: u64 = 0;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                emit_progress(&app, &transfer_id, done, total, "cancelled");
                cleanup_remote_partial(&sftp, &final_path).await;
                return Err(AppError::ESftpIo("传输已取消".to_string()));
            }
            n = local.read(&mut buf) => {
                let n = match n {
                    Ok(n) => n,
                    Err(e) => {
                        emit_progress(&app, &transfer_id, done, total, "error");
                        cleanup_remote_partial(&sftp, &final_path).await;
                        return Err(AppError::ESftpIo(format!("读取本地文件失败: {e}")));
                    }
                };
                if n == 0 { break; }
                if let Err(e) = remote.write_all(&buf[..n]).await {
                    emit_progress(&app, &transfer_id, done, total, "error");
                    cleanup_remote_partial(&sftp, &final_path).await;
                    return Err(AppError::ESftpIo(format!("写入远端失败: {e}")));
                }
                done += n as u64;
                maybe_emit_running(&app, &transfer_id, done, total, &mut last_emit, &mut last_milestone);
            }
        }
    }
    // sync_all 成功 = 数据已全部落盘。close 只是收尾握手，失败（网络恰在
    // 此刻断开）不代表文件不完整——保留文件供校验/续用，绝不能再清理
    // （清理会误删已完整传输的文件）。
    if let Err(e) = remote.sync_all().await {
        emit_progress(&app, &transfer_id, done, total, "error");
        cleanup_remote_partial(&sftp, &final_path).await;
        return Err(AppError::ESftpIo(format!("远端落盘失败: {e}")));
    }
    if let Err(e) = remote.close().await {
        emit_progress(&app, &transfer_id, done, total, "error");
        applog!("[sftp] 远端文件 {final_path} 已完整落盘但关闭握手失败（保留文件）: {e}");
        return Err(AppError::ESftpIo(format!("关闭远端文件失败: {e}")));
    }
    emit_progress(&app, &transfer_id, done, total, "done");
    events::emit(
        &app,
        events::SFTP_CHANGED,
        SftpChangedPayload {
            session_id,
            side: "remote".to_string(),
            path: parent_dir(&final_path),
        },
    );
    Ok(())
}

/// 下载：remotePath → localPath。
pub async fn download(
    app: AppHandle,
    sftp: Arc<SftpSession>,
    transfer_id: String,
    session_id: String,
    remote_path: String,
    local_path: String,
    cancel: CancellationToken,
) -> AppResult<()> {
    let meta = match sftp.metadata(&remote_path).await {
        Ok(m) => m,
        Err(e) => {
            emit_progress(&app, &transfer_id, 0, 0, "error");
            return Err(AppError::ESftpIo(format!("读取远端元数据失败: {e}")));
        }
    };
    let total = meta.size.unwrap_or(0);

    // 同名追加 " (n)"：本地 exists 检查失败时直接取消（语义同 upload），冲突时循环探测。
    // 目标可能是目录（UI 语义），先解析成“目录/远端文件名”的最终路径。
    // 选定过程同时做并发占主。
    let local_target = match resolve_download_target(&local_path, &remote_path) {
        Ok(p) => p,
        Err(e) => {
            emit_progress(&app, &transfer_id, 0, total, "error");
            return Err(e);
        }
    };
    let reservation = match reserve_free_local_path(&local_target).await {
        Ok(r) => r,
        Err(e) => {
            emit_progress(&app, &transfer_id, 0, total, "error");
            return Err(e);
        }
    };
    let final_path = reservation.path.clone();
    let mut remote = match sftp.open(&remote_path).await {
        Ok(f) => f,
        Err(e) => {
            emit_progress(&app, &transfer_id, 0, total, "error");
            return Err(AppError::ESftpIo(format!(
                "打开远端文件 {remote_path} 失败: {e}"
            )));
        }
    };
    // create_new：与远端 EXCL 对称，防止外部进程在 exists→create 窗口里
    // 创建同名文件后被静默覆盖。
    let mut local = match tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&final_path)
        .await
    {
        Ok(f) => f,
        Err(e) => {
            emit_progress(&app, &transfer_id, 0, total, "error");
            return Err(AppError::ESftpIo(format!(
                "创建本地文件 {final_path} 失败（目标可能刚被其他程序占用）: {e}"
            )));
        }
    };

    let mut done: u64 = 0;
    let mut buf = vec![0u8; CHUNK];
    emit_progress(&app, &transfer_id, 0, total, "running");
    let mut last_emit = Instant::now();
    let mut last_milestone: u64 = 0;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                emit_progress(&app, &transfer_id, done, total, "cancelled");
                // 半截文件不留现场：取消同样清理已创建的本地文件。
                cleanup_local_partial(&final_path).await;
                return Err(AppError::ESftpIo("传输已取消".to_string()));
            }
            n = remote.read(&mut buf) => {
                let n = match n {
                    Ok(n) => n,
                    Err(e) => {
                        emit_progress(&app, &transfer_id, done, total, "error");
                        cleanup_local_partial(&final_path).await;
                        return Err(AppError::ESftpIo(format!("读取远端失败: {e}")));
                    }
                };
                if n == 0 { break; }
                if let Err(e) = local.write_all(&buf[..n]).await {
                    emit_progress(&app, &transfer_id, done, total, "error");
                    cleanup_local_partial(&final_path).await;
                    return Err(AppError::ESftpIo(format!("写入本地失败: {e}")));
                }
                done += n as u64;
                maybe_emit_running(&app, &transfer_id, done, total, &mut last_emit, &mut last_milestone);
            }
        }
    }
    if let Err(e) = local.sync_all().await {
        emit_progress(&app, &transfer_id, done, total, "error");
        cleanup_local_partial(&final_path).await;
        return Err(AppError::ESftpIo(format!("本地落盘失败: {e}")));
    }
    emit_progress(&app, &transfer_id, done, total, "done");
    events::emit(
        &app,
        events::SFTP_CHANGED,
        SftpChangedPayload {
            session_id,
            side: "local".to_string(),
            path: parent_dir(&final_path),
        },
    );
    Ok(())
}

fn parent_dir(path: &str) -> String {
    match path.rfind(['/', '\\']) {
        Some(0) => "/".to_string(),
        Some(idx) => path[..idx].to_string(),
        None => ".".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        base_name, dedup_name, join_remote, reserve_free_local_path, resolve_download_target,
        LOCAL_RESERVED,
    };

    #[test]
    fn dedup_name_keeps_extension_and_increments() {
        assert_eq!(
            dedup_name("/home/deploy/app.log", 1),
            "/home/deploy/app (1).log"
        );
        assert_eq!(
            dedup_name("/home/deploy/app.log", 2),
            "/home/deploy/app (2).log"
        );
        assert_eq!(
            dedup_name("/home/deploy/notes", 1),
            "/home/deploy/notes (1)"
        );
        // 无 '/'（Windows 本地路径）：整体当文件名，仍能生成不同候选
        assert_eq!(
            dedup_name(r"C:\Users\me\a.txt", 1),
            r"C:\Users\me\a.txt (1)"
        );
        // 前导点的隐藏文件没有扩展名
        assert_eq!(dedup_name("/d/.hidden", 1), "/d/.hidden (1)");
        // 多段扩展名只在最后一段前插序号
        assert_eq!(dedup_name("/d/a.tar.gz", 3), "/d/a.tar (3).gz");
    }

    /// 连续冲突必须给出**互不相同**的名字，否则第三次同名上传会以
    /// CREATE|TRUNCATE 覆盖第二次写入的文件（静默数据丢失）。
    #[test]
    fn dedup_candidates_are_unique_so_repeated_upload_never_overwrites() {
        let base = "/tmp/a.txt";
        let names: Vec<String> = (1..=5).map(|n| dedup_name(base, n)).collect();
        let mut unique = names.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(names.len(), unique.len(), "候选名必须互不相同: {names:?}");
        assert!(!names.contains(&base.to_string()), "候选名不得等于原路径");
        // 第一个候选仍保持既有命名习惯
        assert_eq!(names[0], "/tmp/a (1).txt");
    }

    /// Windows / POSIX 路径都要能取到文件名；尾部斜杠不产生空名。
    #[test]
    fn base_name_handles_both_separators() {
        assert_eq!(base_name("/home/deploy/a.txt"), Some("a.txt"));
        assert_eq!(base_name(r"C:\Users\me\a.txt"), Some("a.txt"));
        assert_eq!(base_name("/home/deploy/dir/"), Some("dir"));
        assert_eq!(base_name("file.txt"), Some("file.txt"));
        assert_eq!(base_name("/"), None);
        assert_eq!(base_name(""), None);
    }

    /// 远端目录拼接（根目录 / 带尾斜杠 / 普通目录）。
    #[test]
    fn join_remote_avoids_double_slash() {
        assert_eq!(join_remote("/home/deploy", "a.txt"), "/home/deploy/a.txt");
        assert_eq!(join_remote("/", "a.txt"), "/a.txt");
        assert_eq!(join_remote("/home/deploy/", "a.txt"), "/home/deploy/a.txt");
        assert_eq!(join_remote("rel", "a.txt"), "rel/a.txt");
    }

    /// 下载目标为目录时拼远端文件名；为文件路径时保持原样。
    #[test]
    fn download_target_resolves_directory_semantics() {
        let dir = std::env::temp_dir().join(format!(
            "ruthenium-dl-target-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let dir_str = dir.to_string_lossy().to_string();

        let resolved = resolve_download_target(&dir_str, "/home/deploy/a.txt").unwrap();
        assert_eq!(
            std::path::Path::new(&resolved),
            dir.join("a.txt"),
            "目录目标必须拼上远端文件名"
        );

        let file_path = dir.join("existing.bin").to_string_lossy().to_string();
        assert_eq!(
            resolve_download_target(&file_path, "/home/deploy/a.txt").unwrap(),
            file_path,
            "完整文件路径必须原样沿用（兼容旧调用 / E2E）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 并发占主——同名目标被别的传输占主时必须拿到**下一个**候选，
    /// 释放后原名才可复用。exists→create 非原子，两个同名传输会互相截断。
    #[tokio::test]
    async fn concurrent_reservation_yields_distinct_names() {
        let dir = std::env::temp_dir().join(format!(
            "ruthenium-resv-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let base = dir.join("a.txt").to_string_lossy().to_string();

        let r1 = reserve_free_local_path(&base).await.expect("首次占主");
        let r2 = reserve_free_local_path(&base)
            .await
            .expect("并发占主应选下一候选");
        assert_ne!(r1.path, r2.path, "并发同名不得拿到同一目标");
        assert!(
            r2.path.starts_with(&base) && r2.path.ends_with(" (1)"),
            "第二个候选应是 “原名 (1)”：{}",
            r2.path
        );

        // 第一个占主释放后，原名应可被新传输复用
        drop(r1);
        let r3 = reserve_free_local_path(&base)
            .await
            .expect("释放后原名应可用");
        assert_eq!(r3.path, base);

        // drop 全部后表必须清空（不留永久占主）
        drop(r2);
        drop(r3);
        {
            let guard = LOCAL_RESERVED
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            assert!(guard.is_empty(), "传输结束后不得残留占主条目: {guard:?}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
