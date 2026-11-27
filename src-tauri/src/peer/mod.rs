//! 局域网对等层（peer）服务：可见性三态 + SSH 监听生命周期 + 设备表快照。
//!
//! 可见性（按产品决策）：
//! - **默认关**（`lanVisible=false`）：不监听、不广播；
//! - **可发现窗口**：配对/连接操作时自动开启，默认 5 分钟后自动关闭；
//! - **常开**：设置里打开「局域网可见」后一直监听。
//!
//! 本模块负责监听、可见性与设备表；发现、配对、终端控制、屏幕查看等
//! 分别在子模块中实现。

pub mod client;

pub mod audio;
pub mod audit;
pub mod capture_state;
pub mod clipboard;
pub mod control;
pub mod discovery;
pub mod h264;
pub mod identity;
pub mod inbox;
pub mod input;
pub mod pairing;
pub mod peers;
pub mod rd;
pub mod server;
pub mod sftp_fs;

use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::Emitter;
use tokio::net::TcpListener;
use tokio::sync::{Mutex, OnceCell, RwLock};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::error::{AppError, AppResult};
use crate::models::Preferences;
use identity::Identity;
use server::SharedPeerState;

/// 端口候选：首选端口失败时在同段内退避。
const PEER_PORT_RANGE: std::ops::RangeInclusive<u16> = 27410..=27419;
/// 可发现窗口默认时长（分钟）。
pub const DEFAULT_WINDOW_MINUTES: u8 = 5;

/// 启动监听所需的配置快照（来自 Preferences，避免服务反向依赖 AppState）。
#[derive(Debug, Clone)]
pub struct PeerConfig {
    pub device_name: String,
    pub port: u16,
    pub share_root: String,
    pub strict_pairing: bool,
}

impl PeerConfig {
    pub fn from_prefs(p: &Preferences) -> Self {
        Self {
            device_name: p.lan_device_name.clone(),
            port: p.lan_port,
            share_root: p.peer_share_root.clone(),
            strict_pairing: p.lan_strict_pairing,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Off,
    Window,
    AlwaysOn,
}

struct Inner {
    /// 已配对设备表（SSH 处理器共享同一份，配对后立即可用）。
    peers: Arc<RwLock<peers::PeersFile>>,
    port: AtomicU16,
    /// (模式, 窗口到期时间)
    mode: StdMutex<(Mode, Option<Instant>)>,
    cancel: Mutex<Option<CancellationToken>>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    fingerprint: StdMutex<Option<String>>,
    /// 与 PairCtx 共享，设置里改设备名后配对 Hello 立即使用新值。
    device_name: Arc<StdMutex<String>>,
    /// 与 PairCtx 共享：设置里切换严格配对后立即生效。
    strict_pairing: Arc<AtomicBool>,
    /// Tauri 句柄（发现/配对事件推送用）
    app: tauri::AppHandle,
    /// 身份密钥（单飞加载；配对握手/SSH 客户端复用）
    identity: OnceCell<Identity>,
    /// 本机设备 id（= 身份公钥指纹）
    local_id: StdMutex<Option<String>>,
    /// 发现服务（随监听生灭）
    discovery: Mutex<Option<discovery::Discovery>>,
    /// 配对服务任务（停止时等待退出：配对端口在返回前已释放，避免重启抢占）
    pair_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// 配对监听实际端口（0 = 不可用）
    pair_port: AtomicU16,
    /// 配对握手任务（停止时等待退出；出站也纳入）
    pair_tasks: Mutex<Option<TaskTracker>>,
    /// 已接受的对等 SSH 连接任务（停止监听时取消并等待）
    conn_tasks: Mutex<Option<TaskTracker>>,
    /// 可发现窗口计时器（切换模式/延长窗口时取消上一个）
    window_timer: Mutex<Option<CancellationToken>>,
    /// 窗口计时器代数（防止已到点的旧计时器误关新监听）
    window_generation: AtomicU64,
    /// 启动互斥（避免并发 start 各绑一套端口）
    start_lock: Mutex<()>,
    /// 已配对设备指纹前 8 位（发现列表「已配对」标记与配对握手共享）
    paired_fps: Arc<Mutex<Vec<String>>>,
    /// 进行中的配对会话
    pair_sessions: Arc<pairing::PairSessions>,
    /// 受控端状态（确认请求 + 活动控制会话）
    control: Arc<control::ControlHub>,
    /// 屏幕查看（受查看端状态）
    screen: Arc<rd::ScreenHub>,
    /// 被控端麦克风确认与活动采集会话
    mic: Arc<audio::MicHub>,
}

pub struct PeerService {
    inner: Arc<Inner>,
}

impl PeerService {
    pub fn new(app: tauri::AppHandle) -> Self {
        Self {
            inner: Arc::new(Inner {
                peers: Arc::new(RwLock::new(peers::PeersFile::default())),
                port: AtomicU16::new(0),
                mode: StdMutex::new((Mode::Off, None)),
                cancel: Mutex::new(None),
                task: Mutex::new(None),
                fingerprint: StdMutex::new(None),
                device_name: Arc::new(StdMutex::new(String::new())),
                strict_pairing: Arc::new(AtomicBool::new(false)),
                app,
                identity: OnceCell::new(),
                local_id: StdMutex::new(None),
                discovery: Mutex::new(None),
                pair_task: Mutex::new(None),
                pair_port: AtomicU16::new(0),
                pair_tasks: Mutex::new(None),
                conn_tasks: Mutex::new(None),
                window_timer: Mutex::new(None),
                window_generation: AtomicU64::new(0),
                start_lock: Mutex::new(()),
                paired_fps: Arc::new(Mutex::new(Vec::new())),
                pair_sessions: Arc::new(pairing::PairSessions::new()),
                control: Arc::new(control::ControlHub::new()),
                screen: Arc::new(rd::ScreenHub::new()),
                mic: Arc::new(audio::MicHub::new()),
            }),
        }
    }

    /// 实际监听端口（未监听时 None）。
    pub fn port(&self) -> Option<u16> {
        match self.inner.port.load(Ordering::SeqCst) {
            0 => None,
            p => Some(p),
        }
    }

    /// 配对监听实际端口（未启动/绑定失败时为 None）。
    pub fn pair_port(&self) -> Option<u16> {
        match self.inner.pair_port.load(Ordering::SeqCst) {
            0 => None,
            p => Some(p),
        }
    }

    pub fn is_always_on(&self) -> bool {
        matches!(self.inner.mode.lock().unwrap().0, Mode::AlwaysOn)
    }

    /// 可发现窗口剩余秒数（常开或关闭时为 None）。
    pub fn window_left_sec(&self) -> Option<u64> {
        let st = self.inner.mode.lock().unwrap();
        match (st.0, st.1) {
            (Mode::Window, Some(deadline)) => {
                Some(deadline.saturating_duration_since(Instant::now()).as_secs())
            }
            _ => None,
        }
    }

    pub fn fingerprint(&self) -> Option<String> {
        self.inner.fingerprint.lock().unwrap().clone()
    }

    pub fn device_name(&self) -> String {
        self.inner.device_name.lock().unwrap().clone()
    }

    /// 设置里改设备名时同步到运行态（配对 Hello 立即生效）。
    pub fn set_device_name(&self, name: &str) {
        *self.inner.device_name.lock().unwrap() = resolve_device_name(name);
    }

    /// 严格配对开关（设置页改动立即生效，无需重启监听）。
    pub fn set_strict_pairing(&self, strict: bool) {
        self.inner.strict_pairing.store(strict, Ordering::SeqCst);
    }

    pub fn strict_pairing(&self) -> bool {
        self.inner.strict_pairing.load(Ordering::SeqCst)
    }

    /// 本机身份密钥（单飞加载：并发首次调用不会生成两把不同密钥）。
    pub async fn identity_handle(&self) -> AppResult<Identity> {
        let id = self
            .inner
            .identity
            .get_or_try_init(|| async {
                let id = tauri::async_runtime::spawn_blocking(identity::Identity::load_or_create)
                    .await
                    .map_err(|e| AppError::ESecret(format!("身份密钥线程失败: {e}")))??;
                let fp = id.fingerprint();
                *self.inner.fingerprint.lock().unwrap() = Some(fp.clone());
                *self.inner.local_id.lock().unwrap() = Some(fp);
                Ok::<Identity, AppError>(id)
            })
            .await?
            .clone();
        Ok(id)
    }

    /// 本机设备 id（= 身份公钥指纹）；未加载过时按需加载。
    pub async fn local_device_id(&self) -> AppResult<String> {
        if let Some(id) = self.inner.local_id.lock().unwrap().clone() {
            return Ok(id);
        }
        let _ = self.identity_handle().await?;
        Ok(self
            .inner
            .local_id
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_default())
    }

    /// 按 id 取设备记录（不存在返回 None）。
    pub async fn peer_record(&self, id: &str) -> Option<peers::PeerRecord> {
        self.inner
            .peers
            .read()
            .await
            .peers
            .iter()
            .find(|p| p.id == id)
            .cloned()
    }

    /// 对端可连地址：优先发现列表里的实时地址，其次 `peers.toml` 的 lastAddr。
    pub async fn peer_addr(&self, id: &str) -> Option<String> {
        {
            let guard = self.inner.discovery.lock().await;
            if let Some(d) = guard.as_ref() {
                if let Some(found) = d.list().await.into_iter().find(|x| x.device_id == id) {
                    return Some(found.addr);
                }
            }
        }
        self.peer_record(id).await.and_then(|p| p.last_addr)
    }

    /// 指纹按需加载（与 identity_handle 共用同一个单飞结果，避免只缓存指纹
    /// 而 identity/local_id 仍是另一把密钥）。
    pub async fn ensure_fingerprint(&self) -> String {
        if let Some(f) = self.fingerprint() {
            return f;
        }
        match self.identity_handle().await {
            Ok(id) => id.fingerprint(),
            Err(e) => {
                applog!("[peer] 身份密钥不可用: {e}");
                String::new()
            }
        }
    }

    pub async fn peer_count(&self) -> usize {
        self.inner.peers.read().await.peers.len()
    }

    /// 重新读取 `peers.toml`（配对/解除配对后调用，处理器共享同一份表）。
    pub async fn reload_peers(&self) -> AppResult<usize> {
        let file = peers::load()?;
        let n = file.peers.len();
        *self.inner.peers.write().await = file;
        Ok(n)
    }

    /// 开始监听。
    /// - `window_minutes=None` → 常开；
    /// - `Some(n)` → 可发现窗口，n 分钟后自动停止。
    pub async fn start(&self, cfg: &PeerConfig, window_minutes: Option<u8>) -> AppResult<u16> {
        // 串行化启动：`peer_discover` 轮询 + 用户操作可能并发触发（两者都看到 port=None
        // 时会各绑一套监听，造成端口泄漏与配对端口抢占）。
        let _start_guard = self.inner.start_lock.lock().await;
        *self.inner.device_name.lock().unwrap() = resolve_device_name(&cfg.device_name);
        self.inner
            .strict_pairing
            .store(cfg.strict_pairing, Ordering::SeqCst);

        // 已在监听：只切换模式（窗口计时器随之重启）。
        // 配对监听可能在上次启动时绑定失败，这里补一次重试（否则入站配对
        // 会一直不可用，直到用户完全关闭再开启局域网）。
        if let Some(port) = self.port() {
            self.ensure_pairing_service().await;
            if self.is_always_on() && window_minutes.is_some() {
                return Ok(port);
            }
            self.set_mode(window_minutes);
            self.arm_window_timer(window_minutes).await;
            return Ok(port);
        }

        let identity = self.identity_handle().await?;
        let fp_full = identity.fingerprint();
        self.reload_peers().await?;

        let root = Arc::new(
            sftp_fs::Root::from_share_root(&cfg.share_root).map_err(AppError::EPeerSftpRoot)?,
        );
        // 收件箱目录：监听启动即创建，配置推送（SFTP `/peers/inbox`）不依赖文件互传授权
        let inbox = inbox::inbox_dir()?;
        let conn_tasks = TaskTracker::new();
        let shared = Arc::new(SharedPeerState {
            peers: self.inner.peers.clone(),
            root,
            inbox,
            app: self.inner.app.clone(),
            control: self.inner.control.clone(),
            screen: self.inner.screen.clone(),
            mic: self.inner.mic.clone(),
            local_id: fp_full.clone(),
            device_name: self.inner.device_name.clone(),
            auth_failures: Arc::new(Mutex::new(std::collections::HashMap::new())),
            connections: conn_tasks.clone(),
        });

        let listener = bind_listener(cfg.port).await?;
        let port = listener
            .local_addr()
            .map_err(|e| AppError::EPeerListen(format!("读取监听地址失败: {e}")))?
            .port();

        // 配对监听提前绑定：发现报文必须广播**实际**端口（默认端口可能被占用，
        // 内部会 28410–28419 退避）。绑不上也不影响 SSH/发现，只是配对不可用。
        let pair_bound = match pairing::bind_listener().await {
            Ok(v) => Some(v),
            Err(e) => {
                applog!("[peer] 配对服务不可用（不影响 SSH）: {e}");
                None
            }
        };

        let cancel = CancellationToken::new();
        // 身份与设备 id（发现/配对复用）
        *self.inner.local_id.lock().unwrap() = Some(fp_full.clone());
        let srv_cfg = server::build_config(&identity);
        let conn_tasks = shared.connections.clone();
        *self.inner.conn_tasks.lock().await = Some(conn_tasks.clone());
        let mut srv = server::PeerSshServer { shared };
        let cancel_loop = cancel.clone();
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel_loop.cancelled() => break,
                    accepted = listener.accept() => {
                        match accepted {
                            Ok((stream, addr)) => {
                                // SFTP 小包往返吞吐：accept 后关闭 Nagle（russh-sftp README 建议）
                                let _ = stream.set_nodelay(true);
                                let handler = russh::server::Server::new_client(&mut srv, Some(addr));
                                let cfg = srv_cfg.clone();
                                let tasks = conn_tasks.clone();
                                let cancel_conn = cancel_loop.clone();
                                tasks.spawn(async move {
                                    tokio::select! {
                                        _ = cancel_conn.cancelled() => {}
                                        r = russh::server::run_stream(cfg, stream, handler) => {
                                            if let Err(e) = r {
                                                applog!("[peer] 连接结束: {e:#}");
                                            }
                                        }
                                    }
                                });
                            }
                            Err(e) => {
                                applog!("[peer] accept 失败: {e}");
                                break;
                            }
                        }
                    }
                }
            }
            applog!("[peer] 监听已停止");
        });

        // 配对握手任务跟踪器：入站与出站都由它管理，停止监听时统一等待。
        let pair_tasks = TaskTracker::new();
        *self.inner.pair_tasks.lock().await = Some(pair_tasks.clone());

        // 配对服务（与监听同生命周期）
        let advertised_pair_port = if let Some((pair_listener, pair_port)) = pair_bound {
            self.spawn_pair_service(
                pair_listener,
                pair_port,
                identity.clone(),
                fp_full.clone(),
                port,
                cancel.clone(),
                pair_tasks,
            )
            .await;
            Some(pair_port)
        } else {
            None
        };

        // 发现服务（失败不影响 SSH/SFTP；配对仍可手动输入 IP）
        {
            let mut fps = self.inner.paired_fps.lock().await;
            fps.clear();
            fps.extend(
                self.inner
                    .peers
                    .read()
                    .await
                    .peers
                    .iter()
                    .map(|p| peers::fp_short(&p.pubkey))
                    .filter(|s| !s.is_empty()),
            );
        }
        self.start_discovery(&fp_full, port, advertised_pair_port)
            .await;

        *self.inner.cancel.lock().await = Some(cancel);
        *self.inner.task.lock().await = Some(task);
        self.inner.port.store(port, Ordering::SeqCst);
        self.set_mode(window_minutes);
        applog!(
            "[peer] 开始监听 0.0.0.0:{port}（{}）",
            if window_minutes.is_some() {
                "可发现窗口"
            } else {
                "常开"
            }
        );
        self.arm_window_timer(window_minutes).await;
        Ok(port)
    }

    /// 启动配对监听（监听 socket 已提前绑定，发现报文才能广播实际端口）。
    #[allow(clippy::too_many_arguments)]
    async fn spawn_pair_service(
        &self,
        listener: TcpListener,
        pair_port: u16,
        identity: Identity,
        local_id: String,
        ssh_port: u16,
        cancel: CancellationToken,
        tasks: TaskTracker,
    ) {
        let pair_ctx = Arc::new(pairing::PairCtx {
            app: self.inner.app.clone(),
            identity,
            local_id,
            local_name: self.inner.device_name.clone(),
            ssh_port,
            peers: self.inner.peers.clone(),
            paired_fps: self.inner.paired_fps.clone(),
            sessions: self.inner.pair_sessions.clone(),
            cancel: cancel.clone(),
            tasks,
            strict_pairing: self.inner.strict_pairing.clone(),
        });
        let pair_cancel = cancel.clone();
        let task = tokio::spawn(async move {
            if let Err(e) = pairing::serve(listener, pair_port, pair_ctx, pair_cancel).await {
                applog!("[peer] 配对服务退出: {e}");
            }
        });
        *self.inner.pair_task.lock().await = Some(task);
        self.inner.pair_port.store(pair_port, Ordering::SeqCst);
    }

    /// 启动/重启发现服务。`pair_port=None` 表示配对不可用，前端据此禁用配对按钮。
    async fn start_discovery(&self, local_id: &str, ssh_port: u16, pair_port: Option<u16>) {
        let fp8: String = local_id
            .trim_start_matches("SHA256:")
            .chars()
            .take(8)
            .collect();
        let local = discovery::LocalInfo {
            id: local_id.to_string(),
            name: self.inner.device_name.clone(),
            fp8,
            ssh_port,
            pair_port,
        };
        match discovery::Discovery::start(
            self.inner.app.clone(),
            local,
            self.inner.paired_fps.clone(),
        )
        .await
        {
            Ok(d) => *self.inner.discovery.lock().await = Some(d),
            Err(e) => applog!("[peer] 发现服务启动失败（不影响连接）: {e}"),
        }
    }

    /// 已在监听时补启动配对服务（启动时配对端口被占用/发现服务失败的恢复路径）。
    /// 成功后重启发现服务，让广播的 `pair_port` 与真实端口一致。
    async fn ensure_pairing_service(&self) {
        {
            let mut task = self.inner.pair_task.lock().await;
            if let Some(t) = task.as_ref() {
                if !t.is_finished() {
                    return;
                }
                // 服务任务意外结束：清掉句柄与端口，走下面的重建路径。
                *task = None;
                self.inner.pair_port.store(0, Ordering::SeqCst);
            }
        }
        let Ok(identity) = self.identity_handle().await else {
            return;
        };
        let Some(ssh_port) = self.port() else { return };
        let Some(cancel) = self.inner.cancel.lock().await.clone() else {
            return;
        };
        let Some(tasks) = self.inner.pair_tasks.lock().await.clone() else {
            return;
        };
        let local_id = identity.fingerprint();
        match pairing::bind_listener().await {
            Ok((listener, pair_port)) => {
                self.spawn_pair_service(
                    listener,
                    pair_port,
                    identity,
                    local_id.clone(),
                    ssh_port,
                    cancel,
                    tasks,
                )
                .await;
                // 发现服务必须重启：LocalInfo 在启动时固化，旧报文仍会广播错误端口。
                let old = self.inner.discovery.lock().await.take();
                if let Some(d) = old {
                    d.shutdown().await;
                }
                self.start_discovery(&local_id, ssh_port, Some(pair_port))
                    .await;
                applog!("[peer] 配对服务已补启动，监听 0.0.0.0:{pair_port}");
            }
            Err(e) => applog!("[peer] 配对服务仍不可用: {e}"),
        }
    }

    /// 已监听时补启动配对服务（设备页轮询/窗口打开时调用；带启动锁）。
    pub async fn ensure_pairing(&self) {
        let _start_guard = self.inner.start_lock.lock().await;
        self.ensure_pairing_service().await;
    }

    pub async fn stop(&self) {
        Inner::stop(&self.inner).await;
    }

    fn set_mode(&self, window_minutes: Option<u8>) {
        let mut st = self.inner.mode.lock().unwrap();
        *st = match window_minutes {
            Some(m) => (
                Mode::Window,
                Some(Instant::now() + Duration::from_secs(m.max(1) as u64 * 60)),
            ),
            None => (Mode::AlwaysOn, None),
        };
    }

    /// 窗口模式：到点自动停止。重复调用会以最新截止时间为准（旧计时器在这里
    /// 被显式取消——不能复用服务级 cancel：升级为「常开」时它不会被触发，
    /// 旧计时器仍会在原截止时间把监听关掉）。
    async fn arm_window_timer(&self, window_minutes: Option<u8>) {
        if let Some(t) = self.inner.window_timer.lock().await.take() {
            t.cancel();
        }
        // 每次重排递增代数：已到点但还在等锁的旧计时器会被代数检查拦下，
        // 不会误关刚重新开启的监听。
        let generation = self
            .inner
            .window_generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        let Some(minutes) = window_minutes else {
            return; // 常开：不安排计时器
        };
        let timer = CancellationToken::new();
        *self.inner.window_timer.lock().await = Some(timer.clone());
        let inner = self.inner.clone();
        tokio::spawn(async move {
            let dur = Duration::from_secs(minutes.max(1) as u64 * 60);
            tokio::select! {
                _ = tokio::time::sleep(dur) => {
                    Inner::stop_if_current_window(&inner, generation).await;
                }
                _ = timer.cancelled() => {}
            }
        });
    }
    // ---------- 发现 / 配对 / 设备管理 ----------

    pub async fn effective_device_name(&self) -> String {
        let n = self.device_name();
        if n.trim().is_empty() {
            std::env::var("COMPUTERNAME").unwrap_or_else(|_| "Ruthenium".to_string())
        } else {
            n
        }
    }

    pub async fn discovered(&self) -> Vec<discovery::DiscoveredDevice> {
        match self.inner.discovery.lock().await.as_ref() {
            Some(d) => d.list().await,
            None => Vec::new(),
        }
    }

    async fn pair_ctx(&self) -> AppResult<Arc<pairing::PairCtx>> {
        let ssh_port = self
            .port()
            .ok_or_else(|| AppError::EPeerWindowClosed("局域网尚未开启".to_string()))?;
        let identity = self.identity_handle().await?;
        let local_id = identity.fingerprint();
        let cancel = self
            .inner
            .cancel
            .lock()
            .await
            .clone()
            .ok_or_else(|| AppError::EPeerWindowClosed("局域网尚未开启".to_string()))?;
        let tasks = self
            .inner
            .pair_tasks
            .lock()
            .await
            .clone()
            .ok_or_else(|| AppError::EPeerWindowClosed("配对服务未就绪".to_string()))?;
        Ok(Arc::new(pairing::PairCtx {
            app: self.inner.app.clone(),
            identity,
            local_id,
            local_name: self.inner.device_name.clone(),
            ssh_port,
            peers: self.inner.peers.clone(),
            paired_fps: self.inner.paired_fps.clone(),
            sessions: self.inner.pair_sessions.clone(),
            cancel,
            tasks,
            strict_pairing: self.inner.strict_pairing.clone(),
        }))
    }

    /// 发起配对（返回 pairingId；码与结果经 `peer://pairing` 事件推送）。
    ///
    /// `manual=true` 表示手填地址（来源不可信），强制数字比对；发现列表发起走一键允许。
    pub async fn pair_start(&self, target: &str, manual: bool) -> AppResult<String> {
        let ctx = self.pair_ctx().await?;
        pairing::pair_with(target, ctx, manual).await
    }

    /// 前端「一致/不一致」落决策。
    pub async fn pair_decide(&self, id: &str, agree: bool, grants: pairing::Grants) -> bool {
        self.inner
            .pair_sessions
            .decide(id, pairing::Decision { agree, grants })
            .await
    }

    pub async fn pair_cancel(&self, id: &str) -> bool {
        // 决策会话与尝试令牌都要清：一键模式没有决策会话，只能靠令牌打断等待。
        let decided = self.inner.pair_sessions.cancel(id).await;
        let waiting = self.inner.pair_sessions.cancel_attempt(id).await;
        decided || waiting
    }

    pub async fn list_peers(&self) -> Vec<peers::PeerRecord> {
        self.inner.peers.read().await.peers.clone()
    }

    // ---------- 终端控制（受控端） ----------

    /// 前端对控制请求「允许 / 拒绝」；返回是否命中（超时后不再命中）。
    pub async fn control_consent(&self, consent_id: &str, allow: bool) -> bool {
        self.inner.control.decide(consent_id, allow).await
    }

    /// 当前受控会话（横幅/审计）。
    pub async fn control_sessions(&self) -> Vec<control::ControlSessionView> {
        self.inner.control.list().await
    }

    /// 「立即断开」某条受控会话（杀掉 ConPTY 并关闭通道）。
    pub async fn kick_control(&self, session_id: &str) -> bool {
        self.inner.control.kick(session_id).await
    }

    // ---------- 屏幕查看（受查看端） ----------

    pub async fn screen_consent(&self, consent_id: &str, allow: bool, allow_input: bool) -> bool {
        self.inner
            .screen
            .decide(consent_id, allow, allow_input)
            .await
    }

    pub async fn screen_sessions(&self) -> Vec<rd::ViewSessionView> {
        self.inner.screen.list().await
    }

    // ---------- 被控端麦克风 ----------

    /// 前端对麦克风请求「允许 / 拒绝」；返回是否命中（超时后不再命中）。
    pub async fn mic_consent(&self, consent_id: &str, allow: bool) -> bool {
        self.inner.mic.decide(consent_id, allow).await
    }

    /// 本机一键停止麦克风：立即释放设备并阻止本会话再次开启。
    pub async fn mic_stop(&self, session_id: &str) -> bool {
        self.inner.mic.stop(session_id).await
    }

    /// 当前实际在采集麦克风的会话（横幅挂载时对账）。
    pub async fn mic_sessions(&self) -> Vec<audio::MicSessionView> {
        self.inner.mic.active().await
    }

    pub async fn kick_screen(&self, session_id: &str) -> bool {
        self.inner.screen.kick(session_id).await
    }

    pub async fn remove_peer(&self, id: &str) -> AppResult<bool> {
        let mut f = self.inner.peers.write().await;
        let mut next = f.clone();
        let removed = peers::remove_in(&mut next, id);
        if removed {
            // 先落盘、成功后再替换内存：失败时不能“假删除”。
            peers::save(&next)?;
            *f = next;
            let mut fps = self.inner.paired_fps.lock().await;
            fps.clear();
            fps.extend(
                f.peers
                    .iter()
                    .map(|p| peers::fp_short(&p.pubkey))
                    .filter(|s| !s.is_empty()),
            );
        }
        Ok(removed)
    }

    pub async fn update_grants(
        &self,
        id: &str,
        g: pairing::Grants,
    ) -> AppResult<Option<peers::PeerRecord>> {
        let mut f = self.inner.peers.write().await;
        let mut next = f.clone();
        let mut found = None;
        for p in &mut next.peers {
            if p.id == id {
                p.grant_files = g.files;
                p.grant_control = g.control;
                p.grant_config = g.config;
                p.grant_screen = g.screen;
                p.grant_clipboard = g.clipboard;
                p.grant_audio = g.audio;
                found = Some(p.clone());
                break;
            }
        }
        if found.is_some() {
            peers::save(&next)?;
            *f = next;
        }
        Ok(found)
    }

    /// 记住最近一次**成功连接**的地址：发现服务未开启或设备换了地址时，
    /// `peer_addr` 回退使用这里（否则只能用配对时记录的旧地址）。
    pub async fn remember_addr(&self, id: &str, addr: &str) {
        if addr.is_empty() {
            return;
        }
        let mut f = self.inner.peers.write().await;
        let mut next = f.clone();
        let mut changed = false;
        for p in &mut next.peers {
            if p.id == id && p.last_addr.as_deref() != Some(addr) {
                p.last_addr = Some(addr.to_string());
                changed = true;
            }
        }
        if changed {
            match peers::save(&next) {
                Ok(()) => *f = next,
                Err(e) => applog!("[peer] 更新设备地址失败: {e}"),
            }
        }
    }

    /// 用对端 `ruthenium-info` 的实时授权刷新本机缓存（`peer_allows_*`）。
    ///
    /// 这些字段只是「上次同步的快照」，**不能作为拒绝依据**：授权可在配对后
    /// 由对方随时开启/撤销，真正的强制点在服务端每次请求时读取的 `grant_*`。
    /// 仅在值变化时落盘，避免每次连接都写 `peers.toml`。
    pub(crate) async fn sync_peer_allows(&self, id: &str, info: &control::PeerInfo) {
        let mut f = self.inner.peers.write().await;
        let mut next = f.clone();
        let Some(p) = next.peers.iter_mut().find(|p| p.id == id) else {
            return;
        };
        let mut changed = false;
        macro_rules! set {
            ($field:ident, $value:expr) => {
                if p.$field != $value {
                    p.$field = $value;
                    changed = true;
                }
            };
        }
        set!(peer_allows_control, info.grant_control);
        set!(peer_allows_files, info.grant_files);
        set!(peer_allows_config, info.grant_config);
        set!(peer_allows_screen, info.grant_screen);
        set!(peer_allows_clipboard, info.grant_clipboard);
        set!(peer_allows_audio, info.grant_audio);
        if changed {
            match peers::save(&next) {
                Ok(()) => *f = next,
                Err(e) => applog!("[peer] 保存授权快照失败: {e}"),
            }
        }
    }
}

impl Inner {
    async fn stop(&self) {
        // 与 start 串行化：否则 stop 清 port 前 start 可能看到“仍在监听”而提前返回，
        // 结果前端显示已开启、实际服务已停。
        let _start_guard = self.start_lock.lock().await;
        self.stop_locked().await;
    }

    /// 仅供窗口计时器：到点时只有仍属于当前代数的计时器才能停服务
    /// （用户可能已经延窗 / 升级常开 / 重新开启监听）。
    async fn stop_if_current_window(&self, generation: u64) {
        let _start_guard = self.start_lock.lock().await;
        if self
            .window_generation
            .load(std::sync::atomic::Ordering::SeqCst)
            != generation
        {
            return;
        }
        applog!("[peer] 可发现窗口到期，自动停止监听");
        self.stop_locked().await;
    }

    async fn stop_locked(&self) {
        // 先停发现并等待其任务退出（UDP 端口释放后才能立刻重新开启），清空前端列表
        let discovery = self.discovery.lock().await.take();
        if let Some(d) = discovery {
            d.shutdown().await;
        }
        // 进行中的配对照样要收尾：否则停止监听后仍可能完成握手并写库
        let dropped = self.pair_sessions.cancel_all().await;
        if dropped > 0 {
            applog!("[peer] 已取消 {dropped} 个进行中的配对");
        }
        let _ = self.app.emit(
            crate::events::PEER_DISCOVERED,
            Vec::<discovery::DiscoveredDevice>::new(),
        );
        if let Some(c) = self.cancel.lock().await.take() {
            c.cancel();
        }
        // 受控/受查看会话的 task 是 handler 内部 spawn 的，不在这两个 tracker 里；
        // 必须显式取消它们的 token（音频/剪贴板/输入都挂在屏幕会话 token 上）。
        self.control.cancel_all().await;
        self.screen.cancel_all().await;
        // 关闭任务跟踪器：不再接收新的握手任务；已存在的任务由 wait() 等待。
        let pair_tasks = self.pair_tasks.lock().await.take();
        if let Some(t) = &pair_tasks {
            t.close();
        }
        // 已接受的对等 SSH 连接同样要在停止监听时结束：否则“关闭局域网”
        // 之后旧的 SFTP/控制/屏幕连接仍然可用。run_stream 已 select 服务级
        // cancel，等待它们退出即可。
        let conn_tasks = self.conn_tasks.lock().await.take();
        if let Some(t) = &conn_tasks {
            t.close();
        }
        if let Some(t) = self.task.lock().await.take() {
            let _ = t.await;
        }
        // 配对监听同样是固定端口：等它退出，避免“停止后立刻开启”时 28410 仍被占用
        if let Some(t) = self.pair_task.lock().await.take() {
            let _ = t.await;
        }
        if let Some(t) = &pair_tasks {
            t.wait().await;
        }
        if let Some(t) = &conn_tasks {
            t.wait().await;
        }
        if let Some(t) = self.window_timer.lock().await.take() {
            t.cancel();
        }
        self.pair_port.store(0, Ordering::SeqCst);
        self.port.store(0, Ordering::SeqCst);
        *self.mode.lock().unwrap() = (Mode::Off, None);
        applog!("[peer] 已停止监听");
    }
}

/// 设备名：空值回退主机名；与配对/发现共用，避免空名称进入 Hello。
fn resolve_device_name(name: &str) -> String {
    if name.trim().is_empty() {
        std::env::var("COMPUTERNAME").unwrap_or_else(|_| "Ruthenium".to_string())
    } else {
        name.trim().to_string()
    }
}

/// 绑定监听端口：首选端口 → 同段退避（27410..27419）。
async fn bind_listener(preferred: u16) -> AppResult<TcpListener> {
    let mut candidates: Vec<u16> = Vec::new();
    if preferred != 0 {
        candidates.push(preferred);
    }
    for p in PEER_PORT_RANGE {
        if !candidates.contains(&p) {
            candidates.push(p);
        }
    }
    let mut last_err = String::new();
    for p in candidates {
        match TcpListener::bind(("0.0.0.0", p)).await {
            Ok(l) => return Ok(l),
            Err(e) => last_err = format!("{p}: {e}"),
        }
    }
    Err(AppError::EPeerPortBusy(format!(
        "没有可用端口（{last_err}）"
    )))
}

/// 单台设备的状态摘要（供前端设置页/设备页）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerStatus {
    pub listening: bool,
    pub always_on: bool,
    pub window_left_sec: Option<u64>,
    pub port: Option<u16>,
    /// 配对监听实际端口（None = 配对不可用）
    pub pair_port: Option<u16>,
    pub device_name: String,
    pub fingerprint: String,
    pub share_root: String,
    pub peer_count: usize,
}
