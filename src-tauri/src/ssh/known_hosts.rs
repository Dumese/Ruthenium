//! 读取 / 匹配 / 追加 ~/.ssh/known_hosts（OpenSSH 格式，SHA256 指纹）。
//!
//! 匹配语义对齐 OpenSSH ssh(1) 的 known_hosts 手册：
//! - 行首标记：`@revoked`（该条目密钥已被吊销）/ `@cert-authority`（CA 证书条目，
//!   本客户端不校验主机证书，解析保留、匹配阶段跳过）；
//! - 主机模式：按 `,` 分隔，支持 `*`（任意序列）/ `?`（单字符）通配与 `!` 否定，
//!   命中任一否定模式整条不适用；非 22 端口的候选串是 `[host]:port`；
//! - hashed 主机名：`|1|<base64 salt>|<base64 hash>`，
//!   hash = HMAC-SHA1(key = 解码后的 salt, message = 候选串)；
//! - `@revoked` 命中（主机匹配 + 指纹一致）优先级最高，先于 Known / Changed 判定。
use std::fs;
use std::path::PathBuf;

use base64::engine::general_purpose::STANDARD as BASE64_STD;
use base64::Engine as _;
use hmac::{Hmac, KeyInit, Mac};
use sha1::Sha1;

use crate::error::{AppError, AppResult};

fn known_hosts_path() -> AppResult<PathBuf> {
    let home =
        dirs::home_dir().ok_or_else(|| AppError::EConfigIo("无法确定用户主目录".to_string()))?;
    Ok(home.join(".ssh").join("known_hosts"))
}

/// `[host]:port`（非 22 端口）或裸 host（22 端口）。
pub fn host_pattern(host: &str, port: u16) -> String {
    if port == 22 {
        host.to_string()
    } else {
        format!("[{host}]:{port}")
    }
}

/// OpenSSH 已知主机条目。
#[derive(Debug, Clone, PartialEq)]
pub struct KnownHostEntry {
    /// 行首标记：`Some("@revoked")` / `Some("@cert-authority")`；普通条目为 `None`。
    pub marker: Option<String>,
    pub host_pattern: String,
    pub key: String, // OpenSSH 单行公钥格式（ssh_key::PublicKey::from_openssh 可解析）
}

/// 解析已知主机文本：逐行调用 [`parse_known_hosts_line`]。
pub fn parse_known_hosts(text: &str) -> Vec<KnownHostEntry> {
    text.lines().filter_map(parse_known_hosts_line).collect()
}

/// 解析单行（无法解析的行返回 `None`，调用方据此决定"保留原样"）。
///
/// 格式: `[@marker] host-pattern keytype base64 [comment...]`
/// 仅 marker 行的首列是标记；普通行的首列就是 host 模式字段，不能丢。
fn parse_known_hosts_line(line: &str) -> Option<KnownHostEntry> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let toks: Vec<&str> = line.split_whitespace().collect();
    let (marker, fields) = match toks.split_first() {
        Some((first, rest)) if first.starts_with('@') => (Some((*first).to_string()), rest),
        _ => (None, toks.as_slice()),
    };
    // host / keytype / base64 三列是最低要求；注释列忽略。
    if fields.len() < 3 {
        return None;
    }
    Some(KnownHostEntry {
        marker,
        host_pattern: fields[0].to_string(),
        key: format!("{} {}", fields[1], fields[2]),
    })
}

/// 解析结果。`Revoked`：命中的适用条目带 `@revoked` 标记且指纹一致。
#[derive(Debug, Clone, PartialEq)]
pub enum HostKeyStatus {
    Known,
    Unknown,
    Changed,
    Revoked,
}

/// 在 known_hosts 中查找 host:port，比较 SHA256 指纹。
pub fn check(host: &str, port: u16, fingerprint: &str) -> HostKeyStatus {
    let path = match known_hosts_path() {
        Ok(p) => p,
        Err(_) => return HostKeyStatus::Unknown,
    };
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return HostKeyStatus::Unknown,
    };
    check_text(&text, host, port, fingerprint)
}

/// [`check`] 的异步版：known_hosts 可能很大 / 在网络盘上，读文件是阻塞 I/O，
/// 不能占住 tokio worker（握手回调在 async 上下文里调用）。
pub async fn check_async(host: String, port: u16, fingerprint: String) -> HostKeyStatus {
    match tokio::task::spawn_blocking(move || check(&host, port, &fingerprint)).await {
        Ok(status) => status,
        Err(e) => {
            applog!("[known_hosts] 指纹校验任务失败（按未知处理）: {e}");
            HostKeyStatus::Unknown
        }
    }
}

/// 对 known_hosts 文本执行校验（`check` 的纯函数形态，便于单测注入文本）。
///
/// 适用条目（主机模式匹配本 host、且非 `@cert-authority`）判定次序：
/// revoked → Known → Changed → Unknown。吊销优先级最高：即使被吊销的密钥
/// 同时也出现在合法条目里（行序无关），仍按 Revoked 处理。
fn check_text(text: &str, host: &str, port: u16, fingerprint: &str) -> HostKeyStatus {
    let candidate = host_pattern(host, port);
    let mut revoked = false;
    let mut known = false;
    let mut changed = false;
    for entry in parse_known_hosts(text) {
        // @cert-authority 只约束「证书形式的 host key」，本客户端不校验证书，
        // 对普通公钥不适用（既不算 Known/Changed，也不会被吊销判定波及）。
        if entry.marker.as_deref() == Some("@cert-authority") {
            continue;
        }
        if !host_applies(&entry.host_pattern, &candidate) {
            continue;
        }
        let Ok(key) = russh::keys::PublicKey::from_openssh(&entry.key) else {
            continue;
        };
        let matches = key.fingerprint(russh::keys::HashAlg::Sha256).to_string() == fingerprint;
        if entry.marker.as_deref() == Some("@revoked") {
            if matches {
                revoked = true;
            }
        } else if matches {
            known = true;
        } else {
            changed = true;
        }
    }
    if revoked {
        HostKeyStatus::Revoked
    } else if known {
        HostKeyStatus::Known
    } else if changed {
        HostKeyStatus::Changed
    } else {
        HostKeyStatus::Unknown
    }
}

/// 主机模式字段是否适用候选串（OpenSSH 语义）：`,` 分隔；命中任一 `!` 否定
/// 模式整条不适用；否则至少命中一个肯定模式才适用。匹配大小写不敏感。
fn host_applies(pattern_field: &str, candidate: &str) -> bool {
    let candidate = candidate.to_lowercase();
    let mut positive_hit = false;
    for pat in pattern_field.split(',') {
        let pat = pat.trim();
        if pat.is_empty() {
            continue;
        }
        if let Some(neg) = pat.strip_prefix('!') {
            if pattern_token_matches(neg, &candidate) {
                return false;
            }
        } else if pattern_token_matches(pat, &candidate) {
            positive_hit = true;
        }
    }
    positive_hit
}

/// 单个模式与候选串匹配：`|1|` 前缀走 hashed 解码，其余按通配符字面匹配。
fn pattern_token_matches(pattern: &str, candidate: &str) -> bool {
    if pattern.starts_with("|1|") {
        return hashed_host_matches(pattern, candidate);
    }
    wildcard_match(&pattern.to_lowercase(), candidate)
}

/// `|1|<base64 salt>|<base64 hash>`：hash = HMAC-SHA1(key = salt, message = candidate)。
/// 非 22 端口的 candidate 已是 `[host]:port` 形式（见 [`host_pattern`]）。
fn hashed_host_matches(field: &str, candidate: &str) -> bool {
    // 前两段为空串与标记 "1"。
    let mut parts = field.split('|').skip(2);
    let Some(salt) = parts
        .next()
        .and_then(|s| BASE64_STD.decode(s.as_bytes()).ok())
    else {
        return false;
    };
    let Some(hash) = parts
        .next()
        .and_then(|s| BASE64_STD.decode(s.as_bytes()).ok())
    else {
        return false;
    };
    let Ok(mut mac) = Hmac::<Sha1>::new_from_slice(&salt) else {
        return false;
    };
    mac.update(candidate.as_bytes());
    mac.verify_slice(&hash).is_ok()
}

/// 经典贪心通配匹配（`*` 回溯）：`*` 匹配任意字符序列，`?` 匹配单个字符，
/// 其余字符字面比较。空模式只匹配空串。
fn wildcard_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut star_mark = 0usize;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            star_mark = ti;
            pi += 1;
        } else if let Some(sp) = star {
            // 回溯：让最近的 `*` 多吞一个字符。
            pi = sp + 1;
            star_mark += 1;
            ti = star_mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// 把 known_hosts 文本重写为「删除本 host:port 的旧普通条目 + 追加新公钥」的结果。
///
/// 为什么必须删除旧条目而不是只 append：`check_text` 只要**任一**匹配条目命中就判
/// `Known`（只有 `@revoked` 优先级更高），只追加会让旧密钥永久保持可信——用户确认过
/// 一次密钥变更后，持有旧密钥的攻击者再连不会触发任何告警。
///
/// 保留：注释、空行、无法解析的行（不猜测用户手写内容）、`@cert-authority`、
/// 以及 `@revoked`（吊销是显式安全策略，不能被"信任"操作顺手解除）、其他主机/端口。
/// 被删除的条目不跟随 `,` 多模式拆分判断：一行只要对当前 host:port 适用就整行删除
/// （与 OpenSSH `ssh-keygen -R` 的整行语义一致）。
pub fn rewrite_trust_text(text: &str, host: &str, port: u16, openssh_public_key: &str) -> String {
    let candidate = host_pattern(host, port);
    let mut out = String::with_capacity(text.len() + 128);
    for line in text.lines() {
        let keep = match parse_known_hosts_line(line) {
            Some(entry) => match entry.marker.as_deref() {
                // 显式策略行：原样保留。
                Some("@cert-authority") | Some("@revoked") => true,
                _ => !host_applies(&entry.host_pattern, &candidate),
            },
            None => true, // 注释 / 空行 / 无法解析：保留原样，绝不误删
        };
        if keep {
            out.push_str(line.trim_end());
            out.push('\n');
        }
    }
    out.push_str(&format!("{} {}\n", candidate, openssh_public_key));
    out
}

/// 信任写入的进程内串行化锁：读-改写-整段 rename 必须原子地串行执行，
/// 否则两个指纹弹窗同时确认（或与外部 ssh 进程写盘交错）会互相覆盖丢条目。
static TRUST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 信任某个主机公钥：原子重写 known_hosts——先删该 host:port 的旧普通条目，再追加新行。
/// 文件不存在则创建（`~/.ssh` 按需创建；NTFS 继承 ACL，不设权限位）。
///
/// 本函数做同步文件 I/O，调用方在 async 上下文应使用 [`replace_host_entry_async`]。
pub fn replace_host_entry(host: &str, port: u16, openssh_public_key: &str) -> AppResult<()> {
    use std::io::Write;

    // 拿不到锁（中毒）也要继续：丢一次串行化好过让“信任”永远失败。
    let _guard = TRUST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let path = known_hosts_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| AppError::EConfigIo(format!("创建 ~/.ssh 失败: {e}")))?;
    }
    let existing = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(AppError::EConfigIo(format!("读取 known_hosts 失败: {e}"))),
    };
    let next = rewrite_trust_text(&existing, host, port, openssh_public_key);
    // tmp → rename 原子替换：避免写入中途崩溃留下半截 known_hosts（把用户全部
    // 已知主机记录一起毁掉）。Windows 上 std::fs::rename 走 MoveFileEx 覆盖语义。
    // tmp 名带 uuid：并发的两个信任操作（两个主机的指纹弹窗同时确认）不会互相踩。
    let tmp = path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4()));
    let result = (|| -> AppResult<()> {
        let mut f = fs::File::create(&tmp)
            .map_err(|e| AppError::EConfigIo(format!("写入 {} 失败: {e}", tmp.display())))?;
        f.write_all(next.as_bytes())
            .map_err(|e| AppError::EConfigIo(format!("写入 {} 失败: {e}", tmp.display())))?;
        f.sync_all()
            .map_err(|e| AppError::EConfigIo(format!("fsync known_hosts 失败: {e}")))?;
        drop(f);
        fs::rename(&tmp, &path)
            .map_err(|e| AppError::EConfigIo(format!("重写 {} 失败: {e}", path.display())))?;
        Ok(())
    })();
    if result.is_err() {
        // rename 失败（目标被 AV/编辑器占用等）：不留 tmp-* 残留
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// [`replace_host_entry`] 的异步版：整段（读 + 写 + fsync + rename）
/// 放到阻塞线程池执行，不占 tokio worker。
pub async fn replace_host_entry_async(
    host: String,
    port: u16,
    openssh_public_key: String,
) -> AppResult<()> {
    tokio::task::spawn_blocking(move || replace_host_entry(&host, port, &openssh_public_key))
        .await
        .map_err(|e| AppError::EConfigIo(format!("信任写入任务执行失败: {e}")))?
}

#[cfg(test)]
mod tests {
    use super::*;

    /// russh known_hosts 自测用过的有效公钥向量。
    const KEY_A: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILIG2T/B0l0gaqj3puu510tu9N1OkQ4znY3LYuEm5zCF";
    const KEY_B: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJdD7y3aLq454yWBdwLWbieU1ebz9/cu7/QEXn9OIeZJ";
    /// 对应 KEY_A、主机 example.com 的 hashed 条目（russh 自测向量）。
    const HASHED_EXAMPLE_COM: &str = "|1|O33ESRMWPVkMYIwJ1Uw+n877jTo=|nuuC5vEqXlEZ/8BXQR7m619W6Ak=";

    fn fp(key_line: &str) -> String {
        russh::keys::PublicKey::from_openssh(key_line)
            .expect("测试密钥必须可解析")
            .fingerprint(russh::keys::HashAlg::Sha256)
            .to_string()
    }

    #[test]
    fn host_pattern_format() {
        assert_eq!(host_pattern("web-01", 22), "web-01");
        assert_eq!(host_pattern("web-01", 2222), "[web-01]:2222");
    }

    /// @revoked / @cert-authority 行要解析出来（带标记）；注释列含 @（如邮箱）不再丢行。
    #[test]
    fn parse_keeps_markers_and_comment_columns() {
        let text = "# comment\nlocalhost ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI user@box\n@revoked evil ssh-rsa AAAA\n@cert-authority *.zone ssh-ed25519 AAAAB3\n";
        let entries = parse_known_hosts(text);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].marker, None);
        assert_eq!(entries[0].host_pattern, "localhost");
        // key 只取 keytype + base64 两列，邮箱注释列不混入。
        assert_eq!(entries[0].key, "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI");
        assert_eq!(entries[1].marker.as_deref(), Some("@revoked"));
        assert_eq!(entries[1].host_pattern, "evil");
        assert_eq!(entries[2].marker.as_deref(), Some("@cert-authority"));
        assert_eq!(entries[2].host_pattern, "*.zone");
    }

    /// @revoked 命中返回 Revoked，且优先级高于合法条目（行序无关）。
    #[test]
    fn revoked_hit_wins_over_known() {
        let fp_a = fp(KEY_A);
        let after_known = format!("host1 {KEY_A}\n@revoked host1 {KEY_A}\n");
        assert_eq!(
            check_text(&after_known, "host1", 22, &fp_a),
            HostKeyStatus::Revoked
        );
        let before_known = format!("@revoked host1 {KEY_A}\nhost1 {KEY_A}\n");
        assert_eq!(
            check_text(&before_known, "host1", 22, &fp_a),
            HostKeyStatus::Revoked
        );
    }

    /// 被吊销的是另一把密钥时不影响正常条目：当前密钥 → Known，被吊销密钥 → Revoked。
    #[test]
    fn revoked_other_key_keeps_normal_match() {
        let fp_a = fp(KEY_A);
        let fp_b = fp(KEY_B);
        let text = format!("host1 {KEY_A}\n@revoked host1 {KEY_B}\n");
        assert_eq!(check_text(&text, "host1", 22, &fp_a), HostKeyStatus::Known);
        assert_eq!(
            check_text(&text, "host1", 22, &fp_b),
            HostKeyStatus::Revoked
        );
    }

    /// hashed 条目：匹配 example.com；不匹配其它主机与非 22 端口变体。
    #[test]
    fn hashed_entry_matches_and_mismatches() {
        let fp_a = fp(KEY_A);
        let text = format!("{HASHED_EXAMPLE_COM} {KEY_A}\n");
        assert_eq!(
            check_text(&text, "example.com", 22, &fp_a),
            HostKeyStatus::Known
        );
        assert_eq!(
            check_text(&text, "other.com", 22, &fp_a),
            HostKeyStatus::Unknown
        );
        // 非 22 端口时被哈希的候选串是 [host]:port，与裸 host 条目不匹配。
        assert_eq!(
            check_text(&text, "example.com", 2222, &fp_a),
            HostKeyStatus::Unknown
        );
    }

    /// 多模式 + 否定模式：`*.example.com,!evil.example.com`（TOFU 语义）。
    #[test]
    fn multi_pattern_with_negation() {
        let fp_a = fp(KEY_A);
        let text = format!("*.example.com,!evil.example.com {KEY_A}\n");
        assert_eq!(
            check_text(&text, "web.example.com", 22, &fp_a),
            HostKeyStatus::Known
        );
        assert_eq!(
            check_text(&text, "evil.example.com", 22, &fp_a),
            HostKeyStatus::Unknown
        );
        assert_eq!(
            check_text(&text, "example.org", 22, &fp_a),
            HostKeyStatus::Unknown
        );
    }

    /// `[host]:port` 变体按候选串 `[host]:port` 精确匹配。
    #[test]
    fn bracket_port_variant_matches() {
        let fp_a = fp(KEY_A);
        let text = format!("[web-01]:2222 {KEY_A}\n");
        assert_eq!(
            check_text(&text, "web-01", 2222, &fp_a),
            HostKeyStatus::Known
        );
        assert_eq!(
            check_text(&text, "web-01", 22, &fp_a),
            HostKeyStatus::Unknown
        );
    }

    /// 主机已收录但密钥不同 → Changed；主机从未收录 → Unknown（保持旧语义）。
    #[test]
    fn changed_and_unknown_semantics_preserved() {
        let fp_b = fp(KEY_B);
        let text = format!("host1 {KEY_A}\n");
        assert_eq!(
            check_text(&text, "host1", 22, &fp_b),
            HostKeyStatus::Changed
        );
        assert_eq!(
            check_text(&text, "host2", 22, &fp_b),
            HostKeyStatus::Unknown
        );
    }

    /// 通配符基础语义（`*` 任意序列、`?` 单字符、无回溯误判）。
    #[test]
    fn wildcard_match_semantics() {
        assert!(wildcard_match("*.example.com", "web.example.com"));
        assert!(!wildcard_match("*.example.com", "web.example.org"));
        assert!(wildcard_match("web?", "web1"));
        assert!(!wildcard_match("web?", "web12"));
        assert!(wildcard_match("*", "anything"));
        assert!(wildcard_match("a*b*c", "a-x-b-y-c"));
        assert!(!wildcard_match("a*b*c", "a-x-b-y"));
    }

    // ---------------- 信任变更后的密钥必须替换旧条目 ----------------

    /// 用户确认密钥变更后，旧公钥必须不再可信（否则旧密钥永久 Known）。
    #[test]
    fn trust_replaces_old_key_for_same_host() {
        let fp_a = fp(KEY_A);
        let fp_b = fp(KEY_B);
        let text = format!("host1 {KEY_A}\nhost2 {KEY_B}\n");
        let next = rewrite_trust_text(&text, "host1", 22, KEY_B);

        // 旧密钥：不再被信任，且相对新记录是"变更"→ 阻断并告警（不能静默判 Known）。
        // 注意语义：删掉旧条目后，旧公钥对该主机表现为 Changed（记录里只有新键），
        // 这正是 TOFU 期望的结果——连接被拦下、由用户重新决策，而不是无声放行。
        assert_eq!(
            check_text(&next, "host1", 22, &fp_a),
            HostKeyStatus::Changed
        );
        // 新密钥：Known
        assert_eq!(check_text(&next, "host1", 22, &fp_b), HostKeyStatus::Known);
        // 其它主机条目不动
        assert_eq!(check_text(&next, "host2", 22, &fp_b), HostKeyStatus::Known);
        // 旧行确实被删除，文件不会随每次确认无限增长
        assert_eq!(next.matches("host1 ").count(), 1, "{next}");
        assert!(!next.contains(KEY_A), "旧公钥必须从文件中删除: {next}");
    }

    /// 只影响「当前 host:port」：其它端口、注释、@revoked / @cert-authority 原样保留。
    #[test]
    fn trust_rewrite_preserves_policy_and_other_lines() {
        let text = format!(
            "# my hosts\n\n@revoked host1 {KEY_A}\n@cert-authority *.corp {KEY_A}\n[host1]:2222 {KEY_A}\nhost1 {KEY_A}\n*.example.com,!evil.example.com {KEY_A}\n"
        );
        let next = rewrite_trust_text(&text, "host1", 22, KEY_B);

        assert!(next.contains("# my hosts"), "{next}");
        assert!(
            next.contains("@revoked host1"),
            "吊销策略不得被信任操作解除: {next}"
        );
        assert!(next.contains("@cert-authority"), "{next}");
        assert!(
            next.contains("[host1]:2222"),
            "其它端口条目不得被删除: {next}"
        );
        assert!(
            next.contains("*.example.com"),
            "其它主机条目不得被删除: {next}"
        );
        assert_eq!(next.matches(&format!("host1 {KEY_B}")).count(), 1, "{next}");
        assert_eq!(
            check_text(&next, "host1", 22, &fp(KEY_B)),
            HostKeyStatus::Known
        );
        assert_eq!(
            check_text(&next, "host1", 2222, &fp(KEY_A)),
            HostKeyStatus::Known,
            "2222 端口的旧条目仍应保留"
        );
    }

    /// hashed 条目（|1|salt|hash）同样按 host:port 删除。
    #[test]
    fn trust_rewrite_drops_hashed_entry_for_same_host() {
        let fp_a = fp(KEY_A);
        let text = format!("{HASHED_EXAMPLE_COM} {KEY_A}\n");
        let next = rewrite_trust_text(&text, "example.com", 22, KEY_B);
        assert!(!next.contains("|1|"), "hashed 旧条目必须删除: {next}");
        // 旧密钥不再静默可信：表现为 Changed（阻断 + 变更告警），而非 Known。
        assert_eq!(
            check_text(&next, "example.com", 22, &fp_a),
            HostKeyStatus::Changed
        );
        assert_eq!(
            check_text(&next, "example.com", 22, &fp(KEY_B)),
            HostKeyStatus::Known
        );
        // 新行按未哈希的 host 写回（与 OpenSSH 默认 HashKnownHosts=no 一致）
        assert!(next.starts_with("example.com "), "{next}");
    }

    /// `,` 多模式行只要对当前 host 适用就整行删除（与 ssh-keygen -R 的整行语义一致）。
    #[test]
    fn trust_rewrite_removes_multi_pattern_line_that_applies() {
        let text = format!("*.example.com,!evil.example.com {KEY_A}\n");
        let next = rewrite_trust_text(&text, "web.example.com", 22, KEY_B);
        assert!(!next.contains(KEY_A), "{next}");
        assert_eq!(next.matches("web.example.com ").count(), 1, "{next}");
        // 同一个模式行对 evil 子域不适用，仍保留
        let kept = rewrite_trust_text(&text, "evil.example.com", 22, KEY_B);
        assert!(
            kept.contains(&format!("*.example.com,!evil.example.com {KEY_A}")),
            "{kept}"
        );
        assert!(
            kept.ends_with(&format!("evil.example.com {KEY_B}\n")),
            "{kept}"
        );
    }

    /// 文件不存在（空文本）时等价于新建：只有一行新条目。
    #[test]
    fn trust_rewrite_from_empty_text() {
        assert_eq!(
            rewrite_trust_text("", "web-01", 22, KEY_A),
            format!("web-01 {KEY_A}\n")
        );
        // 非 22 端口写出 [host]:port 形式
        assert_eq!(
            rewrite_trust_text("", "web-01", 2222, KEY_A),
            format!("[web-01]:2222 {KEY_A}\n")
        );
    }
}
