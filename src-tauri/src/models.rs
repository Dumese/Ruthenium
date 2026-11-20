use serde::{Deserialize, Serialize};

/// 落盘的完整配置（hosts.toml / 备份 JSON 的根对象，`version` 固定见 `CONFIG_VERSION`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    pub version: u32, // 当前固定 1
    pub preferences: Preferences,
    pub folders: Vec<Folder>, // 单层，不嵌套、不继承
    pub hosts: Vec<Host>,
    pub snippets: Vec<Snippet>,
    pub tunnels: Vec<TunnelRule>,
}

/// 全局外观与连接偏好；缺字段时按 serde 默认值回退，保证缺失字段的配置可直接加载。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Preferences {
    #[serde(default)]
    pub lang: Language, // zh-CN | zh-TW | en | ja，默认 zh-CN
    #[serde(default = "default_theme")]
    pub theme: Theme, // light | dark | system
    #[serde(default = "default_nav")]
    pub nav: NavStyle, // classic | x
    #[serde(default = "default_font_size")]
    pub font_size: f32, // 10..18，默认 13
    /// 终端行高（字号倍数，1.0–2.0，默认 1.3；渲染时换算成 xterm 的 lineHeight 系数）
    #[serde(default = "default_line_height")]
    pub line_height: f32,
    #[serde(default = "default_font")]
    pub font: String, // "JetBrains Mono" | "IBM Plex Mono" | "Source Code Pro" | "Consolas" | "Monaco"
    #[serde(default = "default_cursor")]
    pub cursor: CursorStyle, // block | bar
    #[serde(default = "default_true")]
    pub show_status: bool, // 默认 true
    #[serde(default = "default_true")]
    pub show_dots: bool, // 默认 true
    /// 终端行号：默认 false；全局外观偏好，不进会话
    #[serde(default)]
    pub show_line_numbers: bool,
    #[serde(default)]
    pub confirm_close: bool, // 默认 false
    #[serde(default = "default_true")]
    pub restore_tabs: bool, // 默认 true
    /// 图形加速：system | off，默认 system
    #[serde(default)]
    pub gpu_mode: GpuMode,
    #[serde(default = "default_sidebar_width")]
    pub sidebar_width: u32, // 210..360，默认 262
    /// 全局默认「记录会话日志到本地文件」；单主机开关见 `Host.log`。
    #[serde(default)]
    pub log_sessions: bool,
    /// SSH keepalive 间隔（秒），默认 30；0 = 禁用。
    #[serde(default = "default_keepalive")]
    pub keepalive_secs: u32,
    /// 断线自动重连（前端策略固定 1/2/4/8/16s、最多 5 次）；默认 false。
    /// 只有 `conn://state.reason = "lost"` 才允许触发，`closed_by_user` 永不触发。
    #[serde(default)]
    pub auto_reconnect: bool,
    /// 启动时的 Windows Hello 身份验证（默认 false，见 `src/hello.rs`）。
    /// 开启后前端在加载配置与会话**之前**先要求系统身份验证；验证不通过时
    /// 主界面不挂载（`src/components/security/HelloGate.tsx`）。
    #[serde(default)]
    pub require_windows_hello: bool,
    /// 局域网可见的**常开开关**（默认 false = 平时不监听、不广播）。
    /// 即使为 false，发起配对/刷新设备时仍会按需打开 5 分钟可发现窗口。
    #[serde(default)]
    pub lan_visible: bool,
    /// 设备名（UDP 应答 / 配对弹窗展示）。空 = 使用主机名。
    #[serde(default)]
    pub lan_device_name: String,
    /// 局域网 SSH 监听端口（默认 27410；0 = 在 27410..27419 内自动选可用端口）。
    #[serde(default = "default_peer_port")]
    pub lan_port: u16,
    /// 对端 SFTP 访问根目录（空 = 全盘：虚拟根 "/" 列出各盘符）。
    #[serde(default)]
    pub peer_share_root: String,
    /// 严格配对：每次配对都要求两端核对 6 位码（默认 false = 一键允许）。
    /// 手填地址始终走数字比对，与此开关无关。
    #[serde(default)]
    pub lan_strict_pairing: bool,
}

/// 局域网对等层默认端口。
pub const DEFAULT_PEER_PORT: u16 = 27410;

fn default_peer_port() -> u16 {
    DEFAULT_PEER_PORT
}

/// keepalive 默认间隔（秒），`0` 表示禁用。
pub const DEFAULT_KEEPALIVE_SECS: u32 = 30;

/// `Host` / `Preferences` 缺省 `keepaliveSecs` 时的 serde 默认值。
pub fn default_keepalive() -> u32 {
    DEFAULT_KEEPALIVE_SECS
}

/// 终端行高默认值：1.3（字号倍数，可调范围 1.0–2.0）
fn default_line_height() -> f32 {
    1.3
}

// ---- 手改 hosts.toml 易缺的基础字段的 serde 默认值（与 Default 保持一致）----

fn default_font_size() -> f32 {
    13.0
}

fn default_font() -> String {
    "JetBrains Mono".to_string()
}

fn default_theme() -> Theme {
    Theme::System
}

fn default_nav() -> NavStyle {
    NavStyle::Classic
}

fn default_cursor() -> CursorStyle {
    CursorStyle::Block
}

/// bool 字段的 serde 默认 true（`#[serde(default)]` 只能给出 false）。
fn default_true() -> bool {
    true
}

fn default_sidebar_width() -> u32 {
    262
}

/// `Host` 缺省 `port` 时的 serde 默认值（22）。
fn default_ssh_port() -> u16 {
    22
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            lang: Language::ZhCn,
            theme: Theme::System,
            nav: NavStyle::Classic,
            font_size: 13.0,
            line_height: default_line_height(),
            font: "JetBrains Mono".to_string(),
            cursor: CursorStyle::Block,
            show_status: true,
            show_dots: true,
            show_line_numbers: false,
            confirm_close: false,
            restore_tabs: true,
            gpu_mode: GpuMode::System,
            sidebar_width: 262,
            log_sessions: false,
            keepalive_secs: default_keepalive(),
            auto_reconnect: false,
            require_windows_hello: false,
            lan_visible: false,
            lan_device_name: String::new(),
            lan_port: default_peer_port(),
            peer_share_root: String::new(),
            lan_strict_pairing: false,
        }
    }
}

/// 偏好数值字段的设计范围（与 SettingsSheet 的滑杆一致）。
pub const FONT_SIZE_RANGE: (f32, f32) = (10.0, 18.0);
pub const LINE_HEIGHT_RANGE: (f32, f32) = (1.0, 2.0);
pub const SIDEBAR_WIDTH_RANGE: (u32, u32) = (210, 360);
/// keepalive 上限 1 小时：再大就失去"及时发现断线"的意义，也避免 Duration 过大。
pub const KEEPALIVE_MAX_SECS: u32 = 3600;

impl Preferences {
    /// 把数值字段夹到设计范围内。
    ///
    /// 前端滑杆只约束 UI 输入；手改 hosts.toml、导入备份 JSON、直接 invoke
    /// save_preferences 都可能写入 0 / 负数 / NaN / 极大值，导致终端渲染异常或
    /// keepalive 实际等同于禁用。集中在这里做一次规范化，三个入口共用。
    pub fn clamp_to_ranges(&mut self) {
        if !self.font_size.is_finite() {
            self.font_size = default_font_size();
        }
        self.font_size = self.font_size.clamp(FONT_SIZE_RANGE.0, FONT_SIZE_RANGE.1);
        if !self.line_height.is_finite() {
            self.line_height = default_line_height();
        }
        self.line_height = self
            .line_height
            .clamp(LINE_HEIGHT_RANGE.0, LINE_HEIGHT_RANGE.1);
        self.sidebar_width = self
            .sidebar_width
            .clamp(SIDEBAR_WIDTH_RANGE.0, SIDEBAR_WIDTH_RANGE.1);
        self.keepalive_secs = self.keepalive_secs.min(KEEPALIVE_MAX_SECS);
        // 设备名进入 UDP 发现报文（2048 上限）与配对 Hello（8 KiB 帧上限）：
        // 不截断会让配对直接失败，这里与前端输入共同约束。
        self.lan_device_name = self.lan_device_name.trim().chars().take(64).collect();
    }
}

/// 界面语言：zh-CN | zh-TW | en | ja，缺省简体中文。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Language {
    #[serde(rename = "zh-CN")]
    #[default]
    ZhCn,
    #[serde(rename = "zh-TW")]
    ZhTw,
    #[serde(rename = "en")]
    En,
    #[serde(rename = "ja")]
    Ja,
}

/// 主题模式：跟随系统或强制亮/暗。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    Light,
    Dark,
    System,
}

/// 侧栏导航样式：classic | x。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NavStyle {
    Classic,
    X,
}

/// 终端光标形状：block | bar。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CursorStyle {
    Block,
    Bar,
}

/// 图形加速：system = 交给 WebView2 自动决定（可用则开），off = 强制禁用。
/// 没有 on —— 可用本来就会开，强制开没有意义。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum GpuMode {
    #[default]
    System,
    Off,
}

/// 主机分组（单层，不嵌套、不继承）；颜色仅用于分组行标记。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Folder {
    pub name: String,
    pub color: String, // 颜色仅用于分组行标记
}

/// 认证方式：随 hosts.toml 落盘，决定认证链走哪一条分支；缺省私钥。
/// 「是否已保存密码」是派生态，由 keyring 的 has_password 决定，不是字段。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum AuthMethod {
    #[default]
    Key,
    Password,
    Agent,
}

/// 一台 SSH 主机；`key_ref` 只在 `auth = key` 时有值，落盘前由 `normalize_auth` 清理。
/// 手改 hosts.toml 易缺的基础字段按下表 serde 默认值回退；id / name / addr 仍必填。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Host {
    pub id: String,   // 稳定 ID，创建后不变（空 id 由 parse_config 回填 uuid）
    pub name: String, // 显示名
    pub addr: String, // HostName / IP
    #[serde(default)]
    pub user: String,
    #[serde(default = "default_ssh_port")]
    pub port: u16, // 默认 22；0 是「本地终端主机」约定（仅前端本地终端用）
    #[serde(default)]
    pub group: String, // 指向 Folder.name，单层
    #[serde(default)]
    pub tag: String, // prod | stage | dev
    /// hosts.toml 缺少该字段时按 key 处理，保证向后兼容。
    #[serde(default)]
    pub auth: AuthMethod, // key | password | agent
    #[serde(default)]
    pub key_ref: String, // "~/.ssh/id_ed25519"；绝不存私钥内容
    #[serde(default)]
    pub known: bool, // 是否已信任指纹（仅 UI 提示，真值以 known_hosts 为准）
    #[serde(default)]
    pub agent: bool, // 是否优先复用 ssh-agent
    #[serde(default)]
    pub note: String,
    /// 把该主机会话的远端原始字节流记录到本地日志文件；默认关。
    #[serde(default)]
    pub log: bool,
    /// 连接成功（PTY/shell 就绪）后自动写入 shell 的一行命令；空串 = 不执行。
    #[serde(default)]
    pub init_command: String,
}

impl Host {
    /// `key_ref` 只在 `auth = key` 时有值。切到 password / agent 后
    /// 必须在落盘前清空，不能留一条永远不会被使用的私钥路径。
    pub fn normalize_auth(&mut self) {
        if self.auth != AuthMethod::Key {
            self.key_ref.clear();
        }
    }
}

/// 可复用命令片段。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snippet {
    pub id: String,
    pub name: String,
    pub cmd: String,
    pub tag: String,
}

/// 一条端口转发规则；`host` 关联 `Host.id`（全局统一用 id，不用 name）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TunnelRule {
    pub id: String,
    pub name: String,
    pub kind: TunnelKind, // local | remote（当前只实现 local）
    pub local: String,    // "127.0.0.1:5432"
    pub remote: String,   // "10.20.0.11:5432"
    pub host: String,     // 关联 Host.id（全局统一用 id，不用 name）
    pub on: bool,
}

/// 转发方向：local | remote（当前只实现 local）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TunnelKind {
    Local,
    Remote,
}

// ---- 远端指标快照（不落盘，仅内存；Rust → React 单向） ----
/// 一帧远端指标快照；差分类字段（CPU/网络/磁盘）在首帧为 0。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostMetrics {
    pub session_id: String,
    pub sampled_at: i64, // Unix 毫秒
    pub cpu_pct: f32,    // 0..100，两次 /proc/stat 差值
    pub cores: u8,
    pub load_avg: [f32; 3],
    pub mem_used_gb: f32,
    pub mem_total_gb: f32,
    pub disk_used_gb: f32,
    pub disk_total_gb: f32,
    pub net_rx_mbps: f32, // MB/s，来自 /proc/net/dev 字节差
    pub net_tx_mbps: f32,
    pub disk_read_mbps: f32, // MB/s，来自 /proc/diskstats 扇区差
    pub disk_write_mbps: f32,
}

// ---- 运行时对象（不落盘） ----
/// 运行时会话句柄：`writer` 接收终端输入，`cancel` 统一用于关闭/取消。
pub struct SessionHandle {
    pub id: String,
    pub host_id: String,
    pub kind: SessionKind, // Term | Sftp | Local
    pub started_at: i64,
    pub writer: tokio::sync::mpsc::Sender<Vec<u8>>, // 终端输入
    pub cancel: tokio_util::sync::CancellationToken, // 关闭/取消
    /// 会话级编码状态：reader task 与 set_session_encoding 共享。
    pub encoding: std::sync::Arc<tokio::sync::Mutex<crate::encoding::SessionEncoding>>,
}

/// 会话类型：SSH 终端 / SFTP / 本地终端 / 对端控制终端。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionKind {
    Term,
    Sftp,
    /// 本地终端（Ctrl+T）：ConPTY 驱动的交互式 shell，无 host。
    Local,
    /// 对端控制终端：`host_id` 是设备 id（不是主机），标签副标题「控制 · <设备名>」。
    Peer,
}

/// list_sessions 返回的可序列化会话摘要（前端 Session 类型）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub id: String,
    pub host_id: String,
    pub kind: SessionKind,
    pub connected: bool,
    pub started_at: i64,
    pub encoding: String, // 会话级编码，默认 "UTF-8"
}

/// import_ssh_config 的预览条目（`duplicate` 标记同 addr+port 已存在）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParsedHost {
    pub name: String,
    pub addr: String,
    pub user: String,
    pub port: u16,
    pub key_ref: String,
    pub group: String,
    pub tag: String,
    pub note: String,
    pub duplicate: bool,
}

/// SFTP / 本地文件条目（`modified` 为 Unix 秒，目录的 size 为 None）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: Option<u64>,
    pub modified: Option<i64>,
    /// POSIX 权限位（如 0o644 / 0o755）；Windows 本地目录为 None。
    pub mode: Option<u32>,
}

/// get_app_info 返回值：版本、配置目录、日志目录与启动时的配置错误。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppInfo {
    pub version: String,
    pub config_dir: String,
    /// 会话日志目录（`<config_dir>/logs`）：前端「打开/清空日志」用，避免前端拼路径。
    pub logs_dir: String,
    pub os: String,
    /// 启动时配置读取失败的原因（正常为 None）。前端据此提示「配置损坏，已用默认配置启动」。
    pub config_error: Option<String>,
    /// 是否低配设备（≤2 逻辑核 / 环境变量强制）：前端据此自动关闭 xterm WebGL 渲染器。
    pub low_end: bool,
}
