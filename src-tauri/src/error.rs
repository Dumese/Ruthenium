use serde::Serialize;

/// 统一错误模型。序列化后形如 {"code":"E_CONFIG_IO","detail":"..."}
#[derive(Debug, thiserror::Error, Serialize)]
#[serde(tag = "code", content = "detail")]
pub enum AppError {
    #[serde(rename = "E_CONFIG_IO")]
    #[error("配置文件读写失败: {0}")]
    EConfigIo(String),
    #[serde(rename = "E_INVALID_INPUT")]
    #[error("输入无效: {0}")]
    EInvalidInput(String),
    #[serde(rename = "E_NOT_FOUND")]
    #[error("对象不存在: {0}")]
    ENotFound(String),
    #[serde(rename = "E_SSH_AUTH")]
    #[error("SSH 认证失败: {0}")]
    ESshAuth(String),
    #[serde(rename = "E_SSH_CONNECT")]
    #[error("SSH 连接失败: {0}")]
    ESshConnect(String),
    #[serde(rename = "E_SSH_CHANNEL")]
    #[error("SSH 通道错误: {0}")]
    ESshChannel(String),
    /// @revoked 命中单独成码——与「指纹变更待用户确认」区分开，
    /// 前端按码提示且不得把它当“可信任重试”的变更流程处理。
    #[serde(rename = "E_SSH_HOSTKEY_REVOKED")]
    #[error("服务器主机密钥已被吊销（@revoked），连接已阻止")]
    ESshHostkeyRevoked(String),
    #[serde(rename = "E_SFTP_IO")]
    #[error("SFTP 操作失败: {0}")]
    ESftpIo(String),
    #[serde(rename = "E_FS")]
    #[error("本地文件操作失败: {0}")]
    EFs(String),
    #[serde(rename = "E_SECRET")]
    #[error("钥匙串不可用: {0}")]
    ESecret(String),
    #[serde(rename = "E_TUNNEL_BIND")]
    #[error("转发端口绑定失败: {0}")]
    ETunnelBind(String),
    /// 连接/认证被用户取消（关闭仍在 connecting 的标签）。
    /// 前端对 E_CANCELLED 静默处理：不弹 Toast、不触发自动重连。
    #[serde(rename = "E_CANCELLED")]
    #[error("操作已取消")]
    ECancelled(String),
    // ---------- 局域网对等层（peer） ----------
    #[serde(rename = "E_PEER_LISTEN")]
    #[error("局域网监听失败: {0}")]
    EPeerListen(String),
    #[serde(rename = "E_PEER_PORT_BUSY")]
    #[error("局域网端口被占用: {0}")]
    EPeerPortBusy(String),
    #[serde(rename = "E_PEER_WINDOW_CLOSED")]
    #[error("可发现窗口已关闭: {0}")]
    EPeerWindowClosed(String),
    #[serde(rename = "E_PEER_NOT_PAIRED")]
    #[error("设备未配对: {0}")]
    EPeerNotPaired(String),
    #[serde(rename = "E_PEER_KEY_MISMATCH")]
    #[error("对端身份密钥不匹配（请重新配对）: {0}")]
    EPeerKeyMismatch(String),
    #[serde(rename = "E_PEER_NOT_ALLOWED")]
    #[error("对方未授权该能力: {0}")]
    EPeerNotAllowed(String),
    #[serde(rename = "E_PEER_OFFLINE")]
    #[error("对端不可达: {0}")]
    EPeerOffline(String),
    #[serde(rename = "E_PEER_PAIR_REJECTED")]
    #[error("配对被拒绝或数字不一致: {0}")]
    EPeerPairRejected(String),
    #[serde(rename = "E_PEER_PAIR_TIMEOUT")]
    #[error("配对超时: {0}")]
    EPeerPairTimeout(String),
    #[serde(rename = "E_PEER_RATE_LIMITED")]
    #[error("请求过于频繁: {0}")]
    EPeerRateLimited(String),
    #[serde(rename = "E_PEER_HELLO_DENIED")]
    #[error("Windows Hello 验证未通过: {0}")]
    EPeerHelloDenied(String),
    #[serde(rename = "E_PEER_CONSENT_TIMEOUT")]
    #[error("本机未确认操作: {0}")]
    EPeerConsentTimeout(String),
    #[serde(rename = "E_PEER_SFTP_ROOT")]
    #[error("路径超出共享目录范围: {0}")]
    EPeerSftpRoot(String),
    #[serde(rename = "E_PEER_INBOX_INVALID")]
    #[error("收到的配置无效: {0}")]
    EPeerInboxInvalid(String),
}

impl From<std::io::Error> for AppError {
    fn from(e: std::io::Error) -> Self {
        AppError::EFs(e.to_string())
    }
}

impl From<keyring::Error> for AppError {
    fn from(e: keyring::Error) -> Self {
        AppError::ESecret(e.to_string())
    }
}

/// 统一返回类型：命令层与内部模块均使用 `AppResult<T>`。
pub type AppResult<T> = Result<T, AppError>;
