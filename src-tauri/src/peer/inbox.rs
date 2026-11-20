//! 配置收件箱。
//!
//! 对端通过 SFTP 写入 **虚拟路径 `/peers/inbox/<时间戳>__<设备名>.json`**，
//! 实际落在本机 `%APPDATA%\Ruthenium\peers\inbox\`（与 `peers.toml` 同目录族，
//! 但不在共享根里——即使关闭「文件互传」，只要授予了「推送配置」也能收）。
//!
//! 收件箱只做三件事：
//! 1. 列条目 + 摘要（多少台主机/片段/隧道），**不**自动导入；
//! 2. 导入：复用 `apply_backup_text`（校验 + 原子写 + 写前备份 + 清理失效凭据）；
//! 3. 忽略：删除该条目。
//!
//! 安全：文件名只接受单层 `[A-Za-z0-9._-]`（拒绝路径分隔符 / `..`），
//! 导入大小沿用备份上限 [`crate::commands::config::MAX_BACKUP_BYTES`]。

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::{AppError, AppResult};

/// 收件箱在「对端 SFTP」里的虚拟目录（服务端映射到本机数据目录）。
pub const INBOX_VIRTUAL_DIR: &str = "peers/inbox";
/// 时间戳与设备名之间的分隔符（设备名里可能有 `-`）。
const NAME_SEP: &str = "__";
/// 收件箱列表上限：避免恶意对端推送大量条目把设备页拖死。
const MAX_INBOX_ENTRIES: usize = 200;

/// 收件箱目录（不存在时按需创建）。
pub fn inbox_dir() -> AppResult<PathBuf> {
    let dir = crate::config::config_dir()?.join("peers").join("inbox");
    if !dir.exists() {
        std::fs::create_dir_all(&dir)
            .map_err(|e| AppError::EConfigIo(format!("创建收件箱目录失败: {e}")))?;
    }
    Ok(dir)
}

/// 收件箱条目（前端展示用；`hosts/snippets/tunnels` 为摘要计数）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InboxItem {
    /// 文件名（列表 / 导入 / 删除都以它为准，已校验不含路径分隔符）
    pub id: String,
    /// 发送方设备名（取自文件名；不可信，仅展示）
    pub from: String,
    /// 接收时间（本地时间，取自文件名的时间戳）
    pub received_at: String,
    pub bytes: u64,
    pub hosts: usize,
    pub snippets: usize,
    pub tunnels: usize,
    pub folders: usize,
    /// 备份里的导出来源时间（如果有）
    pub exported_at: Option<String>,
    /// 版本/JSON 是否可导入；不可导入时 `error` 给出原因
    pub valid: bool,
    pub error: Option<String>,
}

/// 收件箱事件 payload（`peer://inbox`）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InboxNotice {
    /// 新条目（已解析摘要）
    pub item: InboxItem,
}

/// 当前收件箱列表（按接收时间倒序）。
pub fn list() -> AppResult<Vec<InboxItem>> {
    let dir = inbox_dir()?;
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(AppError::EConfigIo(format!("读取收件箱失败: {e}"))),
    };
    for ent in entries.flatten() {
        if out.len() >= MAX_INBOX_ENTRIES {
            break;
        }
        let name = ent.file_name().to_string_lossy().to_string();
        if !name.to_ascii_lowercase().ends_with(".json") || !is_safe_name(&name) {
            continue;
        }
        if let Ok(item) = item_of(&dir, &name) {
            out.push(item);
        }
    }
    out.sort_by(|a, b| b.received_at.cmp(&a.received_at).then(b.id.cmp(&a.id)));
    Ok(out)
}

/// 读取单个条目的摘要（服务端写入完成时推送 `peer://inbox` 用）。
pub fn item(id: &str) -> AppResult<InboxItem> {
    if !is_safe_name(id) {
        return Err(AppError::EInvalidInput("收件箱条目名非法".to_string()));
    }
    let dir = inbox_dir()?;
    item_of(&dir, id)
}

/// 读取某条目的原始文本（导入用；调用方负责大小与二次确认）。
pub fn read_text(id: &str) -> AppResult<String> {
    if !is_safe_name(id) {
        return Err(AppError::EInvalidInput("收件箱条目名非法".to_string()));
    }
    let path = inbox_dir()?.join(id);
    let meta = std::fs::metadata(&path)
        .map_err(|e| AppError::EConfigIo(format!("读取配置条目失败: {e}")))?;
    if !meta.is_file() {
        return Err(AppError::EInvalidInput("收件箱条目不是文件".to_string()));
    }
    if meta.len() > crate::commands::config::MAX_BACKUP_BYTES {
        return Err(AppError::EInvalidInput(format!(
            "配置条目过大（{} 字节，上限 {} MiB）",
            meta.len(),
            crate::commands::config::MAX_BACKUP_BYTES / (1024 * 1024)
        )));
    }
    std::fs::read_to_string(&path)
        .map_err(|e| AppError::EConfigIo(format!("读取配置条目失败: {e}")))
}

/// 删除条目（导入成功后或用户点「忽略」）。
pub fn delete(id: &str) -> AppResult<bool> {
    if !is_safe_name(id) {
        return Err(AppError::EInvalidInput("收件箱条目名非法".to_string()));
    }
    let path = inbox_dir()?.join(id);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(AppError::EConfigIo(format!("删除配置条目失败: {e}"))),
    }
}

/// 生成发送侧文件名：`<yyyyMMdd-HHmmss>__<清理后的设备名>.json`。
pub fn outbound_name(device_name: &str) -> String {
    let t = crate::session_log::LogStamp::now();
    let safe: String = device_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let safe = if safe.is_empty() {
        "device".to_string()
    } else {
        safe
    };
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}{NAME_SEP}{safe}.json",
        t.year, t.month, t.day, t.hour, t.minute, t.second
    )
}

/// 文件名是否安全（单层、无路径分隔符、无 `..`）。
fn is_safe_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\'])
        && !name.contains("..")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ' '))
}

fn item_of(dir: &Path, name: &str) -> AppResult<InboxItem> {
    let path = dir.join(name);
    let md = std::fs::metadata(&path)
        .map_err(|e| AppError::EConfigIo(format!("读取配置条目失败: {e}")))?;
    let (received_at, from) = split_name(name);
    // 列表页不能无条件把每个条目都读进内存；超过备份上限的条目直接标为无效。
    let summary = if md.len() > crate::commands::config::MAX_BACKUP_BYTES {
        Summary {
            error: Some(format!(
                "配置条目过大（{} 字节，上限 {} MiB）",
                md.len(),
                crate::commands::config::MAX_BACKUP_BYTES / (1024 * 1024)
            )),
            ..Default::default()
        }
    } else {
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        summarize(&text)
    };
    Ok(InboxItem {
        id: name.to_string(),
        from,
        received_at,
        bytes: md.len(),
        hosts: summary.hosts,
        snippets: summary.snippets,
        tunnels: summary.tunnels,
        folders: summary.folders,
        exported_at: summary.exported_at,
        valid: summary.valid,
        error: summary.error,
    })
}

/// 从文件名还原「接收时间 + 发送方设备名」（格式不符时原样放进 from）。
fn split_name(name: &str) -> (String, String) {
    let stem = name.trim_end_matches(".json");
    match stem.split_once(NAME_SEP) {
        Some((stamp, dev)) => {
            let ts = if stamp.len() == 15 {
                format!(
                    "{}-{}-{} {}:{}:{}",
                    &stamp[0..4],
                    &stamp[4..6],
                    &stamp[6..8],
                    &stamp[9..11],
                    &stamp[11..13],
                    &stamp[13..15]
                )
            } else {
                stamp.to_string()
            };
            (ts, dev.to_string())
        }
        None => (String::new(), stem.to_string()),
    }
}

#[derive(Default)]
struct Summary {
    hosts: usize,
    snippets: usize,
    tunnels: usize,
    folders: usize,
    exported_at: Option<String>,
    valid: bool,
    error: Option<String>,
}

/// 只做摘要 + 语义校验（不落盘、不碰凭据）。
fn summarize(text: &str) -> Summary {
    let mut s = Summary::default();
    let parsed: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => {
            s.error = Some(format!("JSON 解析失败: {e}"));
            return s;
        }
    };
    let version = parsed.get("version").and_then(|v| v.as_u64());
    if version != Some(crate::config::CONFIG_VERSION as u64) {
        s.error = Some(match version {
            Some(v) => format!("不支持的备份版本: {v}"),
            None => "备份缺少 version 字段".to_string(),
        });
        return s;
    }
    s.hosts = count_of(&parsed, "hosts");
    s.snippets = count_of(&parsed, "snippets");
    s.tunnels = count_of(&parsed, "tunnels");
    s.folders = count_of(&parsed, "folders");
    s.exported_at = parsed
        .get("exportedAt")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    s.valid = true;
    s
}

fn count_of(v: &serde_json::Value, key: &str) -> usize {
    v.get(key)
        .and_then(|x| x.as_array())
        .map(|a| a.len())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outbound_name_is_safe_and_parseable() {
        let n = outbound_name(r#"DESKTOP-1\legit:*/name"#);
        assert!(n.ends_with(".json"));
        assert!(is_safe_name(&n), "生成的文件名必须能通过安全校验: {n}");
        let (_, from) = split_name(&n);
        assert!(!from.contains(['/', '\\', ':']), "设备名必须已清理: {from}");
    }

    #[test]
    fn unsafe_names_are_rejected() {
        assert!(!is_safe_name("../peers.toml"));
        assert!(!is_safe_name(r"..\hosts.toml"));
        assert!(!is_safe_name("a/b.json"));
        assert!(!is_safe_name("…"));
        assert!(is_safe_name("20261117-101500__LAPTOP-F7Q.json"));
    }

    #[test]
    fn summarize_counts_and_validates_version() {
        let ok = r#"{"version":1,"exportedAt":"unix:1","hosts":[{},{}],"snippets":[{}],"tunnels":[],"folders":[]}"#;
        let s = summarize(ok);
        assert!(s.valid, "{:?}", s.error);
        assert_eq!((s.hosts, s.snippets), (2, 1));
        let bad = r#"{"version":99,"hosts":[]}"#;
        let s = summarize(bad);
        assert!(!s.valid);
        assert!(s.error.unwrap_or_default().contains("版本"));
        assert!(!summarize("not json").valid);
    }

    #[test]
    fn split_name_reads_timestamp() {
        let (ts, from) = split_name("20261117-231500__LAPTOP-X.json");
        assert_eq!(ts, "2026-11-17 23:15:00");
        assert_eq!(from, "LAPTOP-X");
    }
}
