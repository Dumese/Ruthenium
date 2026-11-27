//! 对等层 SSH 服务端（russh 0.65）。
//!
//! 四类能力，全部按 `peers.toml` 的**实时**授权判定（撤销后下一次连接即失效）：
//! 1. **认证**：publickey-only，公钥必须与 `peers.toml` 中某条记录一致（钉扎）；
//! 2. **SFTP**：文件互传（`grant_files`）或配置推送（`grant_config`），见 `sftp_fs.rs`；
//! 3. **终端控制**：`session`+`pty` → ConPTY（`grant_control` + **每次本机确认** +
//!    可选 Windows Hello），见 `control.rs`；
//! 4. **能力查询**：`exec("ruthenium-info")` 返回 JSON（控制方连接前的授权预检）。
//!
//! 拒绝一律用 `channel_failure`（控制方会立刻收到，而不是“连上就断”）。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use russh::server::{Auth, ChannelOpenHandle, Msg, Session, VerifiedPublicKey};
use russh::{Channel, ChannelId, MethodKind, MethodSet};
use tauri::Manager;
use tokio::sync::{mpsc, Mutex, RwLock};
use tokio_util::task::TaskTracker;

use crate::peer::control::{self, ControlHub};
use crate::peer::identity::Identity;
use crate::peer::peers::{self, PeerRecord, PeersFile};
use crate::peer::sftp_fs::{FsHandler, Grants, InboxNotify, Root};

/// 所有连接共享的状态。
pub struct SharedPeerState {
    pub peers: Arc<RwLock<PeersFile>>,
    pub root: Arc<Root>,
    /// 配置收件箱真实目录（`/peers/inbox`）
    pub inbox: std::path::PathBuf,
    /// 事件出口（收件箱 / 控制请求 / 横幅）
    pub app: tauri::AppHandle,
    /// 受控端状态（确认请求 + 活动会话）
    pub control: Arc<ControlHub>,
    /// 屏幕查看（受查看端状态）
    pub screen: Arc<crate::peer::rd::ScreenHub>,
    /// 被控端麦克风确认与活动采集会话
    pub mic: Arc<crate::peer::audio::MicHub>,
    /// 本机设备 id（`ruthenium-info` 返回，供控制方展示/审计）
    pub local_id: String,
    /// 本机设备名（与 PeerService 共享，设置页改名后立即生效）
    pub device_name: Arc<std::sync::Mutex<String>>,
    /// 认证失败计数（同 IP 60s 内 >10 次 → 断开）
    pub auth_failures: Arc<Mutex<HashMap<std::net::IpAddr, Vec<std::time::Instant>>>>,
    /// 已接受连接的跟踪器：停止局域网时取消并等待，避免“关了开关但旧连接还能用”。
    pub connections: TaskTracker,
}

/// 长寿命子系统的授权周期复核句柄——音频/剪贴板通道建立时只查一次授权，
/// 运行中每 5s 复核一次 `peers.toml` 实时快照；设备被删除或对应授权被撤销即断开。
#[derive(Clone)]
pub struct GrantCheck {
    peers: Arc<RwLock<PeersFile>>,
    peer_id: String,
}

impl GrantCheck {
    pub fn new(peers: Arc<RwLock<PeersFile>>, peer_id: String) -> Self {
        Self { peers, peer_id }
    }

    /// 当前设备记录（设备被删除返回 None）；用于实时复核授权与刷新快照。
    pub async fn current(&self) -> Option<PeerRecord> {
        self.peers
            .read()
            .await
            .peers
            .iter()
            .find(|p| p.id == self.peer_id)
            .cloned()
    }

    /// 设备是否仍存在且持有全部指定授权。
    pub async fn still_granted(&self, check: impl Fn(&PeerRecord) -> bool) -> bool {
        self.current().await.is_some_and(|p| check(&p))
    }
}

pub struct PeerSshServer {
    pub shared: Arc<SharedPeerState>,
}

impl russh::server::Server for PeerSshServer {
    type Handler = PeerHandler;

    fn new_client(&mut self, peer_addr: Option<SocketAddr>) -> Self::Handler {
        PeerHandler {
            shared: self.shared.clone(),
            peer: None,
            peer_addr,
            channels: Arc::new(Mutex::new(HashMap::new())),
            pty_sizes: HashMap::new(),
            ctrl_resize: HashMap::new(),
        }
    }

    fn handle_session_error(&mut self, error: <Self::Handler as russh::server::Handler>::Error) {
        applog!("[peer] 会话错误: {error:#}");
    }
}

pub struct PeerHandler {
    shared: Arc<SharedPeerState>,
    peer: Option<PeerRecord>,
    peer_addr: Option<SocketAddr>,
    /// `Channel` 必须在 `channel_open_session` 里接住，`subsystem_request` 才能
    /// `into_stream()` 交给 russh-sftp（russh 0.65 官方 sftp_server 示例模式）。
    channels: Arc<Mutex<HashMap<ChannelId, Channel<Msg>>>>,
    /// pty_request 记录的终端尺寸（shell_request 启动 ConPTY 时用）
    pty_sizes: HashMap<ChannelId, (u16, u16)>,
    /// 活动控制会话的 resize 发送端（window_change_request → ConPTY）
    ctrl_resize: HashMap<ChannelId, mpsc::Sender<(u16, u16)>>,
}

impl PeerHandler {
    /// 实时读取设备记录：认证时的 `self.peer` 只是快照，授权/删除可能在连接
    /// 存活期间发生变化。所有新 channel 打开前都必须用这里的结果判定。
    async fn live_peer(&self) -> Option<PeerRecord> {
        let id = self.peer.as_ref()?.id.clone();
        self.shared
            .peers
            .read()
            .await
            .peers
            .iter()
            .find(|p| p.id == id)
            .cloned()
    }

    /// 当前偏好里的 Windows Hello 开关（每次请求实时读取）。
    async fn require_windows_hello(&self) -> bool {
        // 先取出 Arc，再用局部变量读锁：避免 State/守卫的借用跨 await
        let config = {
            let state = self.shared.app.state::<crate::state::AppState>();
            state.config.clone()
        };
        let required = config.read().await.preferences.require_windows_hello;
        required
    }
}

impl russh::server::Handler for PeerHandler {
    type Error = anyhow::Error;

    async fn auth_publickey(
        &mut self,
        _user: &str,
        key: VerifiedPublicKey<'_>,
    ) -> Result<Auth, Self::Error> {
        let peers = self.shared.peers.read().await;
        match peers::find_by_pubkey(&peers, key.public_key) {
            Some(rec) => {
                // 认证成功：清掉该 IP 的失败计数（避免历史失败拖累正常重连）
                if let Some(ip) = self.peer_addr.map(|a| a.ip()) {
                    self.shared.auth_failures.lock().await.remove(&ip);
                }
                applog!("[peer] 认证通过: {} from {:?}", rec.name, self.peer_addr);
                self.peer = Some(rec.clone());
                Ok(Auth::Accept)
            }
            None => {
                // 同 IP 60s 内失败 >10 次直接断开（否则可无限爆破公钥）。
                let Some(ip) = self.peer_addr.map(|a| a.ip()) else {
                    return Ok(Auth::Reject {
                        proceed_with_methods: None,
                        partial_success: false,
                    });
                };
                let over = {
                    let mut map = self.shared.auth_failures.lock().await;
                    let list = map.entry(ip).or_default();
                    list.retain(|t| t.elapsed() < std::time::Duration::from_secs(60));
                    list.push(std::time::Instant::now());
                    // 不要在这里 remove：那会把计数清零，攻击者每次新建连接又获得
                    // 10 次机会。保留窗口内的失败记录，有效公钥认证成功时才清空。
                    list.len() > 10
                };
                if over {
                    applog!("[peer] 认证失败次数过多，断开连接: {ip}");
                    return Err(anyhow::anyhow!("authentication failures exceeded limit"));
                }
                applog!("[peer] 认证拒绝: 未配对公钥 from {:?}", self.peer_addr);
                Ok(Auth::Reject {
                    proceed_with_methods: None,
                    partial_success: false,
                })
            }
        }
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.channels.lock().await.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn subsystem_request(
        &mut self,
        id: ChannelId,
        name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        // 声音播放：持久授权 + 屏幕会话令牌，默认关。
        if name == "ruthenium-audio" {
            let Some(peer) = self.live_peer().await else {
                session.channel_failure(id)?;
                return Ok(());
            };
            if !peer.grant_audio {
                applog!("[peer] 音频子系统拒绝: {} 未授予声音权限", peer.name);
                session.channel_failure(id)?;
                return Ok(());
            }
            let channel = match self.channels.lock().await.remove(&id) {
                Some(c) => c,
                None => {
                    session.channel_failure(id)?;
                    return Ok(());
                }
            };
            session.channel_success(id)?;
            let hub = self.shared.screen.clone();
            let mic = self.shared.mic.clone();
            let app = self.shared.app.clone();
            let peer_name = peer.name.clone();
            let addr = self.peer_addr;
            // 授权周期复核句柄（peers 实时快照 + 设备 id），撤销即断音频
            let grant_check = GrantCheck {
                peers: self.shared.peers.clone(),
                peer_id: peer.id.clone(),
            };
            tokio::spawn(async move {
                if let Err(e) = crate::peer::audio::serve(
                    hub,
                    grant_check,
                    mic,
                    app,
                    peer_name,
                    channel.into_stream(),
                )
                .await
                {
                    applog!("[peer] 音频通道结束（{addr:?}）: {e}");
                }
            });
            return Ok(());
        }

        // 剪贴板文本同步：持久授权 + 屏幕会话令牌，默认关。
        if name == "ruthenium-clipboard" {
            let Some(peer) = self.live_peer().await else {
                session.channel_failure(id)?;
                return Ok(());
            };
            if !peer.grant_clipboard {
                applog!("[peer] 剪贴板子系统拒绝: {} 未授予剪贴板权限", peer.name);
                session.channel_failure(id)?;
                return Ok(());
            }
            let channel = match self.channels.lock().await.remove(&id) {
                Some(c) => c,
                None => {
                    session.channel_failure(id)?;
                    return Ok(());
                }
            };
            session.channel_success(id)?;
            let hub = self.shared.screen.clone();
            let addr = self.peer_addr;
            let grant_check = GrantCheck {
                peers: self.shared.peers.clone(),
                peer_id: peer.id.clone(),
            };
            tokio::spawn(async move {
                if let Err(e) =
                    crate::peer::clipboard::serve(hub, grant_check, channel.into_stream()).await
                {
                    applog!("[peer] 剪贴板通道结束（{addr:?}）: {e}");
                }
            });
            return Ok(());
        }

        // 键鼠输入子系统（会话级「允许操作」由 ScreenHub 校验）。
        if name == "ruthenium-input" {
            let Some(peer) = self.live_peer().await else {
                session.channel_failure(id)?;
                return Ok(());
            };
            if !peer.grant_control {
                applog!("[peer] 输入子系统拒绝: {} 未授予控制权限", peer.name);
                session.channel_failure(id)?;
                return Ok(());
            }
            let channel = match self.channels.lock().await.remove(&id) {
                Some(c) => c,
                None => {
                    session.channel_failure(id)?;
                    return Ok(());
                }
            };
            session.channel_success(id)?;
            let hub = self.shared.screen.clone();
            let addr = self.peer_addr;
            // 运行中每 1s 复核 peers.toml：撤销控制权限后注入立即停止
            let grant = GrantCheck::new(self.shared.peers.clone(), peer.id.clone());
            tokio::spawn(async move {
                if let Err(e) = crate::peer::input::serve(hub, grant, channel.into_stream()).await {
                    applog!("[peer] 输入通道结束（{addr:?}）: {e}");
                }
            });
            return Ok(());
        }

        // 屏幕流子系统（只看）：授权 + 每次本机确认 + 可选 Hello。
        // `ruthenium-screen+input` 表示查看端声明自己会请求键鼠操作（确认框才展示
        // 「允许操作」），避免每次查看都默认索要操作权限。
        if name == "ruthenium-screen" || name == "ruthenium-screen+input" {
            let request_input = name.ends_with("+input");
            let Some(peer) = self.live_peer().await else {
                session.channel_failure(id)?;
                return Ok(());
            };
            if !peer.grant_screen {
                applog!("[peer] 屏幕子系统拒绝: {} 未授予查看屏幕权限", peer.name);
                session.channel_failure(id)?;
                return Ok(());
            }
            let channel = match self.channels.lock().await.remove(&id) {
                Some(c) => c,
                None => {
                    session.channel_failure(id)?;
                    return Ok(());
                }
            };
            let require_hello = self.require_windows_hello().await;
            let allow_input = match crate::peer::rd::approve(
                &self.shared.screen,
                &self.shared.app,
                &peer,
                self.peer_addr,
                require_hello,
                request_input,
                false,
            )
            .await
            {
                Ok(v) => v,
                Err(e) => {
                    applog!("[peer] 屏幕查看未通过: {e}");
                    session.channel_failure(id)?;
                    return Ok(());
                }
            };
            session.channel_success(id)?;
            let addr = self
                .peer_addr
                .map(|a| a.ip().to_string())
                .unwrap_or_default();
            // 运行中每 5s 复核 peers.toml：撤销屏幕权限后流立即结束
            let grant = GrantCheck::new(self.shared.peers.clone(), peer.id.clone());
            if let Err(e) = crate::peer::rd::start_stream(
                self.shared.app.clone(),
                self.shared.screen.clone(),
                peer,
                self.peer_addr,
                addr,
                65,
                allow_input,
                require_hello,
                channel.into_stream(),
                grant,
            )
            .await
            {
                applog!("[peer] 启动屏幕流失败: {e}");
            }
            return Ok(());
        }

        // 文件互传（grantFiles）→ 共享根；配置推送（grantConfig）→ 保留路径
        // `/peers/inbox`。两者独立：只授予配置时看不到任何共享文件；两者都无则拒绝。
        let live = self.live_peer().await;
        let grants = if name == "sftp" {
            live.as_ref()
                .map(|p| Grants {
                    files: p.grant_files,
                    config: p.grant_config,
                })
                .filter(|g| g.any())
        } else {
            None
        };
        let Some(grants) = grants else {
            applog!(
                "[peer] 子系统拒绝: name={name} peer={:?} grantFiles={:?} grantConfig={:?}",
                live.as_ref().map(|p| p.name.clone()),
                live.as_ref().map(|p| p.grant_files),
                live.as_ref().map(|p| p.grant_config)
            );
            session.channel_failure(id)?;
            return Ok(());
        };
        let channel = match self.channels.lock().await.remove(&id) {
            Some(c) => c,
            None => {
                session.channel_failure(id)?;
                return Ok(());
            }
        };
        session.channel_success(id)?;
        applog!(
            "[peer] SFTP 会话开始: {} from {:?}（{}）",
            live.as_ref().map(|p| p.name.clone()).unwrap_or_default(),
            self.peer_addr,
            if grants.files {
                "文件互传"
            } else {
                "仅配置推送"
            }
        );
        let notify = live.as_ref().map(|p| InboxNotify {
            app: self.shared.app.clone(),
            peer_name: p.name.clone(),
        });
        // 每个 SFTP 请求都复核 peers.toml（撤销 files/config 后旧会话立刻失效）
        let grant = live
            .as_ref()
            .map(|p| GrantCheck::new(self.shared.peers.clone(), p.id.clone()));
        russh_sftp::server::run(
            channel.into_stream(),
            FsHandler::new(
                self.shared.root.clone(),
                grants,
                self.shared.inbox.clone(),
                notify,
                grant,
            ),
        )
        .await;
        applog!("[peer] SFTP 会话结束: {:?}", self.peer_addr);
        Ok(())
    }

    /// `exec("ruthenium-info")`：控制方连接前的授权预检（不做任何副作用）。
    async fn exec_request(
        &mut self,
        id: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let cmd = String::from_utf8_lossy(data).trim().to_string();
        if cmd != "ruthenium-info" {
            applog!("[peer] exec 拒绝: {cmd}");
            session.channel_failure(id)?;
            return Ok(());
        }
        let Some(peer) = self.peer.clone() else {
            session.channel_failure(id)?;
            return Ok(());
        };
        let channel = match self.channels.lock().await.remove(&id) {
            Some(c) => c,
            None => {
                session.channel_failure(id)?;
                return Ok(());
            }
        };
        session.channel_success(id)?;
        let require_hello = self.require_windows_hello().await;
        // 授权以 peers.toml 当前记录为准（连接建立后撤销/补授权都要立刻反映）
        let live = self
            .shared
            .peers
            .read()
            .await
            .peers
            .iter()
            .find(|p| p.id == peer.id)
            .cloned();
        let device_name = self
            .shared
            .device_name
            .lock()
            .map(|n| n.clone())
            .unwrap_or_default();
        let info = serde_json::json!({
            "id": self.shared.local_id,
            "name": device_name,
            "grantControl": live.as_ref().is_some_and(|p| p.grant_control),
            "grantFiles": live.as_ref().is_some_and(|p| p.grant_files),
            "grantConfig": live.as_ref().is_some_and(|p| p.grant_config),
            "grantScreen": live.as_ref().is_some_and(|p| p.grant_screen),
            "grantClipboard": live.as_ref().is_some_and(|p| p.grant_clipboard),
            "grantAudio": live.as_ref().is_some_and(|p| p.grant_audio),
            "screenInput": true,
            "requireWindowsHello": require_hello,
        });
        if let Ok(bytes) = serde_json::to_vec(&info) {
            let _ = channel.data_bytes(bytes).await;
        }
        let _ = channel.eof().await;
        let _ = channel.close().await;
        Ok(())
    }

    async fn pty_request(
        &mut self,
        id: ChannelId,
        _term: &str,
        cols: u32,
        rows: u32,
        _pix_w: u32,
        _pix_h: u32,
        _modes: &[(russh::Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let Some(peer) = self.live_peer().await else {
            session.channel_failure(id)?;
            return Ok(());
        };
        if !peer.grant_control {
            applog!("[peer] PTY 拒绝: {} 未授予控制权限", peer.name);
            session.channel_failure(id)?;
            return Ok(());
        }
        // clamp：0×0 会让 CreatePseudoConsole 返回 E_INVALIDARG
        let size = control::clamp_size(
            cols.min(u16::MAX as u32) as u16,
            rows.min(u16::MAX as u32) as u16,
        );
        self.pty_sizes.insert(id, size);
        session.channel_success(id)?;
        Ok(())
    }

    async fn shell_request(
        &mut self,
        id: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let Some(peer) = self.live_peer().await else {
            session.channel_failure(id)?;
            return Ok(());
        };
        if !peer.grant_control {
            applog!("[peer] shell 拒绝: {} 未授予控制权限", peer.name);
            session.channel_failure(id)?;
            return Ok(());
        }
        let channel = match self.channels.lock().await.remove(&id) {
            Some(c) => c,
            None => {
                session.channel_failure(id)?;
                return Ok(());
            }
        };
        let size = self
            .pty_sizes
            .remove(&id)
            .unwrap_or_else(|| control::clamp_size(0, 0));
        let require_hello = self.require_windows_hello().await;

        // 本机确认 + 可选 Hello：**成功后才 channel_success**，拒绝直接 Failure。
        // 这里会阻塞本连接的消息循环（最长 30s + Hello），因此每条控制会话独立连接。
        let approved = control::approve(
            &self.shared.control,
            &self.shared.app,
            &peer,
            self.peer_addr,
            size,
            require_hello,
        )
        .await;
        if let Err(e) = approved {
            applog!("[peer] 控制请求未通过: {e}");
            session.channel_failure(id)?;
            return Ok(());
        }
        session.channel_success(id)?;
        let addr_text = self
            .peer_addr
            .map(|a| a.ip().to_string())
            .unwrap_or_default();
        // 运行中每 5s 复核控制授权：撤销后立刻断开
        let grant = GrantCheck::new(self.shared.peers.clone(), peer.id.clone());
        match control::start(
            self.shared.app.clone(),
            self.shared.control.clone(),
            peer,
            addr_text,
            size,
            channel.into_stream(),
            grant,
        ) {
            Ok(resize_tx) => {
                self.ctrl_resize.insert(id, resize_tx);
            }
            Err(e) => applog!("[peer] 启动 ConPTY 失败: {e}"),
        }
        Ok(())
    }

    async fn window_change_request(
        &mut self,
        id: ChannelId,
        cols: u32,
        rows: u32,
        _pix_w: u32,
        _pix_h: u32,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        let size = control::clamp_size(
            cols.min(u16::MAX as u32) as u16,
            rows.min(u16::MAX as u32) as u16,
        );
        match self.ctrl_resize.get(&id) {
            // resize 幂等：通道满（try_send 失败）就丢弃本次，等下一次窗口变化
            Some(tx) => {
                let _ = tx.try_send(size);
            }
            None => {
                self.pty_sizes.insert(id, size);
            }
        }
        Ok(())
    }

    async fn channel_close(
        &mut self,
        id: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.channels.lock().await.remove(&id);
        self.pty_sizes.remove(&id);
        self.ctrl_resize.remove(&id);
        Ok(())
    }
}

/// 服务端配置：只允许 publickey；认证拒绝固定耗时（防时序侧信道）。
pub fn build_config(identity: &Identity) -> Arc<russh::server::Config> {
    Arc::new(russh::server::Config {
        keys: vec![identity.private_key().clone()],
        methods: MethodSet::from(&[MethodKind::PublicKey][..]),
        auth_rejection_time: Duration::from_secs(2),
        auth_rejection_time_initial: Some(Duration::ZERO),
        ..Default::default()
    })
}
