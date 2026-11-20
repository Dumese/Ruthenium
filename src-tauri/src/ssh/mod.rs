//! SshManager + LiveSession（含 SessionHandle）。
pub mod auth;
pub mod channel;
pub mod client;
pub mod known_hosts;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::error::{AppError, AppResult};
use crate::models::{Session, SessionHandle, SessionKind};
use crate::ssh::channel::ChannelCmd;
use crate::ssh::client::ClientHandler;
use crate::state::PendingHostKeyMap;

/// 一个已建立的 SSH 会话：SessionHandle + russh 连接句柄 + 控制通道。
pub struct LiveSession {
    pub inner: SessionHandle,
    pub connected: Arc<AtomicBool>,
    /// 与其他会话共享的 SSH 连接句柄。SFTP 复用终端连接时持有同一个 `Arc`。
    pub russh: Arc<Mutex<russh::client::Handle<ClientHandler>>>,
    pub ctrl: mpsc::Sender<ChannelCmd>,
    /// 是否拥有底层 SSH 连接：终端 / 独立 SFTP 会话为 true；
    /// 复用终端连接派生的 SFTP 会话为 false，关闭时不得断开连接或释放隧道。
    pub owns_connection: bool,
    /// 建连时的主机身份快照：SFTP 复用 / 隧道绑定前与当前配置比较，
    /// 不一致（编辑主机改了 addr/port/user）则不复用——否则新开的 SFTP / 转发
    /// 会静默复用“旧地址”的连接，把数据读写到错误的服务器。
    pub addr: String,
    pub port: u16,
    pub user: String,
    /// 最近一次窗口尺寸：重连时沿用，避免先按默认尺寸建 PTY 再被前端 fit 拉回。
    pub cols: std::sync::atomic::AtomicU16,
    pub rows: std::sync::atomic::AtomicU16,
}

impl LiveSession {
    /// 会话建立时的身份是否仍与当前主机配置一致（addr/port/user 都没变）。
    pub fn identity_matches(&self, host: &crate::models::Host) -> bool {
        self.addr == host.addr && self.port == host.port && self.user == host.user
    }
}

/// SSH 会话表：sessionId → LiveSession；SFTP 复用终端连接时共享同一个 `russh` 句柄。
#[derive(Default)]
pub struct SshManager {
    sessions: Mutex<HashMap<String, Arc<LiveSession>>>,
}

impl SshManager {
    /// 创建空的 SSH 会话表。
    pub fn new() -> Self {
        Self::default()
    }

    /// 取会话句柄；会话不存在返回 E_NOT_FOUND。
    pub async fn get(&self, session_id: &str) -> AppResult<Arc<LiveSession>> {
        self.sessions
            .lock()
            .await
            .get(session_id)
            .cloned()
            .ok_or_else(|| AppError::ENotFound(format!("会话 {session_id} 不存在")))
    }

    /// 插入（或覆盖）一条会话记录。
    pub async fn insert(&self, live: Arc<LiveSession>) {
        self.sessions
            .lock()
            .await
            .insert(live.inner.id.clone(), live);
    }

    /// 会话是否存在（commands 层用于 SSH → 本地双查，避免用 Err 区分）。
    pub async fn has(&self, session_id: &str) -> bool {
        self.sessions.lock().await.contains_key(session_id)
    }

    /// 合并所有 SSH 会话为可序列化摘要（不含本地会话）。
    ///
    /// 锁序说明（信息性）：持 sessions 锁期间会逐个 await 各会话的 encoding 锁。
    /// 全仓不存在反向（持 encoding 取 sessions）路径，无死锁风险；代价只是 list 期间
    /// 短暂多等。如需优化可先克隆 `Arc<LiveSession>` 列表再逐个取 encoding。
    pub async fn list(&self) -> Vec<Session> {
        let mut out = Vec::new();
        for live in self.sessions.lock().await.values() {
            let encoding = live.inner.encoding.lock().await.name.clone();
            out.push(Session {
                id: live.inner.id.clone(),
                host_id: live.inner.host_id.clone(),
                kind: live.inner.kind,
                connected: live.connected.load(Ordering::Relaxed),
                started_at: live.inner.started_at,
                encoding,
            });
        }
        out
    }

    /// 返回某主机当前配置下活跃且**拥有底层连接**的会话 id（用于隧道拉起）。
    /// 隧道必须绑定拥有连接的会话：派生 SFTP 会话只是共享同一连接的旁观者，
    /// 绑到它们上面会在拥有者关闭后悬挂。
    /// 必须校验身份快照——编辑主机（改 addr/port/user）后，旧会话虽然
    /// host_id 相同，但连的是旧服务器，隧道绑上去会把流量转发到错误目标。
    pub async fn connected_session_for_host(&self, host: &crate::models::Host) -> Option<String> {
        let map = self.sessions.lock().await;
        let matches = |live: &&Arc<LiveSession>| {
            live.inner.host_id == host.id
                && live.owns_connection
                && live.connected.load(Ordering::Relaxed)
                && live.identity_matches(host)
        };
        // 终端会话优先：独立 SFTP 也能承载隧道，但关闭 SFTP 标签不应把
        // 仍存活的终端上的转发一起停掉。
        map.values()
            .find(|live| matches(live) && live.inner.kind == SessionKind::Term)
            .or_else(|| map.values().find(matches))
            .map(|live| live.inner.id.clone())
    }

    /// 同一 host 已有活动终端会话时，复用其 SSH 连接派生一个 SFTP 会话，
    /// 只开新 channel，**不重新握手 / 重新认证**。
    /// 返回新会话 id；没有活动终端会话时返回 None（调用方走完整建连 + 认证）。
    /// 只复用身份与当前配置一致的终端会话（改过地址的旧会话不复用）。
    pub async fn reuse_sftp_session(&self, host: &crate::models::Host) -> Option<String> {
        let source = {
            let map = self.sessions.lock().await;
            map.values()
                .find(|live| {
                    live.inner.host_id == host.id
                        && live.inner.kind == SessionKind::Term
                        && live.connected.load(Ordering::Relaxed)
                        && live.identity_matches(host)
                })
                .cloned()?
        };
        let session_id = new_session_id();
        let (inner, _writer_rx) =
            make_session_handle(session_id.clone(), host.id.clone(), SessionKind::Sftp);
        // SFTP 会话没有终端输入 / 窗口变更需求：保留一个已断开接收端的控制通道，
        // 使 close / resize 的发送失败成为无害 no-op。
        let (ctrl, _ctrl_rx) = mpsc::channel::<ChannelCmd>(1);
        let live = Arc::new(LiveSession {
            inner,
            connected: Arc::new(AtomicBool::new(true)),
            russh: source.russh.clone(),
            ctrl,
            owns_connection: false,
            // 身份快照沿用拥有者的（同一连接、同一目标）
            addr: source.addr.clone(),
            port: source.port,
            user: source.user.clone(),
            // SFTP 没有 PTY，尺寸仅作占位（resize 会被 resize_terminal 的 kind 守卫拦下）。
            cols: std::sync::atomic::AtomicU16::new(0),
            rows: std::sync::atomic::AtomicU16::new(0),
        });
        self.insert(live).await;
        applog!(
            "[ssh] sftp 会话 {session_id} 复用终端会话 {} 的连接（host={}，不重新认证）",
            source.inner.id,
            host.id
        );
        Some(session_id)
    }

    /// 查找某设备/主机下活动且拥有连接的指定类型会话（对端 SFTP 复用）。
    pub async fn find_connected(&self, host_id: &str, kind: SessionKind) -> Option<String> {
        self.sessions
            .lock()
            .await
            .values()
            .find(|live| {
                live.inner.host_id == host_id
                    && live.inner.kind == kind
                    && live.connected.load(Ordering::Relaxed)
                    && live.owns_connection
            })
            .map(|live| live.inner.id.clone())
    }

    /// 该会话是否拥有底层 SSH 连接（关闭时决定是否释放该主机的隧道）。
    pub async fn owns_connection(&self, session_id: &str) -> bool {
        self.sessions
            .lock()
            .await
            .get(session_id)
            .is_some_and(|live| live.owns_connection)
    }

    /// 关闭会话：cancel token + 移除映射。返回 host_id（供释放隧道）。
    pub async fn close(&self, session_id: &str) -> AppResult<String> {
        let live = self.get(session_id).await?;
        live.inner.cancel.cancel();
        live.connected.store(false, Ordering::Relaxed);
        // 只做 best-effort 唤醒：writer 可能正卡在 data_bytes 背压里且 ctrl 队列已满，
        // 用阻塞 send 会把关标签流程一起挂死（try_send 忽略 Full/Closed 即可）。
        let _ = live.ctrl.try_send(ChannelCmd::Resize(1, 1));
        // 让 writer task 在 cancel 后自行退出；这里直接移除映射并 Drop handle。
        self.sessions.lock().await.remove(session_id);
        // sftp 缓存由调用方（commands::session::close_session）清理。
        Ok(live.inner.host_id.clone())
    }

    /// 按谓词批量移除会话并取消它们，返回被移除的 sessionId。
    pub async fn remove_if<F>(&self, f: F) -> Vec<String>
    where
        F: Fn(&LiveSession) -> bool,
    {
        let mut map = self.sessions.lock().await;
        let ids: Vec<String> = map
            .values()
            .filter(|l| f(l))
            .map(|l| l.inner.id.clone())
            .collect();
        for id in &ids {
            if let Some(live) = map.remove(id) {
                live.inner.cancel.cancel();
                live.connected.store(false, Ordering::Relaxed);
            }
        }
        ids
    }

    /// 该 host 全部派生（不拥有连接）的会话 id：拥有者断开 / 关闭时级联清理用。
    pub async fn derived_sessions_of(&self, host_id: &str) -> Vec<String> {
        self.sessions
            .lock()
            .await
            .values()
            .filter(|live| live.inner.host_id == host_id && !live.owns_connection)
            .map(|live| live.inner.id.clone())
            .collect()
    }

    /// 连接断开：把该 host 的全部会话（拥有者与派生）connected 置 false，
    /// 并 cancel **派生会话**的 token——连接已死，派生会话（SFTP subsystem 通道、
    /// 后续 SFTP 操作等待）上的任何挂起都必须终止。
    /// 拥有者自身的 token 不在此处 cancel：其 reader 任务正在收尾，且主动关闭
    /// 路径（close_session）依赖「token 已 cancel ⇒ closed_by_user」的判定语义。
    pub async fn mark_host_disconnected(&self, host_id: &str) {
        let map = self.sessions.lock().await;
        for live in map.values().filter(|l| l.inner.host_id == host_id) {
            live.connected.store(false, Ordering::Relaxed);
            if !live.owns_connection {
                live.inner.cancel.cancel();
            }
        }
    }

    /// 建立一条 SFTP 用的独立 channel（复用同一 SSH 连接）。
    pub async fn open_subsystem_channel(
        &self,
        session_id: &str,
    ) -> AppResult<russh::ChannelStream<russh::client::Msg>> {
        let live = self.get(session_id).await?;
        let handle = live.russh.lock().await;
        let channel = handle
            .channel_open_session()
            .await
            .map_err(|e| AppError::ESshChannel(format!("打开 SFTP 通道失败: {e}")))?;
        channel
            .request_subsystem(false, "sftp")
            .await
            .map_err(|e| AppError::ESshChannel(format!("请求 sftp subsystem 失败: {e}")))?;
        Ok(channel.into_stream())
    }

    /// 打开 direct-tcpip 通道（-L 转发用），返回双向流。
    pub async fn open_direct_tcpip(
        &self,
        session_id: &str,
        target_host: &str,
        target_port: u32,
        origin_port: u32,
    ) -> AppResult<russh::ChannelStream<russh::client::Msg>> {
        let live = self.get(session_id).await?;
        let handle = live.russh.lock().await;
        let channel = handle
            .channel_open_direct_tcpip(target_host, target_port, "127.0.0.1", origin_port)
            .await
            .map_err(|e| AppError::ESshChannel(format!("打开 direct-tcpip 失败: {e}")))?;
        Ok(channel.into_stream())
    }
}

/// 构造 pending hostkey 决策的上下文 + 新 sessionId。
pub fn new_session_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// russh 客户端配置（keepalive）。
///
/// `keepalive_secs > 0` 时启用 russh 内建心跳：无数据收到超过该时长即发 keepalive，
/// 连续 [`KEEPALIVE_MAX`] 次无响应则断开（走现有 `conn://state(disconnected)` 链路）。
/// `0` = 禁用（`keepalive_interval = None`）。
///
/// **不要**改用 channel 层 `inactivity_timeout`：那是「无流量即回收」的语义，
/// 会把空闲但健康的会话杀掉。
pub fn client_config(keepalive_secs: u32) -> Arc<russh::client::Config> {
    let mut config = russh::client::Config::default();
    if keepalive_secs > 0 {
        config.keepalive_interval = Some(std::time::Duration::from_secs(keepalive_secs as u64));
        config.keepalive_max = KEEPALIVE_MAX;
    }
    config.inactivity_timeout = None;
    Arc::new(config)
}

/// 连续多少次 keepalive 无响应后断开：30s × 3。
pub const KEEPALIVE_MAX: usize = 3;

/// 连接阶段硬超时（TCP + 版本交换 + KEX）：对端「TCP 通但 SSH 不回包」时必须能失败，
/// 不能无限挂起（`russh::client::connect` 自身不带任何超时）。
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// 指纹弹窗期间的目标上限：决策窗口（client::HOSTKEY_DECISION_TIMEOUT，120s）+ 余量。
/// 用户正在看指纹弹窗时不能把连接按「网络超时」掐掉。
pub const CONNECT_TIMEOUT_WITH_HOSTKEY: Duration = Duration::from_secs(150);

/// 建立 SSH 连接并施加「分段」超时 + 可选取消。
///
/// 单段 15s 只覆盖正常的 TCP/KEX；一旦发现该会话存在挂起的 hostkey 决策（指纹弹窗
/// 正在等用户点"信任/拒绝"），就把上限放宽到 [`CONNECT_TIMEOUT_WITH_HOSTKEY`]——
/// 决策等待属于用户时间，不应算作网络故障。
/// `cancel` 由命令层传入（用户关闭仍在 connecting 的标签时触发），为 `None` 表示不可取消。
pub async fn connect_client(
    config: Arc<russh::client::Config>,
    addr: (String, u16),
    handler: ClientHandler,
    cancel: Option<&CancellationToken>,
) -> AppResult<russh::client::Handle<ClientHandler>> {
    connect_client_with_timeouts(
        config,
        addr,
        handler,
        CONNECT_TIMEOUT,
        CONNECT_TIMEOUT_WITH_HOSTKEY,
        cancel,
    )
    .await
}

/// 等待可选取消令牌：`None` 时永不就绪（供 `select!` 的"可取消分支"用）。
pub(crate) async fn cancelled(cancel: Option<&CancellationToken>) {
    match cancel {
        Some(token) => token.cancelled().await,
        None => std::future::pending::<()>().await,
    }
}

/// [`connect_client`] 的可注入超时/取消版本（单测用：真实等待 15s 太慢）。
pub(crate) async fn connect_client_with_timeouts(
    config: Arc<russh::client::Config>,
    addr: (String, u16),
    handler: ClientHandler,
    normal_timeout: Duration,
    hostkey_timeout: Duration,
    cancel: Option<&CancellationToken>,
) -> AppResult<russh::client::Handle<ClientHandler>> {
    let session_id = handler.session_id.clone();
    let pending: PendingHostKeyMap = handler.pending.clone();
    let target = format!("{}:{}", addr.0, addr.1);
    let mut connecting = Box::pin(russh::client::connect(
        config,
        (addr.0.as_str(), addr.1),
        handler,
    ));
    let mut deadline = tokio::time::Instant::now() + normal_timeout;
    loop {
        tokio::select! {
            result = &mut connecting => {
                return result
                    .map_err(|e| AppError::ESshConnect(format!("连接 {target} 失败: {e}")));
            }
            _ = cancelled(cancel) => {
                // 用户关了标签。清掉可能已注册的 hostkey 挂起条目，
                // 否则指纹决策表里会留一条永远没人消费的记录（直到 120s 超时）。
                pending.lock().await.remove(&session_id);
                return Err(AppError::ECancelled("连接已取消".to_string()));
            }
            _ = tokio::time::sleep_until(deadline) => {
                if pending.lock().await.contains_key(&session_id) {
                    deadline = tokio::time::Instant::now() + hostkey_timeout;
                    continue;
                }
                return Err(AppError::ESshConnect(format!(
                    "连接 {target} 超时（{}s）：对端无响应，请检查网络、端口或防火墙",
                    normal_timeout.as_secs_f32().ceil() as u64
                )));
            }
        }
    }
}

/// 构造会话句柄与 writer 接收端：`SessionHandle` 持有输入 sender，返回 receiver。
pub fn make_session_handle(
    id: String,
    host_id: String,
    kind: SessionKind,
) -> (SessionHandle, mpsc::Receiver<Vec<u8>>) {
    let (tx, rx) = mpsc::channel::<Vec<u8>>(64);
    (
        SessionHandle {
            id,
            host_id,
            kind,
            started_at: chrono_now(),
            writer: tx,
            cancel: CancellationToken::new(),
            encoding: std::sync::Arc::new(Mutex::new(
                crate::encoding::SessionEncoding::default_utf8(),
            )),
        },
        rx,
    )
}

fn chrono_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// keepalive 间隔来自偏好；0 = 禁用；始终不使用 inactivity_timeout。
    #[test]
    fn keepalive_config_follows_preference() {
        let on = client_config(30);
        assert_eq!(on.keepalive_interval, Some(Duration::from_secs(30)));
        assert_eq!(on.keepalive_max, 3);
        assert!(on.inactivity_timeout.is_none(), "不得用 inactivity_timeout");

        let custom = client_config(10);
        assert_eq!(custom.keepalive_interval, Some(Duration::from_secs(10)));

        let off = client_config(0);
        assert!(off.keepalive_interval.is_none(), "0 应禁用 keepalive");
        assert!(off.inactivity_timeout.is_none());
    }

    // ---------------- 连接阶段超时 ----------------

    use std::time::Instant;
    use tokio::sync::oneshot;

    fn timeout_handler(session_id: &str, pending: PendingHostKeyMap) -> ClientHandler {
        ClientHandler {
            session_id: session_id.to_string(),
            host_id: "h1".to_string(),
            host: "127.0.0.1".to_string(),
            port: 22,
            pending,
            app: None,
            pinned: None,
            pinned_mismatch: None,
        }
    }

    /// 起一个「接受 TCP 但永不发 SSH 版本串」的对端：模拟半死服务器。
    async fn silent_ssh_server() -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("绑定本地端口失败");
        let addr = listener.local_addr().expect("取本地地址失败");
        tokio::spawn(async move {
            if let Ok((sock, _)) = listener.accept().await {
                // 保持连接打开但不发送任何字节：客户端应命中连接超时而不是永远挂起。
                tokio::time::sleep(Duration::from_secs(10)).await;
                drop(sock);
            }
        });
        addr
    }

    #[tokio::test]
    async fn connect_times_out_when_server_never_speaks_ssh() {
        let addr = silent_ssh_server().await;
        let started = Instant::now();
        let err = match connect_client_with_timeouts(
            Arc::new(russh::client::Config::default()),
            (addr.ip().to_string(), addr.port()),
            timeout_handler("s-timeout", PendingHostKeyMap::default()),
            Duration::from_millis(200),
            Duration::from_secs(5),
            None,
        )
        .await
        {
            Ok(_) => panic!("对端不回 SSH 版本串时必须超时失败，不能无限挂起"),
            Err(e) => e,
        };
        assert!(matches!(err, AppError::ESshConnect(_)), "实际: {err:?}");
        assert!(
            err.to_string().contains("超时"),
            "错误文案应说明超时: {err}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "超时应约 200ms 触发，实际 {:?}",
            started.elapsed()
        );
    }

    /// 指纹弹窗等待用户决策期间不能被「网络超时」掐断（连接预算应放宽）。
    #[tokio::test]
    async fn connect_budget_is_extended_while_hostkey_decision_pending() {
        let addr = silent_ssh_server().await;
        let pending: PendingHostKeyMap = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let (tx, _rx) = oneshot::channel();
        pending.lock().await.insert(
            "s-hostkey".to_string(),
            crate::state::PendingHostKey {
                host: "127.0.0.1".to_string(),
                port: 22,
                fingerprint: "SHA256:test".to_string(),
                public_key: "ssh-ed25519 AAAATEST".to_string(),
                decision_tx: tx,
            },
        );

        let fut = connect_client_with_timeouts(
            Arc::new(russh::client::Config::default()),
            (addr.ip().to_string(), addr.port()),
            timeout_handler("s-hostkey", pending.clone()),
            Duration::from_millis(100),
            Duration::from_secs(5),
            None,
        );
        // 100ms 的网络预算已过，但该会话有挂起指纹挑战 → 必须仍在等待（未返回超时错误）。
        let outcome = tokio::time::timeout(Duration::from_millis(600), fut).await;
        assert!(
            outcome.is_err(),
            "指纹挑战挂起期间不得按网络超时提前失败（连接应仍在等待用户决策）"
        );
        pending.lock().await.remove("s-hostkey");
    }

    /// 取消令牌触发后连接必须立刻中止（返回 E_CANCELLED），
    /// 而不是继续在后台握手直到超时——这正是"关标签后后台还在偷偷连"的根因。
    #[tokio::test]
    async fn connect_is_aborted_when_cancelled() {
        let addr = silent_ssh_server().await;
        let cancel = CancellationToken::new();
        let cancel_remote = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(60)).await;
            cancel_remote.cancel();
        });

        let started = Instant::now();
        let err = match connect_client_with_timeouts(
            Arc::new(russh::client::Config::default()),
            (addr.ip().to_string(), addr.port()),
            timeout_handler("s-cancel", PendingHostKeyMap::default()),
            // 超时给足 30s：能提前返回只可能是取消生效
            Duration::from_secs(30),
            Duration::from_secs(30),
            Some(&cancel),
        )
        .await
        {
            Ok(_) => panic!("取消后连接不得继续成功"),
            Err(e) => e,
        };
        assert!(
            matches!(err, AppError::ECancelled(_)),
            "应返回 E_CANCELLED，实际: {err:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "取消应在毫秒级生效，实际 {:?}",
            started.elapsed()
        );
    }

    /// 取消时同时清掉可能已注册的 hostkey 挂起条目（避免残留到 120s 超时）。
    #[tokio::test]
    async fn cancel_cleans_pending_hostkey_entry() {
        let addr = silent_ssh_server().await;
        let pending: PendingHostKeyMap = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let (tx, _rx) = oneshot::channel();
        pending.lock().await.insert(
            "s-cancel-hostkey".to_string(),
            crate::state::PendingHostKey {
                host: "127.0.0.1".to_string(),
                port: 22,
                fingerprint: "SHA256:test".to_string(),
                public_key: "ssh-ed25519 AAAATEST".to_string(),
                decision_tx: tx,
            },
        );
        let cancel = CancellationToken::new();
        cancel.cancel(); // 已取消：进入 select 后立刻命中取消分支

        let err = match connect_client_with_timeouts(
            Arc::new(russh::client::Config::default()),
            (addr.ip().to_string(), addr.port()),
            timeout_handler("s-cancel-hostkey", pending.clone()),
            Duration::from_secs(30),
            Duration::from_secs(30),
            Some(&cancel),
        )
        .await
        {
            Ok(_) => panic!("已取消的连接不得成功"),
            Err(e) => e,
        };
        assert!(matches!(err, AppError::ECancelled(_)), "{err:?}");
        assert!(
            !pending.lock().await.contains_key("s-cancel-hostkey"),
            "取消后必须清理 hostkey 挂起条目"
        );
    }
}
