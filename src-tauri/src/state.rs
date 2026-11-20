use std::sync::Arc;

use tauri::AppHandle;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::config;
use crate::error::AppResult;
use crate::local::LocalManager;
use crate::models::Config;
use crate::peer::PeerService;
use crate::secret;
use crate::sftp::SftpManager;
use crate::ssh::SshManager;
use crate::tunnel::TunnelManager;

/// 挂起的 hostkey 决策：握手挂起等待用户「信任/拒绝」。
pub struct PendingHostKey {
    pub host: String,
    pub port: u16,
    pub fingerprint: String,
    pub public_key: String, // OpenSSH 格式，信任时写入 known_hosts
    pub decision_tx: tokio::sync::oneshot::Sender<bool>,
}

/// 原生文件对话框选中、待 export/import 命令消费的路径。
///
/// 备份导出/导入只接受「对话框刚刚选中」的路径：token 一次性、10 分钟过期。
/// 这样即使 WebView 内出现脚本执行，也无法让后端读写任意路径的 JSON。
pub struct DialogPath {
    pub path: std::path::PathBuf,
    pub created: std::time::Instant,
}

/// token 有效期：对话框选择后给用户留出确认/操作的时间。
const DIALOG_TOKEN_TTL: std::time::Duration = std::time::Duration::from_secs(600);

/// 挂起指纹决策表：AppState 与 ClientHandler 共享（SSH 核心不依赖 tauri）。
pub type PendingHostKeyMap = Arc<Mutex<std::collections::HashMap<String, PendingHostKey>>>;

/// 应用全局状态：配置、各会话管理器、挂起指纹表与启动时的配置错误。
pub struct AppState {
    pub app: AppHandle,
    pub config: Arc<tokio::sync::RwLock<Config>>,
    pub ssh: Arc<SshManager>,
    /// 本地终端（Ctrl+T）会话表，与 SSH 会话表并列、互不混用。
    pub local: LocalManager,
    pub sftp: SftpManager,
    pub tunnels: TunnelManager,
    /// 局域网对等层（可见性/监听/设备表），见 peer/mod.rs.
    pub peer: PeerService,
    /// 屏幕查看端会话（远程桌面）
    pub screen: crate::peer::rd::ViewerManager,
    /// 远程操作输入通道
    pub input: crate::peer::input::InputManager,
    /// 剪贴板同步通道
    pub clipboard: crate::peer::clipboard::ClipboardManager,
    /// 声音播放通道
    pub audio: crate::peer::audio::AudioManager,
    pub pending_hostkey: PendingHostKeyMap,
    /// 进行中的传输 → 取消令牌
    pub transfers: Mutex<std::collections::HashMap<String, tokio_util::sync::CancellationToken>>,
    /// 进行中的连接尝试 → 取消令牌。
    /// 前端在建立"临时标签"时生成 attemptId 并随 connect / connect_transient / reconnect
    /// 传入；关闭该标签时调用 `cancel_connect`，握手/认证立即中止，
    /// 不再出现"用户关了标签、后台还在偷偷连"的情况。
    pub connect_cancels: Mutex<std::collections::HashMap<String, CancellationToken>>,
    /// 指标采样任务表
    pub metrics: crate::metrics::MetricsManager,
    /// 原生对话框选中的备份路径：token → 路径 + 时间，一次性消费。
    pub dialog_paths: Mutex<std::collections::HashMap<String, DialogPath>>,
    pub keyring_service: &'static str,
    /// 启动时配置读取失败的详情（Ok 时 None）。
    /// 配置损坏不应让应用起不来：用默认配置继续启动，把这个错误交给前端提示「重建默认配置」。
    /// 也不在启动时覆盖磁盘文件（不静默覆盖），只有用户真的改动偏好后才会原子写回。
    pub config_error: Option<String>,
}

impl AppState {
    /// 读取配置并构造全局状态；配置损坏时回退默认配置并把错误留给前端提示。
    pub fn new(app: AppHandle) -> AppResult<Self> {
        let (config, config_error) = match config::load_config_from_disk() {
            Ok(c) => (c, None),
            Err(e) => {
                let msg = e.to_string();
                // 先把损坏文件另存一份（避免后续偏好保存把用户数据静默覆盖掉）
                let outcome = config::preserve_corrupt_config();
                let detail = match (&outcome.backed_up_to, &outcome.warning) {
                    (Some(p), _) => format!("{msg}（原文件已备份为 {p}，本次以默认配置启动）"),
                    (None, Some(w)) => format!("{msg}（本次以默认配置启动。{w}）"),
                    (None, None) => format!("{msg}（本次以默认配置启动）"),
                };
                applog!("[config] 读取配置失败，已回退默认配置：{detail}");
                (config::default_config(), Some(detail))
            }
        };
        let app_for_peer = app.clone();
        Ok(Self {
            app,
            config: Arc::new(tokio::sync::RwLock::new(config)),
            ssh: Arc::new(SshManager::new()),
            local: LocalManager::new(),
            sftp: SftpManager::new(),
            tunnels: TunnelManager::new(),
            peer: PeerService::new(app_for_peer),
            screen: crate::peer::rd::ViewerManager::new(),
            input: crate::peer::input::InputManager::new(),
            clipboard: crate::peer::clipboard::ClipboardManager::new(),
            audio: crate::peer::audio::AudioManager::new(),
            pending_hostkey: Arc::new(Mutex::new(std::collections::HashMap::new())),
            transfers: Mutex::new(std::collections::HashMap::new()),
            connect_cancels: Mutex::new(std::collections::HashMap::new()),
            metrics: crate::metrics::MetricsManager::new(),
            dialog_paths: Mutex::new(std::collections::HashMap::new()),
            keyring_service: secret::SERVICE,
            config_error,
        })
    }

    /// 修改配置并原子落盘；落盘失败时回滚内存。
    pub async fn mutate_config<F>(&self, f: F) -> AppResult<()>
    where
        F: FnOnce(&mut Config),
    {
        let mut guard = self.config.write().await;
        let snapshot: Config = guard.clone();
        f(&mut guard);
        let serialized = match config::serialize_config(&guard) {
            Ok(s) => s,
            Err(e) => {
                *guard = snapshot;
                return Err(e);
            }
        };
        if let Err(e) = config::atomic_write(&config::config_file_path()?, &serialized) {
            *guard = snapshot;
            return Err(e);
        }
        Ok(())
    }

    // ---- 进行中连接的取消 ----

    /// 注册一次连接尝试并返回取消令牌（同一 attemptId 重复注册会取消旧令牌）。
    pub async fn register_connect_attempt(&self, attempt_id: &str) -> CancellationToken {
        let token = CancellationToken::new();
        let mut map = self.connect_cancels.lock().await;
        if let Some(old) = map.insert(attempt_id.to_string(), token.clone()) {
            old.cancel();
        }
        token
    }

    /// 连接尝试结束（成功/失败/被取消）后清理，避免表只增不减。
    ///
    /// 必须比较令牌后再移除：同一 attemptId 可能已被新的重连尝试重新注册
    /// （register 会 cancel 旧令牌并替换映射），旧尝试收尾时若无条件 remove，
    /// 会把新尝试的取消令牌摘掉，导致“关标签取消在途连接”失效。
    pub async fn finish_connect_attempt(&self, attempt_id: &str, token: &CancellationToken) {
        let mut map = self.connect_cancels.lock().await;
        if map.get(attempt_id).is_some_and(|t| t == token) {
            map.remove(attempt_id);
        }
    }

    /// 取消进行中的连接尝试；返回是否真的存在并已取消。
    pub async fn cancel_connect_attempt(&self, attempt_id: &str) -> bool {
        match self.connect_cancels.lock().await.remove(attempt_id) {
            Some(token) => {
                token.cancel();
                true
            }
            None => false,
        }
    }

    // ---- 对话框路径 token ----

    /// 登记一个对话框选中的路径，返回一次性 token。
    pub async fn register_dialog_path(&self, path: std::path::PathBuf) -> String {
        let token = uuid::Uuid::new_v4().to_string();
        let mut map = self.dialog_paths.lock().await;
        // 顺手清理过期条目，表不随历史选择无限增长。
        map.retain(|_, v| v.created.elapsed() < DIALOG_TOKEN_TTL);
        map.insert(
            token.clone(),
            DialogPath {
                path,
                created: std::time::Instant::now(),
            },
        );
        token
    }

    /// 消费一个 token 取回路径：一次性（无论成败都移除）、过期即失效。
    pub async fn consume_dialog_path(&self, token: &str) -> Option<std::path::PathBuf> {
        let mut map = self.dialog_paths.lock().await;
        match map.remove(token) {
            Some(entry) if entry.created.elapsed() < DIALOG_TOKEN_TTL => Some(entry.path),
            _ => None,
        }
    }
}
