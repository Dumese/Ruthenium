//! SFTP 与本地文件命令。
//!
//! # 安全边界说明（设计取舍）
//!
//! 本地 FS 命令（`local_list` / `local_mkdir` / `local_delete` / `local_rename`
//! 及下载目标 `File::create`）**有意保留全盘访问**，不套用备份导出的一次性 token 防护，理由：
//!
//! 1. SFTP 双栏浏览器的产品语义就是访问本机任意目录（「此电脑」盘符列表是
//!    明确特性，见 `local_list` 空路径分支），token/白名单会把核心功能堵死；
//! 2. Tauri 2 的 capabilities 体系只约束插件命令，应用自身命令的防护由
//!    Rust 侧实现负责——本模块对**写入类**操作已有最低限度校验：
//!    `validate_new_name` 阻断 `..`/分隔符/绝对路径注入，
//!    重命名/传输有不覆盖检查与并发占主；
//! 3. `local_delete` 是唯一递归删除入口，当前直接执行。若后续要收紧，
//!    优先方案是「SFTP 视图激活期间才允许 + 路径必须在最近一次 local_list
//!    返回的目录树内」，而不是 token 化（删除目标由列表点击产生，天然可校验）。
//!
//! 已知残余风险：WebView 被攻破（XSS/供应链）时，攻击者可直接 invoke 这些
//! 命令读写任意本地路径。这与任意 SSH 客户端（scp/sftp -r）暴露面相当，
//! 由 CSP（无外部脚本源）与依赖最小化兜底。
use std::time::UNIX_EPOCH;

use russh_sftp::client::SftpSession;
use tokio_util::sync::CancellationToken;

use crate::error::{AppError, AppResult};
use crate::models::FileEntry;
use crate::sftp::{list_dir, transfer};
use crate::state::AppState;

// ---------------- 重命名（本地 / 远端共用校验） ----------------

/// 新文件名只允许是单个文件名：非空、不含路径分隔符、不是 `.` / `..`。
/// 返回 trim 后的名字（前端输入框也会 trim，保持一致）。
fn validate_new_name(new_name: &str) -> AppResult<&str> {
    let name = new_name.trim();
    if name.is_empty() {
        return Err(AppError::EInvalidInput("新文件名不能为空".to_string()));
    }
    if name == "." || name == ".." {
        return Err(AppError::EInvalidInput(format!("非法的文件名: {name}")));
    }
    if name.contains('/') || name.contains('\\') {
        return Err(AppError::EInvalidInput(format!(
            "新文件名不能包含路径分隔符: {name}"
        )));
    }
    Ok(name)
}

/// 本地重命名的纯执行部分（便于单测）：校验 → 不覆盖检查 → `std::fs::rename`。
/// 返回新的完整路径。
///
/// 已知边界（接受）：不覆盖检查与 rename 之间存在 TOCTOU 窗口——检查后
/// 目标被并发创建时，Windows `std::fs::rename` 走 `MoveFileEx(REPLACE_EXISTING)`
/// 会覆盖它。触发需要「并发操作同一目录 + 恰好同名」，概率极低；SFTP 传输路径
/// 已有占主表防并发覆盖，此处不再引入跨进程锁（代价大于收益）。
fn local_rename_path(path: &str, new_name: &str) -> AppResult<String> {
    let new_name = validate_new_name(new_name)?;
    // 去掉末尾分隔符：目录路径若带 `\`，Path::with_file_name 会错误地退到上级。
    let old = std::path::Path::new(path.trim_end_matches(['/', '\\']));
    if old.file_name().is_none() {
        return Err(AppError::EInvalidInput(format!("路径无效: {path}")));
    }
    let new_path = old.with_file_name(new_name);
    if new_path.as_path() == old {
        return Ok(path.to_string());
    }
    // Windows 只改大小写（`a.txt` → `A.txt`）时，目标在大小写不敏感的文件系统上
    // 会命中源文件自身，误报"目标已存在"。大小写变更交给 rename 完成。
    let case_only = new_path.parent() == old.parent()
        && match (new_path.file_name(), old.file_name()) {
            (Some(a), Some(b)) => {
                a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
            }
            _ => false,
        };
    // 不覆盖既有条目；symlink_metadata 也能识别失效的符号链接。
    if !case_only {
        match std::fs::symlink_metadata(&new_path) {
            Ok(_) => {
                return Err(AppError::EInvalidInput(format!(
                    "目标已存在：{}（不会覆盖）",
                    new_path.display()
                )))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(AppError::EFs(format!(
                    "检查目标 {} 失败: {e}",
                    new_path.display()
                )))
            }
        }
    }
    std::fs::rename(old, &new_path).map_err(|e| {
        AppError::EFs(format!(
            "重命名 {} 为 {} 失败: {e}",
            old.display(),
            new_path.display()
        ))
    })?;
    Ok(new_path.to_string_lossy().to_string())
}

/// 远端 POSIX 路径重命名目标：返回（新完整路径, 父目录）。
fn remote_rename_target(path: &str, new_name: &str) -> AppResult<(String, String)> {
    if path.trim().is_empty() {
        return Err(AppError::EInvalidInput("路径不能为空".to_string()));
    }
    let new_name = validate_new_name(new_name)?;
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(AppError::EInvalidInput("不能重命名远端根目录".to_string()));
    }
    let (dir, _) = trimmed
        .rsplit_once('/')
        .filter(|(_, name)| !name.is_empty())
        .ok_or_else(|| AppError::EInvalidInput(format!("远端路径无效: {path}")))?;
    let parent = if dir.is_empty() { "/" } else { dir };
    let new_path = if parent == "/" {
        format!("/{new_name}")
    } else {
        format!("{parent}/{new_name}")
    };
    Ok((new_path, parent.to_string()))
}

/// 远端目标是否已存在（lstat：失效符号链接也算已占用，避免静默覆盖）。
async fn remote_target_exists(sftp: &SftpSession, path: &str) -> AppResult<bool> {
    match sftp.symlink_metadata(path).await {
        Ok(_) => Ok(true),
        Err(russh_sftp::client::error::Error::Status(status))
            if status.status_code == russh_sftp::protocol::StatusCode::NoSuchFile =>
        {
            Ok(false)
        }
        Err(e) => Err(AppError::ESftpIo(format!("检查远端目标 {path} 失败: {e}"))),
    }
}

/// 取路径的父目录（事件刷新用）。
fn parent_of(path: &str) -> String {
    std::path::Path::new(path)
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default()
}

// ---------------- 本地 FS ----------------

fn sort_local_entries(mut entries: Vec<FileEntry>) -> Vec<FileEntry> {
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    entries
}

/// 「此电脑」视图：Windows 返回可用盘符，其它平台返回用户主目录。
#[tauri::command]
async fn list_local_roots() -> AppResult<Vec<FileEntry>> {
    #[cfg(windows)]
    {
        // 并发探测 A:–Z：断开的网络盘 / 光驱 metadata 可能阻塞数秒，
        // 逐个探测会拖慢「此电脑」列表。每个盘符独立任务 + 750ms 超时，
        // 超时视为不存在；结果按盘符字母序返回（行为与返回格式不变）。
        let mut handles = Vec::new();
        for letter in b'A'..=b'Z' {
            let root = format!("{}:\\", letter as char);
            handles.push(tokio::spawn(async move {
                tokio::time::timeout(
                    std::time::Duration::from_millis(750),
                    tokio::fs::metadata(&root),
                )
                .await
                .ok()
                .and_then(|r| r.ok())
                .map(|_| letter)
            }));
        }
        let mut letters = Vec::new();
        for h in handles {
            if let Ok(Some(letter)) = h.await {
                letters.push(letter);
            }
        }
        // spawn 顺序即 A→Z，且逐个 await 保序；sort 只是防御性保证字母序。
        letters.sort_unstable();
        let out: Vec<FileEntry> = letters
            .into_iter()
            .map(|letter| FileEntry {
                name: format!("{}:", letter as char),
                is_dir: true,
                size: None,
                modified: None,
                mode: None,
            })
            .collect();
        if out.is_empty() {
            return Err(AppError::EFs("未找到可用的盘符".to_string()));
        }
        Ok(out)
    }
    #[cfg(not(windows))]
    {
        let home =
            dirs::home_dir().ok_or_else(|| AppError::EFs("无法确定用户主目录".to_string()))?;
        local_list(home.to_string_lossy().to_string()).await
    }
}

/// 本机文件权限位：Unix 取 mode（含类型位），Windows 无 POSIX 权限概念返回 None。
#[cfg(unix)]
fn local_mode(meta: &std::fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    Some(meta.mode())
}
#[cfg(not(unix))]
fn local_mode(_meta: &std::fs::Metadata) -> Option<u32> {
    None
}

/// 列出本地目录；空路径表示「此电脑」（Windows 盘符 / 其它平台主目录）。
#[tauri::command]
pub async fn local_list(path: String) -> AppResult<Vec<FileEntry>> {
    // 空路径 = 「此电脑」：列出可用盘符（SFTP 本地栏的初始视图）。
    if path.trim().is_empty() {
        return list_local_roots().await;
    }
    let mut out = Vec::new();
    let mut read = tokio::fs::read_dir(&path)
        .await
        .map_err(|e| AppError::EFs(format!("读取目录 {path} 失败: {e}")))?;
    while let Some(entry) = read
        .next_entry()
        .await
        .map_err(|e| AppError::EFs(format!("遍历目录失败: {e}")))?
    {
        // 条目级失败不让整个目录列不出来。典型场景：列表遍历期间文件被删除、
        // 失效符号链接、被独占占用（Windows 上 metadata 会对断链报错）。
        let meta = match entry.metadata().await {
            Ok(m) => m,
            Err(e) => {
                applog!(
                    "[sftp] 跳过无法读取元数据的本地条目 {:?}: {e}",
                    entry.file_name()
                );
                continue;
            }
        };
        out.push(FileEntry {
            name: entry.file_name().to_string_lossy().to_string(),
            is_dir: meta.is_dir(),
            size: if meta.is_file() {
                Some(meta.len())
            } else {
                None
            },
            modified: meta
                .modified()
                .ok()
                .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64),
            mode: local_mode(&meta),
        });
    }
    Ok(sort_local_entries(out))
}

/// 在本地目录下新建子目录；目录名复用 [`validate_new_name`] 的同一套校验。
///
/// 不能用「只校验非空 + Path::join」：`join` 遇到绝对路径会丢弃 `path`，
/// `..` 也会越出目标目录，等于允许 IPC 调用方在任意位置建目录。
#[tauri::command]
pub async fn local_mkdir(path: String, name: String) -> AppResult<()> {
    let name = validate_new_name(&name)?;
    let dir = std::path::Path::new(&path).join(name);
    tokio::fs::create_dir(&dir)
        .await
        .map_err(|e| AppError::EFs(format!("创建目录 {} 失败: {e}", dir.display())))
}

/// 删除本地文件或目录；目录递归删除，符号链接只删链接本身。
#[tauri::command]
pub async fn local_delete(path: String) -> AppResult<()> {
    let p = std::path::Path::new(&path);
    let meta = tokio::fs::symlink_metadata(p)
        .await
        .map_err(|e| AppError::EFs(format!("读取 {path} 失败: {e}")))?;
    if meta.is_dir() {
        tokio::fs::remove_dir_all(p)
            .await
            .map_err(|e| AppError::EFs(format!("删除目录 {path} 失败: {e}")))
    } else {
        tokio::fs::remove_file(p)
            .await
            .map_err(|e| AppError::EFs(format!("删除文件 {path} 失败: {e}")))
    }
}

/// 本地重命名（右键「重命名」）：`path` 为旧完整路径，`newName` 为新文件名。
/// 返回新的完整路径；目标已存在时拒绝，不覆盖。
#[tauri::command]
pub async fn local_rename(
    state: tauri::State<'_, AppState>,
    path: String,
    new_name: String,
) -> AppResult<String> {
    let new_path = local_rename_path(&path, &new_name)?;
    crate::events::emit(
        &state.app,
        crate::events::SFTP_CHANGED,
        crate::events::SftpChangedPayload {
            session_id: String::new(),
            side: "local".to_string(),
            path: parent_of(&new_path),
        },
    );
    Ok(new_path)
}

// ---------------- SFTP ----------------

/// 远端默认目录（登录用户 home）：SFTP 首次打开时的起点。
#[tauri::command]
pub async fn sftp_home(state: tauri::State<'_, AppState>, session_id: String) -> AppResult<String> {
    let sftp = state.sftp.get(&state.ssh, &state.app, &session_id).await?;
    crate::sftp::home_dir(&sftp).await
}

/// 列出远端目录（目录在前，名称按不区分大小写排序）。
#[tauri::command]
pub async fn sftp_list(
    state: tauri::State<'_, AppState>,
    session_id: String,
    path: String,
) -> AppResult<Vec<FileEntry>> {
    let sftp = state.sftp.get(&state.ssh, &state.app, &session_id).await?;
    list_dir(&sftp, &path).await
}

async fn register_transfer(
    state: &tauri::State<'_, AppState>,
    transfer_id: &str,
) -> CancellationToken {
    let cancel = CancellationToken::new();
    state
        .transfers
        .lock()
        .await
        .insert(transfer_id.to_string(), cancel.clone());
    cancel
}

/// 启动异步上传，返回 transferId；进度经 `sftp://progress` 上报。
#[tauri::command]
pub async fn sftp_upload(
    state: tauri::State<'_, AppState>,
    session_id: String,
    local_path: String,
    remote_path: String,
) -> AppResult<String> {
    let sftp = state.sftp.get(&state.ssh, &state.app, &session_id).await?;
    let transfer_id = uuid::Uuid::new_v4().to_string();
    let cancel = register_transfer(&state, &transfer_id).await;
    let app = state.app.clone();
    let sid = session_id.clone();
    let tid = transfer_id.clone();
    let cleanup_app = state.app.clone();
    let cleanup_id = transfer_id.clone();
    tokio::spawn(async move {
        if let Err(e) =
            transfer::upload(app.clone(), sftp, tid, sid, local_path, remote_path, cancel).await
        {
            events_app_error(&app, &e);
        }
        // 无论成功/失败/取消，结束即释放令牌，避免表只增不减（长会话内存泄漏）。
        use tauri::Manager;
        if let Some(app_state) = cleanup_app.try_state::<crate::state::AppState>() {
            app_state.transfers.lock().await.remove(&cleanup_id);
        }
    });
    Ok(transfer_id)
}

/// 启动异步下载，返回 transferId；进度经 `sftp://progress` 上报。
#[tauri::command]
pub async fn sftp_download(
    state: tauri::State<'_, AppState>,
    session_id: String,
    remote_path: String,
    local_path: String,
) -> AppResult<String> {
    let sftp = state.sftp.get(&state.ssh, &state.app, &session_id).await?;
    let transfer_id = uuid::Uuid::new_v4().to_string();
    let cancel = register_transfer(&state, &transfer_id).await;
    let app = state.app.clone();
    let sid = session_id.clone();
    let tid = transfer_id.clone();
    let cleanup_app = state.app.clone();
    let cleanup_id = transfer_id.clone();
    tokio::spawn(async move {
        if let Err(e) =
            transfer::download(app.clone(), sftp, tid, sid, remote_path, local_path, cancel).await
        {
            events_app_error(&app, &e);
        }
        // 无论成功/失败/取消，结束即释放令牌，避免表只增不减（长会话内存泄漏）。
        use tauri::Manager;
        if let Some(app_state) = cleanup_app.try_state::<crate::state::AppState>() {
            app_state.transfers.lock().await.remove(&cleanup_id);
        }
    });
    Ok(transfer_id)
}

/// 在远端新建目录，并通知前端刷新父目录。
#[tauri::command]
pub async fn sftp_mkdir(
    state: tauri::State<'_, AppState>,
    session_id: String,
    path: String,
) -> AppResult<()> {
    let sftp = state.sftp.get(&state.ssh, &state.app, &session_id).await?;
    sftp.create_dir(&path)
        .await
        .map_err(|e| AppError::ESftpIo(format!("创建远端目录 {path} 失败: {e}")))?;
    crate::events::emit(
        &state.app,
        crate::events::SFTP_CHANGED,
        crate::events::SftpChangedPayload {
            session_id,
            side: "remote".to_string(),
            path: path
                .trim_end_matches('/')
                .rsplit_once('/')
                .map(|(d, _)| {
                    if d.is_empty() {
                        "/".to_string()
                    } else {
                        d.to_string()
                    }
                })
                .unwrap_or_else(|| ".".to_string()),
        },
    );
    Ok(())
}

/// 删除远端文件或目录，并通知前端刷新父目录。
#[tauri::command]
pub async fn sftp_delete(
    state: tauri::State<'_, AppState>,
    session_id: String,
    path: String,
) -> AppResult<()> {
    let sftp = state.sftp.get(&state.ssh, &state.app, &session_id).await?;
    // lstat（symlink_metadata）：指向目录的符号链接必须按「文件」删除链接本身，
    // 跟随链接的 metadata 会把它误判成目录导致 remove_dir 失败（同 remote_target_exists 的语义）。
    let meta = sftp
        .symlink_metadata(&path)
        .await
        .map_err(|e| AppError::ESftpIo(format!("读取远端元数据 {path} 失败: {e}")))?;
    let parent = path
        .trim_end_matches('/')
        .rsplit_once('/')
        .map(|(d, _)| {
            if d.is_empty() {
                "/".to_string()
            } else {
                d.to_string()
            }
        })
        .unwrap_or_else(|| ".".to_string());
    if matches!(meta.file_type(), russh_sftp::protocol::FileType::Dir) {
        sftp.remove_dir(&path)
            .await
            .map_err(|e| AppError::ESftpIo(format!("删除远端目录 {path} 失败: {e}")))?;
    } else {
        sftp.remove_file(&path)
            .await
            .map_err(|e| AppError::ESftpIo(format!("删除远端文件 {path} 失败: {e}")))?;
    }
    crate::events::emit(
        &state.app,
        crate::events::SFTP_CHANGED,
        crate::events::SftpChangedPayload {
            session_id,
            side: "remote".to_string(),
            path: parent,
        },
    );
    Ok(())
}

/// 重命名远端条目；目标已存在时拒绝，不覆盖。
#[tauri::command]
pub async fn sftp_rename(
    state: tauri::State<'_, AppState>,
    session_id: String,
    path: String,
    new_name: String,
) -> AppResult<String> {
    let (new_path, parent) = remote_rename_target(&path, &new_name)?;
    // 同名：无需访问远端，直接返回原路径（避免把自身当成「目标已存在」）。
    if new_path == path.trim_end_matches('/') {
        return Ok(new_path);
    }
    let sftp = state.sftp.get(&state.ssh, &state.app, &session_id).await?;
    if remote_target_exists(&sftp, &new_path).await? {
        return Err(AppError::EInvalidInput(format!(
            "目标已存在：{new_path}（不会覆盖）"
        )));
    }
    sftp.rename(&path, &new_path)
        .await
        .map_err(|e| AppError::ESftpIo(format!("重命名 {path} 为 {new_path} 失败: {e}")))?;
    crate::events::emit(
        &state.app,
        crate::events::SFTP_CHANGED,
        crate::events::SftpChangedPayload {
            session_id,
            side: "remote".to_string(),
            path: parent,
        },
    );
    Ok(new_path)
}

/// 取消进行中的传输；transferId 不存在时返回 E_NOT_FOUND。
#[tauri::command]
pub async fn sftp_cancel(state: tauri::State<'_, AppState>, transfer_id: String) -> AppResult<()> {
    let mut map = state.transfers.lock().await;
    match map.remove(&transfer_id) {
        Some(cancel) => {
            cancel.cancel();
            Ok(())
        }
        None => Err(AppError::ENotFound(format!("传输 {transfer_id} 不存在"))),
    }
}

fn events_app_error(app: &tauri::AppHandle, e: &AppError) {
    use crate::events::AppErrorPayload;
    let json = serde_json::to_value(e).ok();
    let (code, detail) = match json {
        Some(serde_json::Value::Object(m)) => (
            m.get("code")
                .and_then(|c| c.as_str())
                .unwrap_or("E_SFTP_IO")
                .to_string(),
            m.get("detail")
                .and_then(|d| d.as_str())
                .unwrap_or_default()
                .to_string(),
        ),
        _ => ("E_SFTP_IO".to_string(), e.to_string()),
    };
    crate::events::emit(
        app,
        crate::events::APP_ERROR,
        AppErrorPayload {
            code,
            detail,
            session_id: None,
        },
    );
}

// ---------------- Rust 单测 ----------------

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("ruthenium-rename-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn local_rename_success_returns_new_full_path() {
        let dir = temp_dir("ok");
        let old = dir.join("a.txt");
        std::fs::write(&old, b"hello").unwrap();
        let new_path = local_rename_path(&old.to_string_lossy(), "b.txt").unwrap();
        assert_eq!(std::path::PathBuf::from(&new_path), dir.join("b.txt"));
        assert!(!old.exists(), "旧路径不应再存在");
        assert_eq!(std::fs::read_to_string(&new_path).unwrap(), "hello");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn local_rename_refuses_existing_target_without_overwrite() {
        let dir = temp_dir("exists");
        let old = dir.join("a.txt");
        let target = dir.join("b.txt");
        std::fs::write(&old, b"old").unwrap();
        std::fs::write(&target, b"keep").unwrap();

        let err = local_rename_path(&old.to_string_lossy(), "b.txt").unwrap_err();
        assert!(
            matches!(err, AppError::EInvalidInput(_)),
            "错误码应为 E_INVALID_INPUT: {err:?}"
        );
        let text = format!("{err:?}");
        assert!(text.contains("目标已存在"), "文案应说明目标已存在: {text}");
        assert_eq!(
            std::fs::read_to_string(&old).unwrap(),
            "old",
            "源文件不得被改动"
        );
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "keep",
            "目标文件不得被覆盖"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rename_rejects_invalid_new_names() {
        let dir = temp_dir("invalid");
        let old = dir.join("a.txt");
        std::fs::write(&old, b"x").unwrap();
        let old_str = old.to_string_lossy().to_string();

        for bad in ["", "   ", "a/b", "a\\b", ".", ".."] {
            let err = local_rename_path(&old_str, bad).unwrap_err();
            assert!(
                matches!(err, AppError::EInvalidInput(_)),
                "非法名字 {bad:?} 应返回 E_INVALID_INPUT: {err:?}"
            );
            assert!(
                remote_rename_target("/home/deploy/a.txt", bad).is_err(),
                "非法名字 {bad:?} 同样应被远端校验拒绝"
            );
        }
        assert!(old.exists(), "非法重命名不得改动源文件");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Windows 上 `a.txt` → `A.txt` 这类仅大小写的重命名必须成功，
    /// 不能被"目标已存在"（大小写不敏感查找命中自身）误拦。
    #[cfg(windows)]
    #[test]
    fn local_rename_allows_case_only_change() {
        let dir = temp_dir("case-only");
        let old = dir.join("a.txt");
        std::fs::write(&old, b"case").unwrap();
        let new_path =
            local_rename_path(&old.to_string_lossy(), "A.txt").expect("仅大小写的重命名应成功");
        assert_eq!(
            std::path::Path::new(&new_path)
                .file_name()
                .unwrap()
                .to_string_lossy(),
            "A.txt"
        );
        // 不得产生副本：目录内仍然只有一个文件
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        assert_eq!(std::fs::read_to_string(dir.join("A.txt")).unwrap(), "case");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 新建目录必须与重命名共用文件名校验，不允许 `..` / 分隔符 / 绝对路径
    /// 把目录建到目标目录之外。
    #[tokio::test]
    async fn mkdir_rejects_traversal_and_creates_nothing_outside() {
        let dir = temp_dir("mkdir-escape");
        let outside_name = format!("ruthenium-escape-{}", std::process::id());
        let outside = dir.parent().unwrap().join(&outside_name);
        let _ = std::fs::remove_dir_all(&outside);

        for bad in ["../", ".", "..", "a/b", "a\\b"] {
            let err = local_mkdir(dir.to_string_lossy().to_string(), bad.to_string())
                .await
                .unwrap_err();
            assert!(
                matches!(err, AppError::EInvalidInput(_)),
                "非法目录名 {bad:?} 应返回 E_INVALID_INPUT，实际: {err:?}"
            );
        }
        // `../<outside>` 这类真正的穿越尝试同样被拒，且外部目录不得出现
        let err = local_mkdir(
            dir.to_string_lossy().to_string(),
            format!("../{outside_name}"),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, AppError::EInvalidInput(_)), "{err:?}");
        assert!(!outside.exists(), "不得在目标目录之外创建目录");
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            0,
            "拒绝时不得创建任何条目"
        );

        // 合法名字照常创建
        local_mkdir(dir.to_string_lossy().to_string(), "sub".to_string())
            .await
            .expect("合法目录名应创建成功");
        assert!(dir.join("sub").is_dir());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remote_rename_target_keeps_posix_parent() {
        assert_eq!(
            remote_rename_target("/home/deploy/a.txt", "b.txt").unwrap(),
            ("/home/deploy/b.txt".to_string(), "/home/deploy".to_string())
        );
        assert_eq!(
            remote_rename_target("/a.txt", "b.txt").unwrap(),
            ("/b.txt".to_string(), "/".to_string())
        );
        assert_eq!(
            remote_rename_target("/home/deploy/dir/", "dir2").unwrap(),
            ("/home/deploy/dir2".to_string(), "/home/deploy".to_string())
        );
        assert!(matches!(
            remote_rename_target("/", "x"),
            Err(AppError::EInvalidInput(_))
        ));
        assert!(matches!(
            remote_rename_target("relative.txt", "x"),
            Err(AppError::EInvalidInput(_))
        ));
    }
}

#[cfg(test)]
mod local_roots_tests {
    use super::local_list;

    /// 空路径 = 「此电脑」：必须返回可用盘符，而不是 E_INVALID_INPUT。
    #[tokio::test]
    async fn empty_path_lists_drives() {
        let entries = local_list(String::new())
            .await
            .expect("空路径应返回盘符列表");
        assert!(!entries.is_empty(), "至少应有一个盘符");
        assert!(entries.iter().all(|e| e.is_dir), "盘符应标记为目录");
        #[cfg(windows)]
        assert!(
            entries.iter().all(|e| e.name.ends_with(':')),
            "Windows 盘符形如 C:，实际: {:?}",
            entries.iter().map(|e| &e.name).collect::<Vec<_>>()
        );
    }
}
