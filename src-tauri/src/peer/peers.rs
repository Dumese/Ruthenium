//! 已配对设备表（`peers.toml`，与 `hosts.toml` 同目录）。
//!
//! 设计要点：
//! - 每台设备一行，私钥永远不在这里（只有对端公钥）；
//! - 六项授权（文件/控制/配置/屏幕/剪贴板/声音）**默认全 false**，由配对弹窗显式勾选；
//! - 写入复用 `config::atomic_write`（临时文件 + 替换 + `.bak`），坏文件先隔离
//!   为 `peers.toml.corrupt-<时间戳>` 再按空表启动，绝不静默覆盖用户数据。

use std::path::{Path, PathBuf};

use russh::keys::PublicKey;
use serde::{Deserialize, Serialize};

use crate::config::{atomic_write, config_dir};
use crate::error::{AppError, AppResult};

/// 一台已配对设备。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerRecord {
    /// 本机分配的设备 id（删除/改名不影响钉扎）。
    pub id: String,
    /// 对端展示名。
    pub name: String,
    /// 对端长期身份公钥（OpenSSH 公钥行）。
    pub pubkey: String,
    /// 最近一次可用地址（`IP:端口`，发现层会刷新）。
    #[serde(default)]
    pub last_addr: Option<String>,
    #[serde(default)]
    pub paired_at: String,
    #[serde(default)]
    pub last_seen: Option<String>,
    /// 本机允许对方：文件互传 / 控制本机终端 / 推送配置 / 查看本机屏幕 / 剪贴板同步（默认全 false）。
    #[serde(default)]
    pub grant_files: bool,
    #[serde(default)]
    pub grant_control: bool,
    #[serde(default)]
    pub grant_config: bool,
    /// 允许对方查看本机屏幕（远程桌面）
    #[serde(default)]
    pub grant_screen: bool,
    /// 允许查看会话同步剪贴板文本（默认关，仅文本）
    #[serde(default)]
    pub grant_clipboard: bool,
    /// 允许查看会话播放本机系统声音（loopback，默认关）
    #[serde(default)]
    pub grant_audio: bool,
    /// 对方允许本机（配对时收到的快照，仅供展示；**不作为拒绝依据**——
    /// 授权可在配对后由对方随时开关，实际判定以对端 `ruthenium-info` 与服务端
    /// 每次请求读取的 `grant_*` 为准，连接每个能力时都会刷新这里）。
    #[serde(default)]
    pub peer_allows_files: bool,
    #[serde(default)]
    pub peer_allows_control: bool,
    #[serde(default)]
    pub peer_allows_config: bool,
    #[serde(default)]
    pub peer_allows_screen: bool,
    #[serde(default)]
    pub peer_allows_clipboard: bool,
    #[serde(default)]
    pub peer_allows_audio: bool,
}

/// `peers.toml` 顶层结构。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeersFile {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub peers: Vec<PeerRecord>,
}

fn default_version() -> u32 {
    1
}

impl Default for PeersFile {
    fn default() -> Self {
        Self {
            version: 1,
            peers: Vec::new(),
        }
    }
}

pub fn peers_path() -> AppResult<PathBuf> {
    Ok(config_dir()?.join("peers.toml"))
}

/// 读取设备表。文件缺失 → 空表；解析失败 → 隔离坏文件后返回空表（不覆盖）。
pub fn load() -> AppResult<PeersFile> {
    load_from(&peers_path()?)
}

pub fn load_from(path: &Path) -> AppResult<PeersFile> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(PeersFile::default()),
        Err(e) => return Err(AppError::EConfigIo(format!("读取 {}: {e}", path.display()))),
    };
    match toml::from_str::<PeersFile>(&text) {
        Ok(mut f) => {
            if f.version != 1 {
                return Err(AppError::EConfigIo(format!(
                    "不支持的 peers.toml 版本 {}（仅支持 1）",
                    f.version
                )));
            }
            // 手改可能留下空 id / 重复 id：与 hosts.toml 同一策略，回填唯一 id。
            let mut seen = std::collections::HashSet::new();
            for (index, p) in f.peers.iter_mut().enumerate() {
                if p.id.is_empty() || !seen.insert(p.id.clone()) {
                    // 与 hosts.toml 同理：随机 id 每次加载都变，会让手改/重复
                    // 条目的身份不稳定。用公钥+名字+序号做确定性 seed。
                    let seed = format!(
                        "{}|{}|{}|{index}",
                        p.pubkey,
                        p.name,
                        p.last_addr.as_deref().unwrap_or("")
                    );
                    let digest = ring::digest::digest(&ring::digest::SHA256, seed.as_bytes());
                    let hex: String = digest.as_ref()[..12]
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect();
                    p.id = format!("repaired-{hex}");
                    seen.insert(p.id.clone());
                }
            }
            Ok(f)
        }
        Err(e) => {
            // 隔离坏文件：保留证据，按空表继续（启动不能因设备表损坏而失败）
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let bad = path.with_file_name(format!("peers.toml.corrupt-{stamp}"));
            let moved = std::fs::rename(path, &bad);
            applog!(
                "[peer] peers.toml 解析失败（{e}），已隔离为 {}（rename ok={}）",
                bad.display(),
                moved.is_ok()
            );
            Ok(PeersFile::default())
        }
    }
}

pub fn save(file: &PeersFile) -> AppResult<()> {
    save_to(&peers_path()?, file)
}

pub fn save_to(path: &Path, file: &PeersFile) -> AppResult<()> {
    let text = toml::to_string_pretty(file)
        .map_err(|e| AppError::EConfigIo(format!("序列化 peers.toml 失败: {e}")))?;
    atomic_write(path, &text)
}

/// 按公钥查设备（比较前两段，忽略注释与 `cert-authority` 之类后缀）。
pub fn find_by_pubkey<'a>(file: &'a PeersFile, key: &PublicKey) -> Option<&'a PeerRecord> {
    let line = key.to_openssh().ok()?;
    file.peers.iter().find(|p| pubkey_matches(&p.pubkey, &line))
}

/// 两个 OpenSSH 公钥行是否描述同一把钥匙（忽略注释）。
pub fn pubkey_matches(a: &str, b: &str) -> bool {
    let body = |s: &str| -> String { s.split_whitespace().take(2).collect::<Vec<_>>().join(" ") };
    !a.trim().is_empty() && body(a) == body(b)
}

/// 插入或替换设备记录（按 id；同公钥也视为同一设备，覆盖旧记录）。
pub fn upsert_in(file: &mut PeersFile, record: PeerRecord) {
    if let Some(pos) = file
        .peers
        .iter()
        .position(|p| p.id == record.id || pubkey_matches(&p.pubkey, &record.pubkey))
    {
        file.peers[pos] = record;
    } else {
        file.peers.push(record);
    }
}

/// 删除设备记录；返回是否删除。
pub fn remove_in(file: &mut PeersFile, id: &str) -> bool {
    let before = file.peers.len();
    file.peers.retain(|p| p.id != id);
    file.peers.len() != before
}

/// 公钥行 → 8 位指纹（与发现报文 / 配对弹窗展示对齐；取 `SHA256:` 之后的 8 个字符）。
pub fn fp_short(pubkey_line: &str) -> String {
    use russh::keys::{HashAlg, PublicKey};
    PublicKey::from_openssh(pubkey_line)
        .ok()
        .map(|k| {
            k.fingerprint(HashAlg::Sha256)
                .to_string()
                .trim_start_matches("SHA256:")
                .chars()
                .take(8)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> PeersFile {
        PeersFile {
            version: 1,
            peers: vec![PeerRecord {
                id: "p1".into(),
                name: "笔记本".into(),
                pubkey: "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAITESTKEY test".into(),
                last_addr: Some("192.168.1.9:27410".into()),
                paired_at: "2026-11-16T10:00:00+08:00".into(),
                last_seen: None,
                grant_files: true,
                grant_control: false,
                grant_config: false,
                grant_screen: false,
                grant_clipboard: false,
                grant_audio: false,
                peer_allows_files: false,
                peer_allows_control: false,
                peer_allows_config: false,
                peer_allows_screen: false,
                peer_allows_clipboard: false,
                peer_allows_audio: false,
            }],
        }
    }

    fn tmp_dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "ruthenium-peers-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn roundtrip_and_defaults() {
        let dir = tmp_dir();
        let path = dir.join("peers.toml");
        save_to(&path, &sample()).unwrap();
        let loaded = load_from(&path).unwrap();
        assert_eq!(loaded.peers.len(), 1);
        assert_eq!(loaded.peers[0].name, "笔记本");
        assert!(loaded.peers[0].grant_files);
        assert!(!loaded.peers[0].grant_control);
        // 未写的授权字段按 false（默认全不勾）
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("grantControl = false") || text.contains("grant_control = false"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_file_is_quarantined_not_overwritten() {
        let dir = tmp_dir();
        let path = dir.join("peers.toml");
        std::fs::write(&path, "this is not toml [[[").unwrap();
        let loaded = load_from(&path).unwrap();
        assert!(loaded.peers.is_empty());
        assert!(!path.exists(), "坏文件应被改名而不是留在原位");
        let quarantined: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("peers.toml.corrupt-")
            })
            .collect();
        assert_eq!(quarantined.len(), 1, "坏文件应保留一份隔离副本");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn duplicate_ids_are_repaired() {
        let dir = tmp_dir();
        let path = dir.join("peers.toml");
        let mut f = sample();
        f.peers.push(f.peers[0].clone());
        save_to(&path, &f).unwrap();
        let loaded = load_from(&path).unwrap();
        assert_eq!(loaded.peers.len(), 2);
        assert_ne!(loaded.peers[0].id, loaded.peers[1].id);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pubkey_match_ignores_comment() {
        assert!(pubkey_matches(
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIabc old-comment",
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIabc ruthenium-peer"
        ));
        assert!(!pubkey_matches(
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIabc",
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIabd"
        ));
        assert!(!pubkey_matches("", "ssh-ed25519 AAA"));
    }
}
