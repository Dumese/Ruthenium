//! -L 本地转发：TcpListener → channel_open_direct_tcpip → copy_bidirectional。
//! 绑定失败 E_TUNNEL_BIND；会话关闭释放监听。
use std::collections::HashMap;
use std::sync::Arc;

use tokio::io::copy_bidirectional;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::error::{AppError, AppResult};
use crate::events::{self, TunnelStatePayload};

/// 每条规则同时服务的 relay 连接上限——绑定 0.0.0.0 的规则可被局域网
/// 连接耗尽任务/通道，超限的连接直接关闭（与 sshd MaxSessions 的防御思路一致）。
const MAX_RELAYS_PER_RULE: usize = 128;

struct ActiveTunnel {
    cancel: CancellationToken,
    /// accept 循环句柄：停止时必须等它退出，否则同一端口会在 stop→start
    /// 的窗口里仍被旧 listener 占用（Windows 上直接 E_TUNNEL_BIND）。
    listener_task: Option<tokio::task::JoinHandle<()>>,
    /// 每规则并发 relay 上限。
    relay_permits: Arc<Semaphore>,
}

/// 活跃转发监听表：ruleId → 取消令牌；停止即 cancel 对应的 accept 循环。
/// 表本身用 Arc 共享：accept 循环异常退出时要反向清理自己的条目（防僵尸规则）。
///
/// `start_lock` 串行化「is_active 检查 → bind → insert」全程——没有互斥时，
/// 同一条规则的并发 upsert/toggle 会双双通过 is_active 检查、各自 bind
/// （后者必然 AddressInUse 报错）并各起一个 accept 任务；先入表者若随后被 stop，
/// 后入表者留下的监听任务在 cancel 令牌上永远等不到通知（任务泄漏）。
/// 用 tokio::sync::Mutex 而非 std：start 是 async fn，持锁期间要 await bind。
#[derive(Default)]
pub struct TunnelManager {
    active: Arc<std::sync::Mutex<HashMap<String, ActiveTunnel>>>,
    start_lock: tokio::sync::Mutex<()>,
}

impl TunnelManager {
    /// 创建空的转发监听表。
    pub fn new() -> Self {
        Self::default()
    }

    /// 取活动表锁：**不让 poison 传染**。
    /// accept 循环与命令层都会操作该表；任一处 panic 后若继续 `unwrap()`，
    /// 关闭会话/删除主机等路径会连锁 panic，规则再也停不下来。
    fn lock_active(&self) -> std::sync::MutexGuard<'_, HashMap<String, ActiveTunnel>> {
        self.active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 该规则是否已有活跃监听。
    pub fn is_active(&self, rule_id: &str) -> bool {
        self.lock_active().contains_key(rule_id)
    }

    /// 启动一条 -L 规则。绑定失败返回 E_TUNNEL_BIND。
    ///
    /// 全程持 `start_lock`——is_active 早退、bind、insert 三个步骤必须原子，
    /// 否则同规则并发 start 会双重 bind / 泄漏 accept 任务（见结构体注释）。
    /// 锁只覆盖启动序列本身，accept 循环在 spawn 后即脱离锁的作用域。
    /// 泛型 `R: Runtime`：生产是 Wry，单测可用 `tauri::test::mock_app()` 的 MockRuntime。
    pub async fn start<R: tauri::Runtime>(
        &self,
        ssh: Arc<crate::ssh::SshManager>,
        app: &tauri::AppHandle<R>,
        rule_id: &str,
        local: &str,
        remote: &str,
        session_id: &str,
    ) -> AppResult<()> {
        let _start_guard = self.start_lock.lock().await;
        if self.is_active(rule_id) {
            return Ok(());
        }
        let listener = tokio::net::TcpListener::bind(local)
            .await
            .map_err(|e| AppError::ETunnelBind(format!("绑定 {local} 失败: {e}")))?;
        let cancel = CancellationToken::new();

        let (remote_host, remote_port) = split_addr(remote)
            .ok_or_else(|| AppError::EInvalidInput(format!("远端地址无效: {remote}")))?;
        let rule = rule_id.to_string();
        let app2 = app.clone();
        let session = session_id.to_string();
        let cancel_task = cancel.clone();
        // accept 循环异常退出时需要把自己从 active 表摘除，否则 is_active 永真，
        // start 早退导致规则永远无法重新启动（僵尸规则）。
        let active_map = self.active.clone();
        let rule_on_err = rule.clone();
        let app_on_err = app2.clone();
        let relay_permits = Arc::new(Semaphore::new(MAX_RELAYS_PER_RULE));
        let permits_for_accept = Arc::clone(&relay_permits);
        let listener_task = tokio::spawn(async move {
            loop {
                let accept = tokio::select! {
                    _ = cancel_task.cancelled() => break,
                    a = listener.accept() => a,
                };
                let (tcp, _peer) = match accept {
                    Ok(x) => x,
                    Err(e) => {
                        // accept 循环死亡（非 cancel 退出）：清理 active 表 + 通知前端，
                        // 规则回到「已停止」状态，之后可以重新 start。
                        active_map
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .remove(&rule_on_err);
                        applog!("[tunnel] {rule_on_err} accept 失败，转发已停止: {e}");
                        events::emit(
                            &app_on_err,
                            events::TUNNEL_STATE,
                            TunnelStatePayload {
                                rule_id: rule_on_err.clone(),
                                active: false,
                                error: Some(e.to_string()),
                            },
                        );
                        break;
                    }
                };
                // RFC 4254 originator port = 发起连接的对端端口，不是本地端口。
                let origin_port = tcp.peer_addr().map_or(0, |a| a.port());
                let remote_host = remote_host.clone();
                let session = session.clone();
                let rule2 = rule.clone();
                let ssh3 = ssh.clone();
                let mut tcp = tcp;
                let permits = Arc::clone(&permits_for_accept);
                tokio::spawn(async move {
                    // 并发 relay 上限。用 try_acquire 而不是无限等待，
                    // 否则攻击者可以在端口上堆积大量 pending 任务/FD 而不受 cap 限制。
                    let _permit = match permits.try_acquire_owned() {
                        Ok(p) => p,
                        Err(_) => return,
                    };
                    let mut stream = match ssh3
                        .open_direct_tcpip(&session, &remote_host, remote_port, origin_port as u32)
                        .await
                    {
                        Ok(s) => s,
                        Err(e) => {
                            // 单个连接失败（远端拒连等瞬时问题）不代表规则失效：
                            // 只记日志、丢弃该连接，accept 循环继续服务下一条。
                            // 规则仍在服务，不发 active:false 状态事件。
                            applog!("[tunnel] {rule2} direct-tcpip 失败: {e}");
                            return;
                        }
                    };
                    let _ = copy_bidirectional(&mut tcp, &mut stream).await;
                });
            }
        });

        self.lock_active().insert(
            rule_id.to_string(),
            ActiveTunnel {
                cancel,
                listener_task: Some(listener_task),
                relay_permits,
            },
        );
        events::emit(
            app,
            events::TUNNEL_STATE,
            TunnelStatePayload {
                rule_id: rule_id.to_string(),
                active: true,
                error: None,
            },
        );
        Ok(())
    }

    /// 停止一条规则的监听；不存在时 no-op。
    /// 同时关闭 relay 信号量，并等待 accept 循环退出，确保端口在返回后已释放
    /// （stop → 立即 start 时不会撞上旧的 listener）。
    pub async fn stop(&self, rule_id: &str) {
        let Some(t) = self.lock_active().remove(rule_id) else {
            return;
        };
        t.relay_permits.close();
        t.cancel.cancel();
        if let Some(task) = t.listener_task {
            let _ = task.await;
        }
    }
}

/// 解析 "host:port"；IPv6 字面量用方括号包裹，形如 "[::1]:22"。
/// 方括号形式：host 取 `]` 之前的内容（去括号），port 取 `]` 之后的 `:port`；
/// 其余维持 rfind(':') 逻辑（IPv4 / 域名没有额外冒号歧义）。
/// 端口按 u32 解析后必须落在 1..=65535——u32 允许 70000 之类保存成功、
/// 到 direct-tcpip 才失败的“延迟报错”。
pub fn split_addr(addr: &str) -> Option<(String, u32)> {
    if let Some(rest) = addr.strip_prefix('[') {
        let close = rest.find(']')?;
        let host = &rest[..close];
        let port: u32 = rest[close + 1..].strip_prefix(':')?.parse().ok()?;
        if host.is_empty() || port == 0 || port > 65535 {
            return None;
        }
        return Some((host.to_string(), port));
    }
    let idx = addr.rfind(':')?;
    let host = &addr[..idx];
    let port: u32 = addr[idx + 1..].parse().ok()?;
    if host.is_empty() || port == 0 || port > 65535 {
        return None;
    }
    Some((host.to_string(), port))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ssh::SshManager;

    #[test]
    fn splits_host_port() {
        assert_eq!(
            split_addr("127.0.0.1:5432"),
            Some(("127.0.0.1".to_string(), 5432))
        );
        assert_eq!(split_addr("no-port"), None);
        assert_eq!(split_addr("h:0"), None);
    }

    /// 端口越界（>65535）必须拒绝，不能保存成功、direct-tcpip 才失败。
    #[test]
    fn rejects_out_of_range_ports() {
        assert_eq!(split_addr("h:65535"), Some(("h".to_string(), 65535)));
        assert_eq!(split_addr("h:65536"), None);
        assert_eq!(split_addr("h:70000"), None);
        assert_eq!(split_addr("[::1]:70000"), None);
    }

    /// IPv6 字面量：方括号包裹 host，port 取 `]` 之后（rfind(':') 会切错）。
    #[test]
    fn splits_ipv6_literal() {
        assert_eq!(split_addr("[::1]:22"), Some(("::1".to_string(), 22)));
        assert_eq!(
            split_addr("[2001:db8::1]:8443"),
            Some(("2001:db8::1".to_string(), 8443))
        );
        // 缺端口 / 空 host / 缺右括号均无效
        assert_eq!(split_addr("[::1]"), None);
        assert_eq!(split_addr("[]:22"), None);
        assert_eq!(split_addr("[::1:22"), None);
        assert_eq!(split_addr("[::1]:0"), None);
    }

    /// 同一条规则的并发 start 不得双重 bind / 双跑 accept 任务。
    ///
    /// 「is_active 检查 → bind → insert」若无互斥，两个并发 start 都通过
    /// is_active 检查后各自 bind 同一本地端口，第二个必然 E_TUNNEL_BIND 报错
    /// （用户看到莫名的“绑定失败”）。加锁后第二个 start 走 is_active 早退，两个都 Ok。
    #[tokio::test]
    async fn concurrent_start_of_same_rule_is_serialized() {
        let app = tauri::test::mock_app();
        let handle = app.handle().clone();
        let manager = std::sync::Arc::new(TunnelManager::new());
        let ssh = std::sync::Arc::new(SshManager::new());
        // 端口 0 交给系统分配，避免与测试机上既有监听冲突。
        let local = "127.0.0.1:0";

        let mut tasks = Vec::new();
        for _ in 0..4 {
            let ssh = ssh.clone();
            let handle = handle.clone();
            let manager = manager.clone();
            // 同一 rule_id、同一 local 地址并发启动（模拟并发 invoke）
            tasks.push(tokio::spawn(async move {
                manager
                    .start(ssh, &handle, "r1", local, "127.0.0.1:1", "sess-none")
                    .await
            }));
        }
        let mut all_ok = true;
        for t in tasks {
            match t.await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    all_ok = false;
                    eprintln!("并发 start 出现错误: {e}");
                }
                Err(e) => {
                    all_ok = false;
                    eprintln!("任务 join 失败: {e}");
                }
            }
        }
        assert!(
            all_ok,
            "同规则并发 start 必须全部成功（第二个走 is_active 早退）"
        );
        // 只有一个 accept 任务存活；stop 一次即彻底回收。
        assert!(manager.is_active("r1"));
        manager.stop("r1").await;
        assert!(!manager.is_active("r1"));
    }
}
