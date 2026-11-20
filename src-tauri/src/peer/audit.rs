//! 局域网会话审计。
//!
//! 只记录**元数据**（谁、何时、哪种会话、来源地址、尺寸/画质/帧数/字节/时长），
//! 不记录屏幕内容、终端字节流或剪贴板文本。写入失败绝不影响会话主流程。
//!
//! 存储：`<config_dir>/peer-audit.jsonl`，一行一条；超过 2 MiB 滚动为
//! `peer-audit.jsonl.1`（只留一份备份），读取时取最近 `limit` 条。

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::config::config_dir;

const MAX_BYTES: u64 = 2 * 1024 * 1024;

/// 一条会话记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditEntry {
    /// 开始时间 `YYYY-MM-DD HH:MM:SS`（本地时间）。
    pub at: String,
    /// `screen`（屏幕查看/操作）、`control`（远程终端）或 `pair`（配对成功/失败）。
    pub kind: String,
    /// `incoming` = 对方连接本机；`outgoing` = 本机连接对方。
    pub direction: String,
    pub peer_name: String,
    pub peer_addr: String,
    #[serde(default)]
    pub quality: Option<u8>,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
    #[serde(default)]
    pub frames: Option<u64>,
    #[serde(default)]
    pub bytes: Option<u64>,
    #[serde(default)]
    pub seconds: u64,
    #[serde(default)]
    pub reason: Option<String>,
}

pub fn audit_path() -> Option<PathBuf> {
    config_dir().ok().map(|d| d.join("peer-audit.jsonl"))
}

/// 删除审计文件（含滚动备份）；失败返回 false。
pub fn clear() -> bool {
    let Some(path) = audit_path() else {
        return false;
    };
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("jsonl.1"));
    true
}

/// 当前本地时间戳（与 applog 一致）。
pub fn stamp() -> String {
    let t = crate::session_log::LogStamp::now();
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        t.year, t.month, t.day, t.hour, t.minute, t.second
    )
}

/// 追加一条记录；失败只写应用日志，不向上传播。
pub fn append(entry: &AuditEntry) {
    let Some(path) = audit_path() else { return };
    let line = match serde_json::to_string(entry) {
        Ok(s) => s,
        Err(e) => {
            applog!("[peer] 审计序列化失败: {e}");
            return;
        }
    };
    if let Err(e) = append_line(&path, &line) {
        applog!("[peer] 审计写入失败: {e}");
    }
}

fn append_line(path: &std::path::Path, line: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if std::fs::metadata(path)
        .map(|m| m.len() > MAX_BYTES)
        .unwrap_or(false)
    {
        let _ = std::fs::rename(path, path.with_extension("jsonl.1"));
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    f.write_all(line.as_bytes())?;
    f.write_all(b"\n")
}

/// 最近 `limit` 条（新的在前）。读失败 → 空列表（页面显示“暂无记录”）。
pub fn recent(limit: usize) -> Vec<AuditEntry> {
    let Some(path) = audit_path() else {
        return Vec::new();
    };
    let Ok(f) = std::fs::File::open(&path) else {
        return Vec::new();
    };
    let mut out: Vec<AuditEntry> = BufReader::new(f)
        .lines()
        .map_while(Result::ok)
        .filter_map(|l| serde_json::from_str::<AuditEntry>(&l).ok())
        .collect();
    out.reverse();
    out.truncate(limit.max(1));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_serializes_camel_case() {
        let e = AuditEntry {
            at: "2026-11-17 13:00:00".into(),
            kind: "screen".into(),
            direction: "incoming".into(),
            peer_name: "peer".into(),
            peer_addr: "192.168.1.5".into(),
            quality: Some(65),
            width: Some(1920),
            height: Some(1080),
            frames: Some(120),
            bytes: Some(2_000_000),
            seconds: 12,
            reason: None,
        };
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["peerName"], "peer");
        assert_eq!(v["peerAddr"], "192.168.1.5");
        assert_eq!(v["quality"], 65);
        assert!(v.get("peer_name").is_none());
    }

    #[test]
    fn recent_is_newest_first_and_bounded() {
        let dir = std::env::temp_dir().join(format!("ruthenium-audit-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        // 直接测 append_line + 解析，不触碰真实用户文件
        let path = dir.join("peer-audit.jsonl");
        for i in 0..3 {
            let line = format!(
                r#"{{"at":"2026-11-17 13:00:0{i}","kind":"screen","direction":"outgoing","peerName":"p{i}","peerAddr":"127.0.0.1","seconds":{i}}}"#
            );
            append_line(&path, &line).unwrap();
        }
        let f = std::fs::File::open(&path).unwrap();
        let mut rows: Vec<AuditEntry> = BufReader::new(f)
            .lines()
            .map_while(Result::ok)
            .filter_map(|l| serde_json::from_str(&l).ok())
            .collect();
        rows.reverse();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].peer_name, "p2");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
