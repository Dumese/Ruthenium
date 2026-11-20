use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::{AppError, AppResult};
use crate::models::{Config, Host, Preferences};

/// 当前配置格式版本：解析时只接受该值。
pub const CONFIG_VERSION: u32 = 1;

/// {config_dir}/Ruthenium 目录
pub fn config_dir() -> AppResult<PathBuf> {
    // 测试/开发：允许用独立数据目录跑第二个实例。
    // 环境变量对普通用户不可见，不影响正常行为；空值按未设置处理。
    if let Some(dir) = std::env::var_os("RUTHENIUM_DATA_DIR") {
        if !dir.is_empty() {
            return Ok(PathBuf::from(dir));
        }
    }
    let base = dirs::config_dir()
        .ok_or_else(|| AppError::EConfigIo("无法确定系统配置目录".to_string()))?;
    Ok(base.join("Ruthenium"))
}

/// 配置文件 `hosts.toml` 的完整路径（不保证文件存在）。
pub fn config_file_path() -> AppResult<PathBuf> {
    Ok(config_dir()?.join("hosts.toml"))
}

/// 损坏配置备份的结果：`backed_up_to` 是备份文件路径；`warning` 在**备份失败**时
/// 携带必须告知用户的警告（原文件仍在原地，但在修复前不应保存偏好，否则会被覆盖）。
pub struct PreserveOutcome {
    pub backed_up_to: Option<String>,
    pub warning: Option<String>,
}

/// 把损坏的 hosts.toml 另存为 `hosts.toml.corrupt-<时间戳>`（只保留最近 CORRUPT_KEEP 份）。
///
/// 为什么必须这样做：回退默认配置后，前端任何一次偏好保存都会把默认配置原子写回
/// `hosts.toml`，等于**静默覆盖**用户的真实数据，这是必须避免的。
/// 先复制一份保留现场，用户仍可在目录里找到并手工修复。
pub fn preserve_corrupt_config() -> PreserveOutcome {
    let Some(src) = config_file_path().ok().filter(|p| p.exists()) else {
        return PreserveOutcome {
            backed_up_to: None,
            warning: None,
        };
    };
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let dest = src.with_file_name(format!("hosts.toml.corrupt-{stamp}"));
    match std::fs::copy(&src, &dest) {
        Ok(_) => {
            prune_corrupt_backups(&src);
            PreserveOutcome {
                backed_up_to: Some(dest.to_string_lossy().to_string()),
                warning: None,
            }
        }
        Err(e) => {
            // 备份失败必须让用户知道后果：原文件未被保留现场，修复前保存偏好会覆盖它。
            applog!("[config] 备份损坏配置失败: {e}");
            PreserveOutcome {
                backed_up_to: None,
                warning: Some(format!(
                    "原文件未能备份（{e}）。请先手工复制 hosts.toml，再继续使用；\
                     在修复前保存任何偏好都会用默认配置覆盖它"
                )),
            }
        }
    }
}

/// 损坏备份的保留份数：旧文件按时间戳命名排序后删到上限，避免每次启动堆积。
const CORRUPT_KEEP: usize = 3;

fn prune_corrupt_backups(config_path: &Path) {
    let Some(dir) = config_path.parent() else {
        return;
    };
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut backups: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("hosts.toml.corrupt-"))
        })
        .collect();
    if backups.len() <= CORRUPT_KEEP {
        return;
    }
    // 时间戳毫秒命名：字典序即时间序，删最旧的。
    backups.sort();
    let excess = backups.len() - CORRUPT_KEEP;
    for old in backups.into_iter().take(excess) {
        let _ = fs::remove_file(old);
    }
}

/// 默认配置
pub fn default_config() -> Config {
    Config {
        version: CONFIG_VERSION,
        preferences: Preferences::default(),
        folders: Vec::new(),
        hosts: Vec::new(),
        snippets: Vec::new(),
        tunnels: Vec::new(),
    }
}

/// 通用原子落盘：写 uuid 临时文件 → fsync →（可选）copy 保留上一版 → 单次 rename 替换。
///
/// - `backup = Some(ext)` 时把旧文件复制为 `<文件名>.<ext>`（best-effort，失败不影响写入）；
///   配置文件传 `Some("bak")`（→ `hosts.toml.bak`），导出等用户自选文件传 `None`（不产生额外文件）。
/// - 临时文件名带 uuid：并发的两次写入不会互相覆盖 tmp；rename 失败时主动清理，不留残留。
/// - 备份用 copy 而不是先把主文件 rename 走：两步 rename 之间存在「主文件从磁盘消失」
///   的窗口，进程恰好被杀时下次启动会被当成首次运行写回默认配置（静默丢用户数据）。
/// - rename 覆盖：Windows std 走 MoveFileEx(REPLACE_EXISTING)。
pub fn write_atomic(path: &Path, content: &str, backup: Option<&str>) -> AppResult<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|e| {
                AppError::EConfigIo(format!("创建目录 {} 失败: {e}", parent.display()))
            })?;
        }
    }
    let tmp = path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4()));
    let attempt = || -> AppResult<()> {
        let mut f = fs::File::create(&tmp)
            .map_err(|e| AppError::EConfigIo(format!("写入 {} 失败: {e}", tmp.display())))?;
        f.write_all(content.as_bytes())
            .map_err(|e| AppError::EConfigIo(format!("写入 {} 失败: {e}", tmp.display())))?;
        f.sync_all()
            .map_err(|e| AppError::EConfigIo(format!("fsync 失败: {e}")))?;
        drop(f);
        if let Some(ext) = backup {
            if path.exists() {
                let mut name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                name.push('.');
                name.push_str(ext);
                let _ = fs::copy(path, path.with_file_name(name));
            }
        }
        fs::rename(&tmp, path)
            .map_err(|e| AppError::EConfigIo(format!("重命名 {} 失败: {e}", tmp.display())))?;
        Ok(())
    };
    let result = attempt();
    if result.is_err() {
        let _ = fs::remove_file(&tmp); // 失败清理临时文件
    }
    result
}

/// 配置文件原子写入（保留一份 `hosts.toml.bak` 上一版；NTFS ACL 语义，
/// 不做任何权限设置；写入一律原子替换）。
pub fn atomic_write(path: &Path, content: &str) -> AppResult<()> {
    write_atomic(path, content, Some("bak"))
}

/// 把配置序列化为磁盘 TOML（字段名为 camelCase，与 IPC JSON 一致）。
pub fn serialize_config(config: &Config) -> AppResult<String> {
    toml::to_string_pretty(config).map_err(|e| AppError::EConfigIo(format!("TOML 序列化失败: {e}")))
}

/// 修复重复/空 id 时使用确定性 seed，避免每次启动换 id 导致钥匙串密码失效。
fn stable_repaired_id(seed: &str) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, seed.as_bytes());
    let hex: String = digest.as_ref()[..12]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("repaired-{hex}")
}

/// 解析 TOML。version 只接受 1，其他值返回 E_INVALID_INPUT。
/// 配置缺少 `auth` 时按 `key` 处理（serde default）；同时归一化
/// `auth != key` 的主机，清掉无用的 `key_ref`。
pub fn parse_config(text: &str) -> AppResult<Config> {
    let mut config: Config =
        toml::from_str(text).map_err(|e| AppError::EInvalidInput(format!("TOML 解析失败: {e}")))?;
    if config.version != CONFIG_VERSION {
        return Err(AppError::EInvalidInput(format!(
            "不支持的配置版本: {}（当前仅支持 {}）",
            config.version, CONFIG_VERSION
        )));
    }
    // 手改 hosts.toml 可能写出越界数值：统一夹到设计范围。
    config.preferences.clamp_to_ranges();
    // host.id 是删除 / 更新 / 钥匙串凭据 / 前端列表 key 的唯一标识。手改配置最常见的
    // 错误是「复制一段 [[hosts]] 新增主机却忘改 id」——重复 id 会造成两类真实故障：
    //   ① delete_host 用 retain(|h| h.id != id) 实现，删一台会把同 id 的另一台一起删掉
    //      （用户看到的是「删了一台，另一台也消失了」）；
    //   ② 密码按 id 存进 Windows 凭据管理器，两台不同主机共用一个 id 就会共用同一个密码，
    //      连接 B 时可能把 A 的密码发出去。
    // 因此在唯一读入口处保证 id 唯一：保留首次出现的那条不动（它的凭据仍然有效），
    // 后续重复项回填新 UUID（与「空 id 回填」同一策略）。repair 只作用于内存，
    // 下次用户真正改动配置时随原子写落盘。
    let mut seen_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (index, host) in config.hosts.iter_mut().enumerate() {
        if host.id.is_empty() || !seen_ids.insert(host.id.clone()) {
            // 必须是确定性 id：随机 UUID 每次加载都会变，用户给该主机保存的
            // 密码下次启动就找不到（凭据按 id 存）。
            let fresh = stable_repaired_id(&format!(
                "{}|{}|{}|{}|{index}",
                host.name, host.addr, host.user, host.port
            ));
            applog!(
                "[config] 主机 id 重复或为空（{} · {}），已回填新 id {}",
                host.name,
                host.addr,
                fresh
            );
            host.id = fresh;
            seen_ids.insert(host.id.clone());
        }
        host.normalize_auth();
    }
    Ok(config)
}

/// 读取 hosts.toml；不存在则生成默认配置并写盘。
pub fn load_config_from_disk() -> AppResult<Config> {
    let path = config_file_path()?;
    if !path.exists() {
        let config = default_config();
        save_config_to_disk(&config)?;
        return Ok(config);
    }
    let text = fs::read_to_string(&path)
        .map_err(|e| AppError::EConfigIo(format!("读取 {} 失败: {e}", path.display())))?;
    parse_config(&text)
}

/// 只读加载配置（启动早期诊断用，无任何写副作用）：
/// 文件不存在 → `Some(default_config())`（**不写盘**，区别于 `load_config_from_disk`）；
/// 读取 / 解析失败 → `None`。适合在单实例插件初始化之前调用（如 main.rs 读 gpuMode），
/// 避免启动尚未完成就先写默认配置。初始化路径仍走 `AppState::new`（有损坏备份机制）。
pub fn read_config_lenient() -> Option<Config> {
    let path = config_file_path().ok()?;
    if !path.exists() {
        return Some(default_config());
    }
    let text = fs::read_to_string(&path).ok()?;
    parse_config(&text).ok()
}

/// 先改内存再落盘（调用方负责回滚内存）。
pub fn save_config_to_disk(config: &Config) -> AppResult<()> {
    let text = serialize_config(config)?;
    atomic_write(&config_file_path()?, &text)
}

/// 从 toml 文本解析出 host 用的辅助：暴露给测试与导入逻辑。
pub fn parse_toml_hosts(text: &str) -> AppResult<Vec<Host>> {
    let config = parse_config(text)?;
    Ok(config.hosts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{AuthMethod, Host, Preferences, TunnelKind, TunnelRule};

    /// 构造一份最小可用 hosts.toml，`host_block` 为 [[hosts]] 下的字段。
    fn config_with_host(host_block: &str) -> String {
        format!(
            r#"version = 1
folders = []
snippets = []
tunnels = []

[preferences]
lang = "zh-CN"
theme = "system"
nav = "classic"
fontSize = 12.5
font = "Consolas"
cursor = "block"
showStatus = true
showDots = true
confirmClose = false
restoreTabs = true
sidebarWidth = 262

[[hosts]]
{host_block}
"#
        )
    }

    fn sample_host(auth: AuthMethod, key_ref: &str) -> Host {
        Host {
            id: "h1".into(),
            name: "web-01".into(),
            addr: "web-01.example.com".into(),
            user: "deploy".into(),
            port: 22,
            group: "生产环境".into(),
            tag: "prod".into(),
            auth,
            key_ref: key_ref.into(),
            known: false,
            agent: false,
            note: String::new(),
            log: false,
            init_command: String::new(),
        }
    }

    #[test]
    fn toml_roundtrip_is_camel_case() {
        let config = Config {
            version: 1,
            preferences: Preferences::default(),
            folders: Vec::new(),
            hosts: vec![Host {
                id: "h1".into(),
                name: "web-01".into(),
                addr: "web-01.example.com".into(),
                user: "deploy".into(),
                port: 22,
                group: "生产环境".into(),
                tag: "prod".into(),
                auth: crate::models::AuthMethod::Key,
                key_ref: "~/.ssh/id_ed25519".into(),
                known: true,
                agent: true,
                note: "主站".into(),
                log: true,
                init_command: "cd /var/log && tail -f app.log".into(),
            }],
            snippets: Vec::new(),
            tunnels: vec![TunnelRule {
                id: "r1".into(),
                name: "本地 → 数据库".into(),
                kind: TunnelKind::Local,
                local: "127.0.0.1:5432".into(),
                remote: "10.20.0.11:5432".into(),
                host: "h1".into(),
                on: true,
            }],
        };
        let text = serialize_config(&config).unwrap();
        // 磁盘 TOML 统一 camelCase
        assert!(text.contains("fontSize"), "fontSize missing:\n{text}");
        assert!(text.contains("sidebarWidth"));
        assert!(text.contains("keyRef"));
        let back = parse_config(&text).unwrap();
        assert_eq!(back.version, 1);
        assert_eq!(back.hosts.len(), 1);
        assert_eq!(back.hosts[0].key_ref, "~/.ssh/id_ed25519");
        assert_eq!(back.tunnels[0].local, "127.0.0.1:5432");
        assert!(matches!(back.tunnels[0].kind, TunnelKind::Local));
        assert_eq!(back.preferences.font_size, 13.0);
        assert_eq!(back.preferences.sidebar_width, 262);
    }

    #[test]
    fn version_must_be_one() {
        let text = "version = 2\n\n[preferences]\ntheme = \"dark\"\nnav = \"classic\"\nfontSize = 12.5\nfont = \"monospace\"\ncursor = \"block\"\nshowStatus = true\nshowDots = true\nconfirmClose = false\nrestoreTabs = true\nsidebarWidth = 262\n";
        let err = parse_config(text).unwrap_err();
        assert!(matches!(err, AppError::EInvalidInput(_)));
    }

    #[test]
    fn app_error_serializes_code_detail() {
        use crate::error::AppError;
        let e = AppError::EConfigIo("boom".to_string());
        let json = serde_json::to_value(&e).unwrap();
        assert_eq!(json["code"], "E_CONFIG_IO");
        assert_eq!(json["detail"], "boom");
        // tag/content 形式，没有其他键
        assert_eq!(json.as_object().unwrap().len(), 2);
        let e2 = AppError::ETunnelBind("port in use".to_string());
        let json2 = serde_json::to_value(&e2).unwrap();
        assert_eq!(json2["code"], "E_TUNNEL_BIND");
    }

    #[test]
    fn default_config_serializes() {
        let config = default_config();
        let text = serialize_config(&config).unwrap();
        assert!(text.contains("version = 1"));
        let back = parse_config(&text).unwrap();
        assert_eq!(back.preferences.font, "JetBrains Mono");
        // 默认语言 zh-CN
        assert!(matches!(
            back.preferences.lang,
            crate::models::Language::ZhCn
        ));
        assert!(text.contains("lang = \"zh-CN\""));
    }

    /// 含 lang 的 Preferences TOML 往返。
    #[test]
    fn preferences_lang_roundtrip() {
        for (raw, expect) in [
            ("\"zh-CN\"", crate::models::Language::ZhCn),
            ("\"zh-TW\"", crate::models::Language::ZhTw),
            ("\"en\"", crate::models::Language::En),
            ("\"ja\"", crate::models::Language::Ja),
        ] {
            let toml = format!(
                "version = 1\nfolders = []\nhosts = []\nsnippets = []\ntunnels = []\n\n[preferences]\nlang = {raw}\ntheme = \"dark\"\nnav = \"classic\"\nfontSize = 12.5\nfont = \"Cascadia Mono\"\ncursor = \"block\"\nshowStatus = true\nshowDots = true\nconfirmClose = false\nrestoreTabs = true\nsidebarWidth = 262\n"
            );
            let cfg = parse_config(&toml).unwrap();
            assert_eq!(cfg.preferences.lang, expect);
            let back = parse_config(&serialize_config(&cfg).unwrap()).unwrap();
            assert_eq!(back.preferences.lang, expect, "raw = {raw}");
        }
    }

    /// Windows 上已存在目标文件时 rename 覆盖语义（原子落盘）。
    #[test]
    fn atomic_write_overwrites_existing_file() {
        let dir = std::env::temp_dir().join(format!("ruthenium-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("hosts.toml");
        // 第一次写入
        atomic_write(&path, "A").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "A");
        // 第二次写入：目标文件已存在，必须覆盖成功
        atomic_write(&path, "B").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "B");
        // 上一版保留为 .bak（文件名 + ".bak"）
        let bak = dir.join("hosts.toml.bak");
        assert_eq!(std::fs::read_to_string(&bak).unwrap(), "A");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 导出等用户自选文件走 `backup=None`，不得在同目录产生 `.bak`
    /// 或固定名 tmp；成功后目录里只有目标文件。
    #[test]
    fn write_atomic_without_backup_leaves_no_sidecar() {
        let dir = std::env::temp_dir().join(format!(
            "ruthenium-wa-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ruthenium-backup.json");
        write_atomic(&path, "{\"a\":1}", None).unwrap();
        // 预写入一次再覆盖：同样不应产生备份文件
        write_atomic(&path, "{\"a\":2}", None).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"a\":2}");
        let entries: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().collect();
        assert_eq!(entries.len(), 1, "目录里只应有目标文件: {entries:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 写入失败（目标是目录，rename 必败）时临时文件必须被清理。
    #[test]
    fn write_atomic_failure_cleans_tmp() {
        let dir = std::env::temp_dir().join(format!(
            "ruthenium-waf-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("occupied"); // 已存在的目录：rename 文件 → 目录必败
        std::fs::create_dir_all(&target).unwrap();
        assert!(write_atomic(&target, "x", None).is_err());
        let leftovers: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("occupied.tmp-"))
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert!(
            leftovers.is_empty(),
            "失败后不得残留 tmp 文件: {leftovers:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 损坏备份按时间戳只保留最近 CORRUPT_KEEP 份。
    #[test]
    fn prune_corrupt_backups_keeps_recent() {
        let dir = std::env::temp_dir().join(format!(
            "ruthenium-corrupt-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..(CORRUPT_KEEP + 2) {
            std::fs::write(dir.join(format!("hosts.toml.corrupt-{i:06}")), b"x").unwrap();
        }
        // 放一个无关文件：prune 不得误删
        std::fs::write(dir.join("hosts.toml"), b"live").unwrap();
        prune_corrupt_backups(&dir.join("hosts.toml"));
        let mut remaining: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter_map(|e| {
                let n = e.file_name().to_string_lossy().to_string();
                n.starts_with("hosts.toml.corrupt-").then_some(n)
            })
            .collect();
        remaining.sort();
        assert_eq!(remaining.len(), CORRUPT_KEEP, "{remaining:?}");
        // 保留的是最新的（时间戳最大）
        assert_eq!(
            remaining.last().unwrap(),
            &format!("hosts.toml.corrupt-{:06}", CORRUPT_KEEP + 1)
        );
        assert!(dir.join("hosts.toml").exists(), "主文件不得被误删");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// HostMetrics serde camelCase。
    #[test]
    fn host_metrics_serde_camel_case() {
        use crate::models::HostMetrics;
        let m = HostMetrics {
            session_id: "s1".into(),
            sampled_at: 42,
            cpu_pct: 12.5,
            cores: 8,
            load_avg: [0.1, 0.2, 0.3],
            mem_used_gb: 1.0,
            mem_total_gb: 2.0,
            disk_used_gb: 3.0,
            disk_total_gb: 4.0,
            net_rx_mbps: 5.0,
            net_tx_mbps: 6.0,
            disk_read_mbps: 7.0,
            disk_write_mbps: 8.0,
        };
        let json = serde_json::to_value(&m).unwrap();
        assert_eq!(json["sessionId"], "s1");
        assert_eq!(json["sampledAt"], 42);
        assert_eq!(json["cpuPct"], 12.5);
        assert_eq!(json["memUsedGb"], 1.0);
        assert_eq!(json["netRxMbps"], 5.0);
        assert_eq!(json["diskWriteMbps"], 8.0);
        assert_eq!(json["loadAvg"].as_array().unwrap().len(), 3);
        assert!((json["loadAvg"][1].as_f64().unwrap() - 0.2).abs() < 1e-6);
        let back: HostMetrics = serde_json::from_value(json).unwrap();
        assert_eq!(back.session_id, "s1");
    }

    /// hosts.toml 缺少 auth 字段时按 key 处理，仍能正常加载。
    #[test]
    fn legacy_toml_without_auth_defaults_to_key() {
        let text = config_with_host(
            r#"id = "h1"
name = "web-01"
addr = "web-01.example.com"
user = "deploy"
port = 22
group = "prod"
tag = "prod"
keyRef = '~\.ssh\id_rsa'
known = false
agent = false
note = 'legacy'
"#,
        );
        let cfg = parse_config(&text).expect("旧版 TOML 必须可加载");
        assert_eq!(cfg.hosts.len(), 1);
        assert_eq!(cfg.hosts[0].auth, AuthMethod::Key);
        assert_eq!(cfg.hosts[0].key_ref, r"~\.ssh\id_rsa");
        // 再次落盘时补上 auth 字段，旧文件被无损升级。
        let saved = serialize_config(&cfg).unwrap();
        assert!(saved.contains("auth = \"key\""), "{saved}");
    }

    /// auth=password 往返；key_ref 落盘必须为空串。
    #[test]
    fn password_auth_roundtrip_clears_key_ref() {
        let text = config_with_host(
            r#"id = "h1"
name = "web-01"
addr = "web-01.example.com"
user = "deploy"
port = 22
group = "prod"
tag = "prod"
auth = "password"
keyRef = 'C:\keys\id_rsa'
known = false
agent = false
note = 'pw'
"#,
        );
        let cfg = parse_config(&text).unwrap();
        assert_eq!(cfg.hosts[0].auth, AuthMethod::Password);
        assert_eq!(cfg.hosts[0].key_ref, "", "非 key 认证不得保留私钥路径");

        let saved = serialize_config(&cfg).unwrap();
        assert!(saved.contains("auth = \"password\""), "{saved}");
        assert!(saved.contains("keyRef = \"\""), "{saved}");
        assert!(
            !saved.contains("id_rsa"),
            "落盘不得残留无用私钥路径:\n{saved}"
        );

        let back = parse_config(&saved).unwrap();
        assert_eq!(back.hosts[0].auth, AuthMethod::Password);
        assert_eq!(back.hosts[0].key_ref, "");
    }

    /// 三种认证方式都能从 TOML 往返；只有 key 保留 keyRef。
    #[test]
    fn auth_method_roundtrip_for_all_variants() {
        for (raw, expect) in [
            ("key", AuthMethod::Key),
            ("password", AuthMethod::Password),
            ("agent", AuthMethod::Agent),
        ] {
            let host = sample_host(expect, r"~\.ssh\id_ed25519");
            let mut cfg = default_config();
            cfg.hosts.push(host);
            let saved = serialize_config(&cfg).unwrap();
            assert!(
                saved.contains(&format!("auth = \"{raw}\"")),
                "{raw}: {saved}"
            );
            let back = parse_config(&saved).unwrap();
            assert_eq!(back.hosts[0].auth, expect);
            if expect == AuthMethod::Key {
                assert_eq!(back.hosts[0].key_ref, r"~\.ssh\id_ed25519");
            } else {
                assert_eq!(back.hosts[0].key_ref, "");
            }
        }
    }

    /// IPC JSON 里只有 auth，不存在 password / saved / pw 字段。
    #[test]
    fn host_ipc_json_has_auth_but_no_secret_fields() {
        let host = sample_host(AuthMethod::Password, "");
        let json = serde_json::to_value(&host).unwrap();
        assert_eq!(json["auth"], "password");
        assert_eq!(json["keyRef"], "");
        let obj = json.as_object().unwrap();
        for forbidden in ["password", "saved", "pw"] {
            assert!(
                !obj.contains_key(forbidden),
                "Host 序列化不得包含 {forbidden} 字段: {json}"
            );
        }
        assert!(!json.to_string().contains("PRIVATE KEY"));
        // 缺失 auth 的 IPC payload 默认 key。
        let mut legacy = json.clone();
        legacy.as_object_mut().unwrap().remove("auth");
        let back: Host = serde_json::from_value(legacy).unwrap();
        assert_eq!(back.auth, AuthMethod::Key);
    }

    /// gpuMode 默认 system；显式 off 可往返；缺字段时回退 system。
    #[test]
    fn gpu_mode_default_roundtrip_and_legacy() {
        use crate::models::GpuMode;

        assert_eq!(
            crate::models::Preferences::default().gpu_mode,
            GpuMode::System
        );

        // 无 gpuMode 的 TOML → 默认 system
        let legacy = "version = 1\nfolders = []\nhosts = []\nsnippets = []\ntunnels = []\n\n[preferences]\nlang = \"zh-CN\"\ntheme = \"system\"\nnav = \"classic\"\nfontSize = 12.5\nfont = \"JetBrains Mono\"\ncursor = \"block\"\nshowStatus = true\nshowDots = true\nconfirmClose = false\nrestoreTabs = true\nsidebarWidth = 262\n";
        let cfg = parse_config(legacy).expect("旧配置应可加载");
        assert_eq!(cfg.preferences.gpu_mode, GpuMode::System);

        // off 往返，且磁盘字段名为 camelCase
        let mut c = default_config();
        c.preferences.gpu_mode = GpuMode::Off;
        let text = serialize_config(&c).unwrap();
        assert!(
            text.contains("gpuMode = \"off\""),
            "磁盘字段应为 gpuMode:\n{text}"
        );
        let back = parse_config(&text).unwrap();
        assert_eq!(back.preferences.gpu_mode, GpuMode::Off);
    }

    /// Host.log / initCommand 与 Preferences.logSessions /
    /// keepaliveSecs 的 TOML 往返（磁盘字段与 IPC JSON 均为 camelCase）。
    #[test]
    fn session_log_and_keepalive_fields_roundtrip() {
        let mut cfg = default_config();
        cfg.preferences.log_sessions = true;
        cfg.preferences.keepalive_secs = 0;
        cfg.preferences.auto_reconnect = true;
        cfg.hosts.push(Host {
            log: true,
            init_command: "echo RUTHENIUM_INIT_OK".into(),
            ..sample_host(AuthMethod::Key, r"~\.ssh\id_ed25519")
        });

        let text = serialize_config(&cfg).unwrap();
        assert!(text.contains("logSessions = true"), "{text}");
        assert!(text.contains("keepaliveSecs = 0"), "{text}");
        assert!(text.contains("autoReconnect = true"), "{text}");
        assert!(
            text.contains("initCommand = \"echo RUTHENIUM_INIT_OK\""),
            "{text}"
        );
        assert!(text.contains("log = true"), "{text}");
        assert!(
            !text.contains("init_command"),
            "磁盘字段必须是 initCommand:\n{text}"
        );
        assert!(
            !text.contains("keepalive_secs"),
            "磁盘字段必须是 keepaliveSecs:\n{text}"
        );
        assert!(
            !text.contains("auto_reconnect"),
            "磁盘字段必须是 autoReconnect:\n{text}"
        );

        let back = parse_config(&text).unwrap();
        assert!(back.preferences.log_sessions);
        assert_eq!(back.preferences.keepalive_secs, 0);
        assert!(back.preferences.auto_reconnect);
        assert!(back.hosts[0].log);
        assert_eq!(back.hosts[0].init_command, "echo RUTHENIUM_INIT_OK");

        // IPC JSON 同样是 camelCase（前端直接消费 Host / Preferences）。
        let host_json = serde_json::to_value(&cfg.hosts[0]).unwrap();
        assert_eq!(host_json["log"], true);
        assert_eq!(host_json["initCommand"], "echo RUTHENIUM_INIT_OK");
        let prefs_json = serde_json::to_value(&cfg.preferences).unwrap();
        assert_eq!(prefs_json["logSessions"], true);
        assert_eq!(prefs_json["keepaliveSecs"], 0);
        assert_eq!(prefs_json["autoReconnect"], true);
    }

    /// hosts.toml 缺新字段 → log=false / initCommand="" / logSessions=false /
    /// keepaliveSecs=30（无需迁移即可加载）。
    #[test]
    fn legacy_toml_defaults_for_new_fields() {
        let legacy = config_with_host(
            r#"id = "h1"
name = "web-01"
addr = "web-01.example.com"
user = "deploy"
port = 22
group = "prod"
tag = "prod"
auth = "key"
keyRef = '~\.ssh\id_rsa'
known = false
agent = false
note = 'legacy'
"#,
        );
        let cfg = parse_config(&legacy).expect("旧配置应可加载");
        assert!(!cfg.hosts[0].log);
        assert!(cfg.hosts[0].init_command.is_empty());
        assert!(!cfg.preferences.log_sessions);
        assert_eq!(cfg.preferences.keepalive_secs, 30);
        assert!(!cfg.preferences.auto_reconnect);

        // 缺字段的 IPC payload（快速连接表单等）也回退同样的默认值。
        let host_json = serde_json::json!({
            "id": "h1", "name": "web-01", "addr": "web-01.example.com", "user": "deploy",
            "port": 22, "group": "prod", "tag": "prod", "keyRef": "",
            "known": false, "agent": false, "note": ""
        });
        let host: Host = serde_json::from_value(host_json).unwrap();
        assert!(!host.log);
        assert!(host.init_command.is_empty());

        // 默认值：日志关、keepalive 30s、自动重连关、自动执行空。
        let default = Preferences::default();
        assert!(!default.log_sessions);
        assert_eq!(default.keepalive_secs, 30);
        assert!(!default.auto_reconnect);
    }

    /// 终端行高：默认 1.3；缺字段时回落默认；显式值可往返。
    #[test]
    fn line_height_default_roundtrip_and_legacy() {
        use crate::models::Preferences;

        assert_eq!(Preferences::default().line_height, 1.3);

        let legacy = "version = 1\nfolders = []\nhosts = []\nsnippets = []\ntunnels = []\n\n[preferences]\nlang = \"zh-CN\"\ntheme = \"system\"\nnav = \"classic\"\nfontSize = 12.5\nfont = \"JetBrains Mono\"\ncursor = \"block\"\nshowStatus = true\nshowDots = true\nconfirmClose = false\nrestoreTabs = true\nsidebarWidth = 262\n";
        let cfg = parse_config(legacy).expect("旧配置应可加载");
        assert_eq!(cfg.preferences.line_height, 1.3);

        let mut c = default_config();
        c.preferences.line_height = 1.9;
        let text = serialize_config(&c).unwrap();
        assert!(
            text.contains("lineHeight = 1.9"),
            "磁盘字段应为 lineHeight:\n{text}"
        );
        let back = parse_config(&text).unwrap();
        assert!((back.preferences.line_height - 1.9).abs() < 0.001);
    }

    /// 手改 hosts.toml 缺 fontSize / sidebarWidth 等基础字段 → serde 默认值兜底；
    /// 空 id 的 host 解析成功后回填稳定 ID。
    #[test]
    fn legacy_toml_missing_basic_fields_defaults() {
        // 注意：hosts 用 [[hosts]] 定义，顶层不能再写 hosts = []（TOML 不允许内联数组后追加）。
        let legacy = "version = 1\nfolders = []\nsnippets = []\ntunnels = []\n\n[preferences]\nlang = \"zh-CN\"\n\n[[hosts]]\nid = \"\"\nname = \"hand-edited\"\naddr = \"10.0.0.9\"\n";
        let cfg = parse_config(legacy).expect("缺基础字段的旧 TOML 应可加载");
        assert_eq!(cfg.preferences.font_size, 13.0);
        assert_eq!(cfg.preferences.sidebar_width, 262);
        assert_eq!(cfg.preferences.font, "JetBrains Mono");
        assert!(matches!(
            cfg.preferences.theme,
            crate::models::Theme::System
        ));
        assert!(matches!(
            cfg.preferences.nav,
            crate::models::NavStyle::Classic
        ));
        assert!(cfg.preferences.show_status);
        assert!(cfg.preferences.show_dots);
        assert!(!cfg.preferences.confirm_close);
        assert!(cfg.preferences.restore_tabs);
        // 空 id 回填：解析成功后不再是空串
        assert!(!cfg.hosts[0].id.is_empty(), "空 id 应回填 uuid");
        // Host 基础字段同样兜底
        assert_eq!(cfg.hosts[0].port, 22);
        assert_eq!(cfg.hosts[0].user, "");
    }

    /// 手改 hosts.toml / 导入备份可写入越界数值，解析后必须夹回设计范围
    /// （fontSize=0 会让 xterm 字号非法、sidebarWidth 过大破坏布局、keepalive 过大等同禁用）。
    #[test]
    fn preferences_out_of_range_are_clamped() {
        let text = "version = 1\nfolders = []\nhosts = []\nsnippets = []\ntunnels = []\n\n[preferences]\nfontSize = 200.0\nlineHeight = 0.1\nsidebarWidth = 9000\nkeepaliveSecs = 999999\n";
        let cfg = parse_config(text).expect("越界偏好应可加载（规范化而不是拒绝启动）");
        assert_eq!(cfg.preferences.font_size, 18.0);
        assert_eq!(cfg.preferences.line_height, 1.0);
        assert_eq!(cfg.preferences.sidebar_width, 360);
        assert_eq!(cfg.preferences.keepalive_secs, 3600);

        // 负值 / 0 同样被抬回下限
        let low = "version = 1\nfolders = []\nhosts = []\nsnippets = []\ntunnels = []\n\n[preferences]\nfontSize = -5.0\nlineHeight = 0.0\nsidebarWidth = 0\n";
        let cfg = parse_config(low).expect("越界偏好应可加载");
        assert_eq!(cfg.preferences.font_size, 10.0);
        assert_eq!(cfg.preferences.line_height, 1.0);
        assert_eq!(cfg.preferences.sidebar_width, 210);
        // keepalive 缺省不受影响
        assert_eq!(cfg.preferences.keepalive_secs, 30);
    }

    /// 手改 hosts.toml 复制 `[[hosts]]` 段忘改 id → 解析后必须回填唯一 id。
    /// 重复 id 会让 delete_host（retain 按 id 删）连带删除另一台，并让两台主机共用
    /// 同一个钥匙串密码。首次出现的条目保留原 id（凭据仍有效），重复项换新 uuid。
    #[test]
    fn duplicate_host_ids_are_made_unique() {
        let text = "version = 1\nfolders = []\nsnippets = []\ntunnels = []\n\n[preferences]\nlang = \"zh-CN\"\n\n[[hosts]]\nid = \"dup\"\nname = \"a\"\naddr = \"10.0.0.1\"\n\n[[hosts]]\nid = \"dup\"\nname = \"b\"\naddr = \"10.0.0.2\"\n\n[[hosts]]\nid = \"keep\"\nname = \"c\"\naddr = \"10.0.0.3\"\n";
        let cfg = parse_config(text).expect("重复 id 不应导致启动失败");
        assert_eq!(cfg.hosts.len(), 3, "解析不得丢弃条目");
        assert_eq!(cfg.hosts[0].id, "dup", "首次出现保留原 id（凭据仍可命中）");
        assert_ne!(cfg.hosts[1].id, "dup", "重复项必须换新 id");
        assert_eq!(cfg.hosts[2].id, "keep", "无关条目不得被改动");
        let uniq: std::collections::HashSet<_> = cfg.hosts.iter().map(|h| h.id.clone()).collect();
        assert_eq!(
            uniq.len(),
            3,
            "id 必须唯一: {:?}",
            cfg.hosts.iter().map(|h| &h.id).collect::<Vec<_>>()
        );
        // 换 id 不得影响其余字段
        assert_eq!(cfg.hosts[1].addr, "10.0.0.2");
    }

    /// 空 id 与重复 id 同时存在：全部回填为唯一非空 id。
    #[test]
    fn empty_and_duplicate_host_ids_are_all_unique() {
        let text = "version = 1\nfolders = []\nsnippets = []\ntunnels = []\n\n[preferences]\nlang = \"zh-CN\"\n\n[[hosts]]\nid = \"\"\nname = \"a\"\naddr = \"10.0.0.1\"\n\n[[hosts]]\nid = \"\"\nname = \"b\"\naddr = \"10.0.0.2\"\n\n[[hosts]]\nid = \"x\"\nname = \"c\"\naddr = \"10.0.0.3\"\n\n[[hosts]]\nid = \"x\"\nname = \"d\"\naddr = \"10.0.0.4\"\n";
        let cfg = parse_config(text).expect("空/重复 id 应可加载");
        let ids: Vec<_> = cfg.hosts.iter().map(|h| h.id.clone()).collect();
        assert!(ids.iter().all(|i| !i.is_empty()), "不得残留空 id: {ids:?}");
        let uniq: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(uniq.len(), ids.len(), "id 必须唯一: {ids:?}");
    }
}
