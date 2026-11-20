//! SftpManager：按 sessionId 缓存独立 SFTP 连接（复用同一 SSH 连接上的新 channel）。
pub mod transfer;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use russh_sftp::client::SftpSession;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::error::{AppError, AppResult};
use crate::models::{FileEntry, SessionKind};
use crate::ssh::SshManager;

/// 独立 SFTP 会话（无终端 reader）的健康探测参数。
const PROBE_INTERVAL: Duration = Duration::from_secs(15);
/// 单次探测的超时：死通道上 canonicalize 要等 russh-sftp 默认超时，先自己掐断。
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// 连续失败多少次判定连接已死（网络断 / 服务器重启）。
const PROBE_FAILURE_LIMIT: u32 = 2;

/// SFTP 会话缓存：按 sessionId 复用已建立的 SftpSession（各自复用底层 SSH 连接）。
/// `monitors`：独立 SFTP 会话的健康探测任务表，随 drop_session 取消。
#[derive(Default)]
pub struct SftpManager {
    sessions: Arc<Mutex<HashMap<String, Arc<SftpSession>>>>,
    monitors: Arc<Mutex<HashMap<String, CancellationToken>>>,
}

impl SftpManager {
    /// 创建空的 SFTP 会话缓存。
    pub fn new() -> Self {
        Self::default()
    }

    /// 取已有缓存，或基于该会话的 SSH 连接开新 channel 建立 SFTP。
    ///
    /// 并发首次调用存在 check-then-act 竞态——两个调用都会开通道，后插入者把
    /// 先插入的 `SftpSession` 顶掉（旧对象随 Arc 释放关闭），表现为"多开一条 SFTP
    /// 通道 + 调用方拿到不同实例"。这里在插入锁内二次检查：若已有实例则复用并丢弃
    /// 自己刚建的那条（Arc drop 会关闭它），保证同一 sessionId 永远只暴露一个实例。
    ///
    /// 对**独立 SFTP 会话**（kind=Sftp 且拥有底层连接、无终端 reader）启动
    /// 健康探测——keepalive 断开后没有任何回调能感知，`connected` 会永真。
    pub async fn get(
        &self,
        ssh: &Arc<SshManager>,
        app: &tauri::AppHandle,
        session_id: &str,
    ) -> AppResult<Arc<SftpSession>> {
        let sftp = {
            let map = self.sessions.lock().await;
            if let Some(s) = map.get(session_id) {
                s.clone()
            } else {
                drop(map);
                let stream = ssh.open_subsystem_channel(session_id).await?;
                let sftp = Arc::new(
                    SftpSession::new(stream)
                        .await
                        .map_err(|e| AppError::ESftpIo(format!("建立 SFTP 连接失败: {e}")))?,
                );
                let mut map = self.sessions.lock().await;
                if let Some(existing) = map.get(session_id) {
                    // 竞态落败：复用先到者的实例；自己这条随 Arc drop 关闭。
                    existing.clone()
                } else {
                    map.insert(session_id.to_string(), sftp.clone());
                    sftp
                }
            }
        };
        // 独立 SFTP 会话起一次健康探测（幂等；派生/终端会话由 reader 负责断线）。
        let sftp_only = ssh
            .get(session_id)
            .await
            .map(|live| live.inner.kind == SessionKind::Sftp && live.owns_connection)
            .unwrap_or(false);
        if sftp_only {
            self.ensure_monitor(Arc::clone(ssh), app.clone(), session_id, Arc::clone(&sftp))
                .await;
        }
        Ok(sftp)
    }

    /// 为独立 SFTP 会话启动健康探测任务（已存在则 no-op）。任务持有缓存实例的克隆，
    /// 连续 [`PROBE_FAILURE_LIMIT`] 次探测失败后按"断线"做全量清理。
    async fn ensure_monitor(
        &self,
        ssh: Arc<SshManager>,
        app: tauri::AppHandle,
        session_id: &str,
        sftp: Arc<SftpSession>,
    ) {
        {
            let monitors = self.monitors.lock().await;
            if monitors.contains_key(session_id) {
                return;
            }
        }
        let cancel = CancellationToken::new();
        self.monitors
            .lock()
            .await
            .insert(session_id.to_string(), cancel.clone());

        let sid = session_id.to_string();
        let sessions = Arc::clone(&self.sessions);
        let monitors = Arc::clone(&self.monitors);
        tokio::spawn(async move {
            let mut failures = 0u32;
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = tokio::time::sleep(PROBE_INTERVAL) => {
                        match tokio::time::timeout(PROBE_TIMEOUT, sftp.canonicalize(".")).await {
                            Ok(Ok(_)) => failures = 0,
                            Ok(Err(_)) | Err(_) => {
                                failures += 1;
                                if failures >= PROBE_FAILURE_LIMIT {
                                    break;
                                }
                            }
                        }
                    }
                }
            }
            monitors.lock().await.remove(&sid);
            if cancel.is_cancelled() {
                return; // drop_session / 关闭路径取消：清理由调用方负责
            }
            // 连续探测失败：连接已死，按断线做全量清理（与 term reader 的级联一致）。
            applog!("[sftp] 独立 SFTP 会话 {sid} 探测连续失败，按断线清理");
            use tauri::Manager;
            let state = app.state::<crate::state::AppState>();
            let mut host_id = String::new();
            let mut owns = false;
            if let Ok(live) = ssh.get(&sid).await {
                host_id = live.inner.host_id.clone();
                owns = live.owns_connection;
                ssh.mark_host_disconnected(&host_id).await;
            }
            let _ = ssh.close(&sid).await;
            if let Some(s) = sessions.lock().await.remove(&sid) {
                let _ = s.close().await;
            }
            if owns && !host_id.is_empty() {
                crate::commands::tunnel::stop_tunnels_for_host(&state, &host_id).await;
            }
            crate::events::emit(
                &app,
                crate::events::CONN_STATE,
                crate::events::ConnStatePayload {
                    session_id: sid.clone(),
                    state: "disconnected".to_string(),
                    latency_ms: None,
                    reason: Some(crate::events::CONN_REASON_LOST.to_string()),
                },
            );
        });
    }

    /// 缓存一条已建立的 SFTP 客户端（对等层：由调用方先行完成「等回执」判定）。
    pub async fn put(&self, session_id: &str, sftp: Arc<SftpSession>) {
        self.sessions
            .lock()
            .await
            .insert(session_id.to_string(), sftp);
    }

    /// 关闭并移除该会话的 SFTP 客户端（同时取消健康探测任务）；不存在时 no-op。
    pub async fn drop_session(&self, session_id: &str) {
        if let Some(cancel) = self.monitors.lock().await.remove(session_id) {
            cancel.cancel();
        }
        if let Some(s) = self.sessions.lock().await.remove(session_id) {
            let _ = s.close().await;
        }
    }
}

/// 把 SFTP 原始错误翻译成可读中文。
///
/// 直接透传底层 Display 会得到 `No such file: No such file` 这类重复文本
/// （StatusCode 自身就显示 "No such file"，外层又拼了一次），对用户毫无帮助。
fn friendly_sftp_err(path: &str, e: &russh_sftp::client::error::Error) -> String {
    use russh_sftp::client::error::Error as SftpError;
    match e {
        SftpError::Status(st) => friendly_status(path, Some(st.status_code)),
        SftpError::Timeout => format!("读取目录 {path} 超时"),
        SftpError::IO(_) => format!("读取目录 {path} 失败：连接中断"),
        other => format!("读取目录 {path} 失败：{other}"),
    }
}

/// 状态码 → 可读文案（单独拆出来便于单测；不再堆叠底层重复文本）。
fn friendly_status(path: &str, code: Option<russh_sftp::protocol::StatusCode>) -> String {
    use russh_sftp::protocol::StatusCode;
    match code {
        Some(StatusCode::NoSuchFile) => format!("目录不存在：{path}"),
        Some(StatusCode::PermissionDenied) => format!("没有权限访问：{path}"),
        Some(StatusCode::NoConnection) | Some(StatusCode::ConnectionLost) => {
            format!("读取目录 {path} 失败：连接已断开")
        }
        Some(StatusCode::Failure) | None => format!("读取目录 {path} 失败（服务器拒绝）"),
        Some(c) => format!("读取目录 {path} 失败（{c:?}）"),
    }
}

/// 列目录：目录在前，按名称排序。
pub async fn list_dir(sftp: &SftpSession, path: &str) -> AppResult<Vec<FileEntry>> {
    let read = sftp
        .read_dir(path)
        .await
        .map_err(|e| AppError::ESftpIo(friendly_sftp_err(path, &e)))?;
    let mut entries: Vec<FileEntry> = read
        .into_iter()
        .map(|e| {
            let meta = e.metadata();
            FileEntry {
                name: e.file_name(),
                is_dir: matches!(meta.file_type(), russh_sftp::protocol::FileType::Dir),
                size: meta.size,
                modified: meta.mtime.map(|m| m as i64),
                mode: meta.permissions,
            }
        })
        .collect();
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(entries)
}

/// 远端默认目录：SFTP 会话的起始目录（通常是登录用户 home，如 /root 或 /home/xxx）。
///
/// 早期版本前端写死了 `/home/deploy/app`，在非该路径的机器上一点 FTP 就报
/// 「目录不存在」。现在由后端向服务器查询真实 home，前端只用这个结果。
pub async fn home_dir(sftp: &SftpSession) -> AppResult<String> {
    match sftp.canonicalize(".").await {
        Ok(p) if !p.trim().is_empty() => Ok(p),
        _ => Ok("/".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::friendly_status;
    use russh_sftp::protocol::StatusCode;

    #[test]
    fn friendly_status_messages_are_readable() {
        // 不能再出现「No such file: No such file」这种重复文本
        let m = friendly_status("/home/deploy/app", Some(StatusCode::NoSuchFile));
        assert_eq!(m, "目录不存在：/home/deploy/app");
        assert!(!m.contains("No such file"));
        assert_eq!(
            friendly_status("/root", Some(StatusCode::PermissionDenied)),
            "没有权限访问：/root"
        );
        assert_eq!(
            friendly_status("/root", Some(StatusCode::ConnectionLost)),
            "读取目录 /root 失败：连接已断开"
        );
        assert!(friendly_status("/root", Some(StatusCode::Failure)).contains("服务器拒绝"));
    }
}
