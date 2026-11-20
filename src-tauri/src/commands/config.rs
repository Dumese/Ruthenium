//! 配置命令：load/save/import/export/backup。
use std::collections::HashSet;

use serde::Serialize;

use crate::config;
use crate::error::{AppError, AppResult};
use crate::models::{
    AuthMethod, Config, Folder, Host, ParsedHost, Preferences, Snippet, TunnelRule,
};
use crate::state::AppState;

/// 返回内存中的当前配置快照。
#[tauri::command]
pub async fn load_config(state: tauri::State<'_, AppState>) -> AppResult<Config> {
    Ok(state.config.read().await.clone())
}

/// 保存全局偏好并原子写回 hosts.toml；落盘失败时回滚内存。
#[tauri::command]
pub async fn save_preferences(
    state: tauri::State<'_, AppState>,
    preferences: Preferences,
) -> AppResult<()> {
    let old_name = state
        .config
        .read()
        .await
        .preferences
        .lan_device_name
        .clone();
    state
        .mutate_config(|c| {
            // IPC 入口同样规范化数值范围（与磁盘/备份路径一致）。
            let mut prefs = preferences;
            prefs.clamp_to_ranges();
            c.preferences = prefs;
        })
        .await?;
    // 设备名同时供发现/配对 Hello 使用：不必重启监听即可生效。
    let new_name = state
        .config
        .read()
        .await
        .preferences
        .lan_device_name
        .clone();
    if old_name != new_name {
        state.peer.set_device_name(&new_name);
    }
    // 严格配对开关同样即时生效（入站握手读取共享原子值）。
    state
        .peer
        .set_strict_pairing(state.config.read().await.preferences.lan_strict_pairing);
    Ok(())
}

/// 解析 ~/.ssh/config 文本 → 预览条目（仅预览，不落盘）。
/// 忽略 `Host *` 通配与 `Match` 块；`Include` 无法在纯文本场景解析，忽略。
pub fn parse_ssh_config(text: &str) -> Vec<ParsedHost> {
    let mut out: Vec<ParsedHost> = Vec::new();
    let mut in_match_block = false;
    let mut current: Option<(Vec<String>, ParsedHost)> = None;

    let flush = |out: &mut Vec<ParsedHost>, cur: Option<(Vec<String>, ParsedHost)>| {
        if let Some((patterns, p)) = cur {
            if patterns.is_empty()
                || patterns
                    .iter()
                    .any(|pat| pat.contains('*') || pat.contains('?'))
            {
                return;
            }
            let name = patterns[0].clone();
            let mut p = p;
            p.name = name.clone();
            if p.addr.is_empty() {
                p.addr = name;
            }
            if out.iter().any(|e| e.addr == p.addr && e.port == p.port) {
                p.duplicate = true;
            }
            out.push(p);
        }
    };

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (keyword, value) = match line.split_once(char::is_whitespace) {
            Some((k, v)) => (k, v.trim()),
            None => (line, ""),
        };
        let key = keyword.to_ascii_lowercase();
        match key.as_str() {
            "match" => {
                flush(&mut out, current.take());
                in_match_block = true;
                current = None;
            }
            "host" => {
                flush(&mut out, current.take());
                in_match_block = false;
                let patterns: Vec<String> = value.split_whitespace().map(String::from).collect();
                current = Some((
                    patterns,
                    ParsedHost {
                        name: String::new(),
                        addr: String::new(),
                        user: String::new(),
                        port: 22,
                        key_ref: String::new(),
                        group: String::new(),
                        tag: String::new(),
                        note: String::new(),
                        duplicate: false,
                    },
                ));
            }
            _ if in_match_block => {}
            "hostname" => {
                if let Some((_, p)) = current.as_mut() {
                    p.addr = value.to_string();
                }
            }
            "user" => {
                if let Some((_, p)) = current.as_mut() {
                    p.user = value.to_string();
                }
            }
            "port" => {
                if let Some((_, p)) = current.as_mut() {
                    if let Ok(port) = value.parse::<u16>() {
                        p.port = port;
                    }
                }
            }
            "identityfile" => {
                if let Some((_, p)) = current.as_mut() {
                    p.key_ref = value.to_string();
                }
            }
            _ => {}
        }
    }
    flush(&mut out, current);
    out
}

/// 读取本机 `~/.ssh/config`（含 `Include` 展开）并解析为预览条目。
///
/// 一键导入：直接从 `~/.ssh/config` 解析出预览条目，前端无需用户手贴文本。
#[tauri::command]
pub async fn import_ssh_config_default() -> AppResult<Vec<ParsedHost>> {
    let path = default_ssh_config_path()?;
    let text = read_ssh_config_with_includes(&path, 0)?;
    if text.trim().is_empty() {
        return Err(AppError::ENotFound(format!(
            "{} 为空或没有可用条目",
            path.display()
        )));
    }
    Ok(parse_ssh_config(&text))
}

/// `<home>\.ssh\config`。
fn default_ssh_config_path() -> AppResult<std::path::PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| AppError::EFs("无法确定用户主目录".to_string()))?;
    Ok(home.join(".ssh").join("config"))
}

/// 读取 config 文本，并把 `Include` 指向的文件按 OpenSSH 规则拼接进来。
///
/// - 支持相对路径（相对 `~/.ssh`）与 `~` 前缀；
/// - 支持 `*` / `?` 通配（只匹配一层目录内的文件）；
/// - 深度上限 8，避免循环 Include；
/// - 单个 Include 失败只跳过（诊断信息写 stderr），不影响主文件。
fn read_ssh_config_with_includes(path: &std::path::Path, depth: usize) -> AppResult<String> {
    if depth > 8 {
        return Ok(String::new());
    }
    let text = std::fs::read_to_string(path)
        .map_err(|e| AppError::EFs(format!("读取 {} 失败: {e}", path.display())))?;
    let base = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        let trimmed = line.trim();
        let is_include = trimmed
            .split_whitespace()
            .next()
            .map(|k| k.eq_ignore_ascii_case("include"))
            .unwrap_or(false);
        if !is_include {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        let spec = trimmed["Include".len()..].trim();
        for file in expand_include_spec(base, spec) {
            match read_ssh_config_with_includes(&file, depth + 1) {
                Ok(sub) => {
                    out.push_str(&sub);
                    out.push('\n');
                }
                Err(e) => applog!("[ssh-config] 跳过 Include {}: {e}", file.display()),
            }
        }
    }
    Ok(out)
}

/// 把 `Include` 的值展开成实际文件列表（支持通配与 `~`）。
fn expand_include_spec(base: &std::path::Path, spec: &str) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for token in spec.split_whitespace() {
        let expanded = if let Some(rest) = token
            .strip_prefix("~/")
            .or_else(|| token.strip_prefix("~\\"))
        {
            match dirs::home_dir() {
                Some(home) => home.join(rest),
                None => continue,
            }
        } else {
            let p = std::path::Path::new(token);
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                base.join(token)
            }
        };
        let text = expanded.to_string_lossy().to_string();
        if text.contains('*') || text.contains('?') {
            let parent = expanded.parent().unwrap_or(base);
            let pattern_name = expanded
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            if let Ok(entries) = std::fs::read_dir(parent) {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if glob_match(&pattern_name, &name) {
                        out.push(entry.path());
                    }
                }
            }
        } else if expanded.is_file() {
            out.push(expanded);
        }
    }
    out
}

/// 极简 glob：只支持 `*` 与 `?`（够用于 `Include config.d/*`）。
fn glob_match(pattern: &str, name: &str) -> bool {
    fn go(p: &[u8], n: &[u8]) -> bool {
        match p.first() {
            None => n.is_empty(),
            Some(b'*') => go(&p[1..], n) || (!n.is_empty() && go(p, &n[1..])),
            Some(b'?') => !n.is_empty() && go(&p[1..], &n[1..]),
            Some(c) => n.first() == Some(c) && go(&p[1..], &n[1..]),
        }
    }
    go(pattern.as_bytes(), name.as_bytes())
}

/// 解析用户粘贴的 `~/.ssh/config` 文本，只返回预览条目，不落盘。
#[tauri::command]
pub async fn import_ssh_config(text: String) -> AppResult<Vec<ParsedHost>> {
    if text.trim().is_empty() {
        return Err(AppError::EInvalidInput("导入内容为空".to_string()));
    }
    Ok(parse_ssh_config(&text))
}

/// 把导入的主机并入配置（`commit_import`）：
/// 重复地址跳过；回填 id / 默认端口；并清掉 auth != key 时的 keyRef。
fn commit_hosts_into(config: &mut Config, hosts: Vec<Host>) -> usize {
    let mut added = 0usize;
    for mut host in hosts {
        // 重复地址跳过（同 addr+port）
        if config
            .hosts
            .iter()
            .any(|h| h.addr == host.addr && h.port == host.port)
        {
            continue;
        }
        if host.id.is_empty() {
            host.id = uuid::Uuid::new_v4().to_string();
        }
        if host.port == 0 {
            host.port = 22;
        }
        host.normalize_auth();
        config.hosts.push(host);
        added += 1;
    }
    added
}

/// 把预览条目并入配置：跳过重复 addr+port，回填 id 并规范化 auth。
#[tauri::command]
pub async fn commit_import(
    state: tauri::State<'_, AppState>,
    hosts: Vec<Host>,
) -> AppResult<usize> {
    let mut added = 0usize;
    state
        .mutate_config(|c| {
            added = commit_hosts_into(c, hosts);
        })
        .await?;
    Ok(added)
}

/// 备份 JSON 中的主机条目：绝不包含 keyRef；
/// 认证方式随备份走，但「是否已保存密码」不在其中。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct BackupHost {
    id: String,
    name: String,
    addr: String,
    user: String,
    port: u16,
    group: String,
    tag: String,
    auth: AuthMethod,
    known: bool,
    agent: bool,
    note: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Backup {
    version: u32,
    exported_at: String,
    /// 主机分组（含颜色）：属于纯配置、不含任何秘密，必须随备份一起走，
    /// 否则导入后分组颜色与空分组会丢。
    folders: Vec<Folder>,
    hosts: Vec<BackupHost>,
    snippets: Vec<Snippet>,
    tunnels: Vec<TunnelRule>,
    preferences: Preferences,
}

fn exported_at() -> String {
    // Unix 秒（无 chrono 依赖）
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("unix:{secs}")
}

/// 备份导入的大小上限：正常备份 <100 KiB；防止被指向超大文件造成长读/OOM。
pub(crate) const MAX_BACKUP_BYTES: u64 = 8 * 1024 * 1024;

/// 对话框选中的路径 + 一次性消费 token。
/// `path` 仅用于前端展示（toast「已导出到 …」）；实际 IPC 一律走 token。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PickedPath {
    pub token: String,
    pub path: String,
}

/// 校验备份文件路径：必须是 `.json` 文件（不是目录）。
fn validate_backup_path(path: &std::path::Path) -> AppResult<()> {
    let is_json = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("json"));
    if !is_json {
        return Err(AppError::EInvalidInput(format!(
            "备份文件必须是 .json：{}",
            path.display()
        )));
    }
    Ok(())
}

/// 消费 token 并校验路径：token 一次性、10 分钟过期；路径必须是 `.json`。
async fn consume_backup_token(
    state: &tauri::State<'_, AppState>,
    token: &str,
) -> AppResult<std::path::PathBuf> {
    let path = state.consume_dialog_path(token).await.ok_or_else(|| {
        AppError::EInvalidInput(
            "备份路径已失效：请重新选择文件（路径凭据仅一次且 10 分钟内有效）".to_string(),
        )
    })?;
    validate_backup_path(&path)?;
    Ok(path)
}

/// 导出不含密钥路径 / 密码的备份 JSON（路径来自对话框 token，不接受任意路径）。
#[tauri::command]
pub async fn export_backup(state: tauri::State<'_, AppState>, save_token: String) -> AppResult<()> {
    let path = consume_backup_token(&state, &save_token).await?;
    let json = {
        let c = state.config.read().await;
        build_backup_json(&c)?
    };
    // 用户自选文件：不留 .bak、不留 tmp 残留（write_atomic 失败自动清理）。
    config::write_atomic(&path, &json, None)
}

/// 把当前配置构造成备份 JSON 文本（导出与「配置互传」共用同一语义）。
///
/// 只含主机/分组/片段/隧道/偏好；**不含** keyRef、密码、私钥、会话日志内容。
pub fn build_backup_json(c: &Config) -> AppResult<String> {
    let backup = Backup {
        version: config::CONFIG_VERSION,
        exported_at: exported_at(),
        folders: c.folders.clone(),
        hosts: c
            .hosts
            .iter()
            .map(|h| BackupHost {
                id: h.id.clone(),
                name: h.name.clone(),
                addr: h.addr.clone(),
                user: h.user.clone(),
                port: h.port,
                group: h.group.clone(),
                tag: h.tag.clone(),
                auth: h.auth,
                known: h.known,
                agent: h.agent,
                note: h.note.clone(),
            })
            .collect(),
        snippets: c.snippets.clone(),
        tunnels: c.tunnels.clone(),
        preferences: c.preferences.clone(),
    };
    serde_json::to_string_pretty(&backup)
        .map_err(|e| AppError::EConfigIo(format!("备份序列化失败: {e}")))
}

/// 从备份 JSON 整体替换配置；无论备份内容如何，keyRef 一律清空。
/// 旧配置中被移除的 host 会逐个 best-effort 清理钥匙串凭据（失败不阻断导入）。
/// 路径来自对话框 token（一次性），并限制文件大小。
#[tauri::command]
pub async fn import_backup(
    state: tauri::State<'_, AppState>,
    open_token: String,
) -> AppResult<Config> {
    let path = consume_backup_token(&state, &open_token).await?;
    let meta = std::fs::metadata(&path)
        .map_err(|e| AppError::EConfigIo(format!("读取备份 {} 失败: {e}", path.display())))?;
    if !meta.is_file() {
        return Err(AppError::EInvalidInput(format!(
            "备份路径不是文件：{}",
            path.display()
        )));
    }
    if meta.len() > MAX_BACKUP_BYTES {
        return Err(AppError::EInvalidInput(format!(
            "备份文件过大（{} 字节，上限 {} MiB）",
            meta.len(),
            MAX_BACKUP_BYTES / (1024 * 1024)
        )));
    }
    let text = std::fs::read_to_string(&path)
        .map_err(|e| AppError::EConfigIo(format!("读取备份 {} 失败: {e}", path.display())))?;
    apply_backup_text(&state, &text).await
}

/// 用备份文本整体替换配置（文件导入与「配置互传」收件箱共用）。
///
/// 校验 → 合并（沿用备份语义：整体替换）→ 原子写 → 清理失效凭据；
/// 调用方负责确认（UI 二次确认）与来源大小限制。
pub async fn apply_backup_text(
    state: &tauri::State<'_, AppState>,
    text: &str,
) -> AppResult<Config> {
    let new_config = parse_backup_config(text)?;
    // diff 出旧配置中被移除 / 身份已变的 host id：
    // - 已移除：凭据不再被任何主机引用，删除；
    // - 同 id 但 addr/user/auth 变化：凭据是按 host.id 存的，若保留，
    //   下次连接会把旧密码自动发往新地址 —— 必须视为“需要重新输入密码”。
    let (removed_ids, stale_ids) = {
        let c = state.config.read().await;
        let removed: Vec<String> = c
            .hosts
            .iter()
            .filter(|h| !new_config.hosts.iter().any(|n| n.id == h.id))
            .map(|h| h.id.clone())
            .collect();
        (removed, stale_secret_host_ids(&c.hosts, &new_config.hosts))
    };
    // 整体替换
    state.mutate_config(|c| *c = new_config.clone()).await?;
    for id in removed_ids.iter().chain(stale_ids.iter()) {
        // keyring 是同步阻塞 API（凭据管理器可能因漫游/提供程序卡顿数秒），
        // 必须放阻塞线程池（与 commands/secret.rs 的 blocking_keyring 同一规范）。
        let cleared = tokio::task::spawn_blocking({
            let id = id.clone();
            move || crate::secret::delete_password(&id)
        })
        .await
        .map_err(|e| AppError::ESecret(format!("钥匙串任务执行失败: {e}")));
        match cleared {
            Ok(Ok(())) => {}
            Ok(Err(e)) | Err(e) => {
                applog!("[config] 清理主机 {id} 的钥匙串凭据失败（忽略）: {e}");
            }
        }
    }
    Ok(new_config)
}

/// 找出“同 id 但主机身份已变（地址/用户/认证方式）”的旧主机 id。
///
/// 凭据只按 `Host.id` 绑定（`secret::Entry::new(SERVICE, host_id)`），所以备份导入不能
/// 仅凭 id 相同就沿用旧密码：否则一份把 addr 改掉、id 保持不变的备份会让用户把已保存
/// 的密码静默发往另一个地址。被移除的 id 由调用方另行处理（本函数不重复返回）。
fn stale_secret_host_ids(old: &[Host], new: &[Host]) -> Vec<String> {
    old.iter()
        .filter_map(|o| {
            let n = new.iter().find(|n| n.id == o.id)?;
            let changed = o.addr != n.addr || o.user != n.user || o.auth != n.auth;
            changed.then(|| o.id.clone())
        })
        .collect()
}

/// 备份是外部输入：缺失 id 回填、重复 id 重新生成（与磁盘解析 / SSH 配置导入的
/// “id 唯一且非空”约定保持一致；否则两台主机会共用一条钥匙串凭据）。
fn normalize_imported_host_ids(hosts: &mut [Host]) {
    let mut seen: HashSet<String> = HashSet::new();
    for host in hosts.iter_mut() {
        while host.id.trim().is_empty() || seen.contains(&host.id) {
            host.id = uuid::Uuid::new_v4().to_string();
        }
        seen.insert(host.id.clone());
    }
}

/// 新增主机分组（`folders`，名称唯一）。名称已存在时只更新颜色，不重复追加。
///
/// 关键点：`folders` 为空时先用**现有主机上出现过的分组**播种。前端侧栏在
/// `folders` 非空时只渲染 `folders`（不再按 host.group 派生），如果直接 push 第一条，
/// 其余已有分组会从侧栏消失。
#[tauri::command]
pub async fn upsert_folder(
    state: tauri::State<'_, AppState>,
    name: String,
    color: Option<String>,
) -> AppResult<Folder> {
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err(AppError::EInvalidInput("分组名不能为空".to_string()));
    }
    let color = color.unwrap_or_default();
    let folder = Folder {
        name: name.clone(),
        color: color.clone(),
    };
    state
        .mutate_config(|c| {
            if c.folders.is_empty() {
                let mut seen: Vec<String> = Vec::new();
                for h in &c.hosts {
                    let g = h.group.trim().to_string();
                    if !g.is_empty() && !seen.contains(&g) {
                        seen.push(g);
                    }
                }
                c.folders = seen
                    .into_iter()
                    .map(|n| Folder {
                        name: n,
                        color: String::new(),
                    })
                    .collect();
            }
            if let Some(existing) = c.folders.iter_mut().find(|f| f.name == name) {
                existing.color = color.clone();
            } else {
                c.folders.push(Folder {
                    name: name.clone(),
                    color: color.clone(),
                });
            }
        })
        .await?;
    Ok(folder)
}

/// 重命名主机分组（右键菜单）：同步更新 folders 条目与所有成员主机的 `group`。
/// 新名必须非空且未与其它分组重名；原分组不存在返回 E_NOT_FOUND。
#[tauri::command]
pub async fn rename_folder(
    state: tauri::State<'_, AppState>,
    old_name: String,
    new_name: String,
) -> AppResult<()> {
    let old_name = old_name.trim().to_string();
    let new_name = new_name.trim().to_string();
    if old_name.is_empty() || new_name.is_empty() {
        return Err(AppError::EInvalidInput("分组名不能为空".to_string()));
    }
    if old_name == new_name {
        return Ok(());
    }
    // mutate_config 的闭包无返回值：校验错误经 Mutex 带出（校验与写入同一把写锁内，无 TOCTOU）；
    // 用 Mutex 而非 RefCell：async 命令的 future 必须 Send/Sync
    let result: std::sync::Mutex<AppResult<()>> = std::sync::Mutex::new(Ok(()));
    state
        .mutate_config(|c| {
            if !c.folders.iter().any(|f| f.name == old_name) {
                *result.lock().unwrap() =
                    Err(AppError::ENotFound(format!("分组 {old_name} 不存在")));
                return;
            }
            if c.folders.iter().any(|f| f.name == new_name) {
                *result.lock().unwrap() =
                    Err(AppError::EInvalidInput(format!("分组 {new_name} 已存在")));
                return;
            }
            for f in c.folders.iter_mut() {
                if f.name == old_name {
                    f.name = new_name.clone();
                }
            }
            for h in c.hosts.iter_mut() {
                if h.group == old_name {
                    h.group = new_name.clone();
                }
            }
        })
        .await?;
    result.into_inner().unwrap()
}

/// 删除主机分组（右键菜单）：从 folders 移除；组内主机**不删除**，改挂到
/// 剩余的第一个分组；没有剩余分组时清空 host.group（folders 为空 → 侧栏按
/// host.group 派生渲染，主机仍然可见，不会“删个分组主机就消失”）。
/// 返回被移动的主机数（前端提示用）。
#[tauri::command]
pub async fn delete_folder(state: tauri::State<'_, AppState>, name: String) -> AppResult<usize> {
    let name = name.trim().to_string();
    let moved: std::sync::Mutex<usize> = std::sync::Mutex::new(0);
    let result: std::sync::Mutex<AppResult<()>> = std::sync::Mutex::new(Ok(()));
    state
        .mutate_config(|c| {
            if !c.folders.iter().any(|f| f.name == name) {
                *result.lock().unwrap() = Err(AppError::ENotFound(format!("分组 {name} 不存在")));
                return;
            }
            c.folders.retain(|f| f.name != name);
            // 剩余的第一个分组作为成员主机的新家；没有剩余分组则回到未分组
            let fallback = c
                .folders
                .first()
                .map(|f| f.name.clone())
                .unwrap_or_default();
            for h in c.hosts.iter_mut() {
                if h.group == name {
                    h.group = fallback.clone();
                    *moved.lock().unwrap() += 1;
                }
            }
        })
        .await?;
    result.into_inner().unwrap()?;
    Ok(moved.into_inner().unwrap())
}

/// 备份导出：原生「另存为」对话框。**以主窗口为父窗口**，保证对话框始终弹在应用前面
/// （直接用 JS 插件拿到的对话框没有 owner，某些窗口状态下会被主窗口盖住，
/// 用户会以为按钮没反应）。用户取消返回 None；对话框打不开则返回错误，前端提示。
///
/// 返回 `{token, path}`——`path` 仅给前端展示，后续 `export_backup` 只认 token
///（一次性、10 分钟过期），WebView 无法让后端写任意路径。
#[tauri::command]
pub async fn pick_backup_save_path(
    window: tauri::WebviewWindow,
    state: tauri::State<'_, AppState>,
    title: String,
    file_name: String,
) -> AppResult<Option<PickedPath>> {
    use tauri_plugin_dialog::DialogExt;
    let (tx, rx) = tokio::sync::oneshot::channel();
    window
        .dialog()
        .file()
        .set_title(title)
        .set_file_name(file_name)
        .add_filter("JSON", &["json"])
        .set_parent(&window)
        .save_file(move |picked| {
            let _ = tx.send(picked);
        });
    let picked = rx
        .await
        .map_err(|_| AppError::EConfigIo("保存对话框未能打开".to_string()))?;
    let Some(path) = picked.and_then(|fp| fp.into_path().ok()) else {
        return Ok(None);
    };
    let token = state.register_dialog_path(path.clone()).await;
    Ok(Some(PickedPath {
        token,
        path: path.to_string_lossy().to_string(),
    }))
}

/// 备份导入：原生「打开」对话框（同样以主窗口为父窗口）。返回语义同 [`pick_backup_save_path`]。
#[tauri::command]
pub async fn pick_backup_open_path(
    window: tauri::WebviewWindow,
    state: tauri::State<'_, AppState>,
    title: String,
) -> AppResult<Option<PickedPath>> {
    use tauri_plugin_dialog::DialogExt;
    let (tx, rx) = tokio::sync::oneshot::channel();
    window
        .dialog()
        .file()
        .set_title(title)
        .add_filter("JSON", &["json"])
        .set_parent(&window)
        .pick_file(move |picked| {
            let _ = tx.send(picked);
        });
    let picked = rx
        .await
        .map_err(|_| AppError::EConfigIo("打开对话框未能打开".to_string()))?;
    let Some(path) = picked.and_then(|fp| fp.into_path().ok()) else {
        return Ok(None);
    };
    let token = state.register_dialog_path(path.clone()).await;
    Ok(Some(PickedPath {
        token,
        path: path.to_string_lossy().to_string(),
    }))
}

/// 选择目录（设置里的局域网共享根）。返回绝对路径；用户取消返回 null。
/// 与备份对话框一样由后端以主窗口为 parent 打开，避免被主窗口遮住。
#[tauri::command]
pub async fn pick_folder(
    window: tauri::WebviewWindow,
    title: String,
    default_path: Option<String>,
) -> AppResult<Option<String>> {
    use tauri_plugin_dialog::DialogExt;
    let (tx, rx) = tokio::sync::oneshot::channel();
    let mut builder = window.dialog().file().set_title(title).set_parent(&window);
    if let Some(dir) = default_path.filter(|p| !p.trim().is_empty()) {
        builder = builder.set_directory(dir);
    }
    builder.pick_folder(move |picked| {
        let _ = tx.send(picked);
    });
    let picked = rx
        .await
        .map_err(|_| AppError::EConfigIo("选择目录对话框未能打开".to_string()))?;
    Ok(picked
        .and_then(|fp| fp.into_path().ok())
        .map(|p| p.to_string_lossy().to_string()))
}

/// 解析备份 JSON → Config（不含 keyRef / 私钥 / 密码）。
fn parse_backup_config(text: &str) -> AppResult<Config> {
    let parsed: serde_json::Value = serde_json::from_str(text)
        .map_err(|e| AppError::EInvalidInput(format!("备份 JSON 解析失败: {e}")))?;
    let version = parsed
        .get("version")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| AppError::EInvalidInput("备份缺少 version 字段".to_string()))?;
    if version != config::CONFIG_VERSION as u64 {
        return Err(AppError::EInvalidInput(format!(
            "不支持的备份版本: {version}"
        )));
    }
    let raw_hosts: Vec<serde_json::Value> = serde_json::from_value(
        parsed
            .get("hosts")
            .cloned()
            .unwrap_or(serde_json::Value::Array(vec![])),
    )
    .map_err(|e| AppError::EInvalidInput(format!("备份 hosts 字段无效: {e}")))?;
    let mut hosts = Vec::new();
    for v in raw_hosts {
        // 备份不含 keyRef：无论备份里混入了什么，一律重建为空串，
        // 绝不从备份带入任何密钥路径或内容。
        let auth = match v.get("auth") {
            None | Some(serde_json::Value::Null) => AuthMethod::Key,
            Some(raw) => serde_json::from_value::<AuthMethod>(raw.clone())
                .map_err(|e| AppError::EInvalidInput(format!("备份主机的 auth 字段无效: {e}")))?,
        };
        // port 校验：先按 u64 取出再验证范围，避免 `as u16` 截断回绕
        // （如 65537 → 1、70000 → 4464）。port 字段缺失仍默认 22。
        let raw_port = v.get("port").and_then(|x| x.as_u64()).unwrap_or(22);
        if !(1..=65535).contains(&raw_port) {
            return Err(AppError::EInvalidInput(format!(
                "备份主机端口无效: {raw_port}"
            )));
        }
        let mut host = Host {
            id: v
                .get("id")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string(),
            name: v
                .get("name")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string(),
            addr: v
                .get("addr")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string(),
            user: v
                .get("user")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string(),
            port: raw_port as u16,
            group: v
                .get("group")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string(),
            tag: v
                .get("tag")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string(),
            auth,
            key_ref: String::new(),
            known: v.get("known").and_then(|x| x.as_bool()).unwrap_or(false),
            agent: v.get("agent").and_then(|x| x.as_bool()).unwrap_or(false),
            note: v
                .get("note")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string(),
            // 备份不含会话日志/自动执行字段（备份不含会话内容与本地记录开关），
            // 导入后一律回到默认值。
            log: false,
            init_command: String::new(),
        };
        host.normalize_auth();
        hosts.push(host);
    }
    // 备份是外部输入：空 id / 重复 id 直接落盘会让两台主机共用一条钥匙串凭据，
    // 这里与磁盘解析 / SSH 配置导入保持一致（回填 + 去重）。
    normalize_imported_host_ids(&mut hosts);
    let snippets: Vec<Snippet> = serde_json::from_value(
        parsed
            .get("snippets")
            .cloned()
            .unwrap_or(serde_json::Value::Array(vec![])),
    )
    .map_err(|e| AppError::EInvalidInput(format!("备份 snippets 字段无效: {e}")))?;
    let tunnels: Vec<TunnelRule> = serde_json::from_value(
        parsed
            .get("tunnels")
            .cloned()
            .unwrap_or(serde_json::Value::Array(vec![])),
    )
    .map_err(|e| AppError::EInvalidInput(format!("备份 tunnels 字段无效: {e}")))?;
    let preferences: Preferences = serde_json::from_value(
        parsed
            .get("preferences")
            .cloned()
            .unwrap_or(serde_json::Value::Object(Default::default())),
    )
    .map_err(|e| AppError::EInvalidInput(format!("备份 preferences 字段无效: {e}")))?;
    // 备份是外部输入，数值必须夹到设计范围（fontSize=0 / sidebarWidth=1e9 等）。
    let mut preferences = preferences;
    preferences.clamp_to_ranges();
    // 分组：备份带 folders；缺少该字段（手写备份等）时，用主机上出现过的
    // group 名兜底，保证导入后侧栏分组结构不丢（颜色回落为空）。
    let mut folders: Vec<Folder> = match parsed.get("folders") {
        Some(v) if !v.is_null() => serde_json::from_value(v.clone())
            .map_err(|e| AppError::EInvalidInput(format!("备份 folders 字段无效: {e}")))?,
        _ => Vec::new(),
    };
    if folders.is_empty() {
        let mut seen: Vec<String> = Vec::new();
        for h in &hosts {
            let g = h.group.trim();
            if !g.is_empty() && !seen.iter().any(|x| x == g) {
                seen.push(g.to_string());
            }
        }
        folders = seen
            .into_iter()
            .map(|name| Folder {
                name,
                color: String::new(),
            })
            .collect();
    }

    Ok(Config {
        version: config::CONFIG_VERSION,
        preferences,
        folders,
        hosts,
        snippets,
        tunnels,
    })
}

// ---------------- Rust 单测 ----------------

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    /// 备份必须带上主机分组（名称 + 颜色），否则导出再导入后分组颜色与空分组会消失。
    #[test]
    fn backup_roundtrip_keeps_folders() {
        let json = serde_json::json!({
            "version": 1,
            "exportedAt": "unix:0",
            "folders": [
                { "name": "生产环境", "color": "#ff0000" },
                { "name": "空分组", "color": "" }
            ],
            "hosts": [{
                "id": "h1", "name": "web", "addr": "1.1.1.1", "user": "u", "port": 22,
                "group": "生产环境", "tag": "prod", "auth": "key",
                "known": false, "agent": false, "note": ""
            }],
            "snippets": [],
            "tunnels": [],
            "preferences": {}
        });
        let cfg = parse_backup_config(&json.to_string()).expect("备份应可解析");
        assert_eq!(cfg.folders.len(), 2);
        assert_eq!(cfg.folders[0].name, "生产环境");
        assert_eq!(cfg.folders[0].color, "#ff0000");
        // 空分组也要保留（用户可能先建组再放主机）
        assert_eq!(cfg.folders[1].name, "空分组");
        assert_eq!(cfg.hosts.len(), 1);
    }

    /// 缺少 folders 字段的备份导入时，用主机上出现过的 group 兜底生成分组，保持侧栏结构。
    #[test]
    fn legacy_backup_without_folders_derives_from_hosts() {
        let json = serde_json::json!({
            "version": 1,
            "exportedAt": "unix:0",
            "hosts": [
                { "id": "h1", "name": "a", "addr": "1.1.1.1", "user": "u", "port": 22, "group": "生产环境", "tag": "prod", "auth": "key" },
                { "id": "h2", "name": "b", "addr": "2.2.2.2", "user": "u", "port": 22, "group": "开发机", "tag": "dev", "auth": "key" },
                { "id": "h3", "name": "c", "addr": "3.3.3.3", "user": "u", "port": 22, "group": "生产环境", "tag": "prod", "auth": "key" }
            ],
            "snippets": [],
            "tunnels": [],
            "preferences": {}
        });
        let cfg = parse_backup_config(&json.to_string()).expect("旧备份应可解析");
        let names: Vec<&str> = cfg.folders.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["生产环境", "开发机"],
            "应按出现顺序去重生成分组"
        );
        assert!(cfg.folders.iter().all(|f| f.color.is_empty()));
    }

    #[test]
    fn parse_ignores_wildcard_match_and_comments() {
        let text = r#"
# a comment
Host *
  User nobody

Host web-01
  HostName web-01.example.com
  User deploy
  Port 22
  IdentityFile ~/.ssh/id_ed25519

Match host staging-*
  User robot

Host staging-api
  HostName 10.0.0.5
  Port 2222
"#;
        let parsed = parse_ssh_config(text);
        assert_eq!(parsed.len(), 2);
        let w = &parsed[0];
        assert_eq!(w.name, "web-01");
        assert_eq!(w.addr, "web-01.example.com");
        assert_eq!(w.user, "deploy");
        assert_eq!(w.port, 22);
        assert_eq!(w.key_ref, "~/.ssh/id_ed25519");
        let s = &parsed[1];
        assert_eq!(s.name, "staging-api");
        assert_eq!(s.port, 2222);
        assert_eq!(s.user, "");
    }

    #[test]
    fn parse_default_port() {
        let text = "Host a\n  HostName a.example.com\n";
        let parsed = parse_ssh_config(text);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].port, 22);
        assert_eq!(parsed[0].addr, "a.example.com");
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
            known: true,
            agent: false,
            note: "主站".into(),
            log: false,
            init_command: String::new(),
        }
    }

    fn backup_json(host: &Host) -> String {
        let backup = Backup {
            version: 1,
            exported_at: exported_at(),
            folders: Vec::new(),
            hosts: vec![BackupHost {
                id: host.id.clone(),
                name: host.name.clone(),
                addr: host.addr.clone(),
                user: host.user.clone(),
                port: host.port,
                group: host.group.clone(),
                tag: host.tag.clone(),
                auth: host.auth,
                known: host.known,
                agent: host.agent,
                note: host.note.clone(),
            }],
            snippets: Vec::new(),
            tunnels: Vec::new(),
            preferences: Preferences::default(),
        };
        serde_json::to_string_pretty(&backup).unwrap()
    }

    /// 备份包含 auth，但不含 keyRef / password / saved / 私钥内容。
    #[test]
    fn backup_json_has_auth_but_no_secret_fields() {
        let host = sample_host(AuthMethod::Password, r"~\.ssh\id_ed25519");
        let json = backup_json(&host);
        assert!(!json.contains("keyRef"), "备份 JSON 不应包含 keyRef");
        assert!(
            !json.contains(&host.key_ref),
            "备份 JSON 不应包含私钥路径内容"
        );
        assert!(json.contains("\"auth\""), "备份 JSON 必须保留认证方式");
        assert!(
            json.contains("\"password\""),
            "auth=password 应以值形式出现"
        );
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let host_obj = parsed["hosts"][0].as_object().unwrap();
        assert_eq!(host_obj["auth"], "password");
        for forbidden in ["password", "saved", "pw"] {
            assert!(
                !host_obj.contains_key(forbidden),
                "备份 JSON 不得包含 {forbidden} 字段: {json}"
            );
        }
        assert!(!json.contains("PRIVATE KEY"));
        assert!(json.contains("fontSize"));
    }

    /// 备份导入：带 auth 与缺 auth 的备份都能导入，keyRef 一律为空。
    #[test]
    fn import_backup_restores_auth_without_key_ref() {
        let prefs = serde_json::to_value(Preferences::default()).unwrap();
        let base_host = |auth: Option<&str>| {
            let mut h = serde_json::json!({
                "id": "h1",
                "name": "web-01",
                "addr": "web-01.example.com",
                "user": "deploy",
                "port": 22,
                "group": "prod",
                "tag": "prod",
                "known": false,
                "agent": false,
                "note": "",
                // 即使备份里混入了 keyRef，也不得被带入
                "keyRef": r"C:\keys\id_rsa",
            });
            if let Some(auth) = auth {
                h["auth"] = serde_json::Value::String(auth.to_string());
            }
            h
        };

        for (raw_auth, expect) in [
            (Some("password"), AuthMethod::Password),
            (Some("agent"), AuthMethod::Agent),
            (Some("key"), AuthMethod::Key),
            (None, AuthMethod::Key),
        ] {
            let value = serde_json::json!({
                "version": 1,
                "hosts": [base_host(raw_auth)],
                "snippets": [],
                "tunnels": [],
                "preferences": prefs.clone(),
            });
            let cfg = parse_backup_config(&value.to_string()).unwrap();
            assert_eq!(cfg.hosts[0].auth, expect, "raw auth = {raw_auth:?}");
            assert_eq!(cfg.hosts[0].key_ref, "", "备份导入不得带入任何私钥路径");
        }

        let bad = serde_json::json!({
            "version": 1,
            "hosts": [base_host(Some("nonsense"))],
            "snippets": [],
            "tunnels": [],
            "preferences": prefs,
        });
        assert!(matches!(
            parse_backup_config(&bad.to_string()),
            Err(AppError::EInvalidInput(_))
        ));
    }

    /// commit_import 落盘前清掉非 key 认证的 keyRef。
    #[test]
    fn commit_import_normalizes_key_ref() {
        let mut config = crate::config::default_config();
        let password_host = sample_host(AuthMethod::Password, r"C:\keys\id_rsa");
        let agent_host = Host {
            addr: "agent.example.com".into(),
            ..sample_host(AuthMethod::Agent, r"C:\keys\id_agent")
        };
        let key_host = Host {
            addr: "key.example.com".into(),
            ..sample_host(AuthMethod::Key, r"~\.ssh\id_ed25519")
        };
        let added = commit_hosts_into(&mut config, vec![password_host, agent_host, key_host]);
        assert_eq!(added, 3);
        assert_eq!(config.hosts[0].key_ref, "");
        assert_eq!(config.hosts[1].key_ref, "");
        assert_eq!(config.hosts[2].key_ref, r"~\.ssh\id_ed25519");
        assert!(config.hosts.iter().all(|h| !h.id.is_empty()), "应回填 id");

        let saved = crate::config::serialize_config(&config).unwrap();
        assert!(saved.contains("auth = \"password\""), "{saved}");
        assert!(saved.contains("auth = \"agent\""), "{saved}");
        assert!(
            !saved.contains("id_rsa"),
            "落盘不得残留无用私钥路径:\n{saved}"
        );
        assert!(
            !saved.contains("id_agent"),
            "落盘不得残留无用私钥路径:\n{saved}"
        );
    }

    #[test]
    fn commit_import_skips_duplicate_addr_and_port() {
        let mut config = crate::config::default_config();
        let first = sample_host(AuthMethod::Key, r"~\.ssh\id_ed25519");
        let duplicate = Host {
            id: "other".into(),
            ..sample_host(AuthMethod::Key, r"~\.ssh\id_ed25519")
        };
        let added = commit_hosts_into(&mut config, vec![first, duplicate]);
        assert_eq!(added, 1);
        assert_eq!(config.hosts.len(), 1);
    }

    /// 同 id 但地址 / 用户 / 认证方式变化 → 必须清理该 id 的钥匙串密码，
    /// 否则下次连接会把旧密码静默发往新地址（备份可任意改写 addr）。
    #[test]
    fn stale_secret_ids_cover_identity_changes() {
        let mut old = vec![
            sample_host(AuthMethod::Password, ""),
            Host {
                id: "keep".into(),
                addr: "same.example.com".into(),
                user: "deploy".into(),
                auth: AuthMethod::Password,
                ..sample_host(AuthMethod::Password, "")
            },
            Host {
                id: "gone".into(),
                ..sample_host(AuthMethod::Key, "")
            },
        ];
        let new = vec![
            // addr 变了
            Host {
                id: "h1".into(),
                addr: "evil.example.com".into(),
                ..sample_host(AuthMethod::Password, "")
            },
            // 完全相同
            Host {
                id: "keep".into(),
                addr: "same.example.com".into(),
                user: "deploy".into(),
                auth: AuthMethod::Password,
                ..sample_host(AuthMethod::Password, "")
            },
            // user 变了
            Host {
                id: "samehost".into(),
                addr: "h.example.com".into(),
                user: "other".into(),
                auth: AuthMethod::Password,
                ..sample_host(AuthMethod::Password, "")
            },
            // auth 变了（password → key）
            Host {
                id: "authchange".into(),
                addr: "a.example.com".into(),
                user: "deploy".into(),
                auth: AuthMethod::Key,
                ..sample_host(AuthMethod::Password, "")
            },
        ];
        old[0].id = "samehost".into();
        old[1].id = "keep".into();
        old[2].id = "gone".into();
        let stale = stale_secret_host_ids(&old, &new);
        assert_eq!(
            stale,
            vec!["samehost".to_string()],
            "只返回身份已变的 id: {stale:?}"
        );
        // 身份完全一致时不得误删凭据（同一台机器的正常备份恢复）
        assert!(
            stale_secret_host_ids(&old, &old.clone()).is_empty(),
            "与旧配置一致时不得误删凭据"
        );
    }

    /// 备份里空 id / 重复 id 必须回填成互不相同的稳定 id。
    #[test]
    fn backup_host_ids_are_backfilled_and_deduplicated() {
        let json = serde_json::json!({
            "version": 1,
            "hosts": [
                { "id": "dup", "name": "a", "addr": "1.1.1.1", "user": "u", "port": 22, "auth": "key" },
                { "id": "dup", "name": "b", "addr": "2.2.2.2", "user": "u", "port": 22, "auth": "key" },
                { "name": "c", "addr": "3.3.3.3", "user": "u", "port": 22, "auth": "key" }
            ],
            "snippets": [],
            "tunnels": [],
            "preferences": {}
        });
        let cfg = parse_backup_config(&json.to_string()).expect("备份应可解析");
        assert_eq!(cfg.hosts.len(), 3);
        let ids: HashSet<&str> = cfg.hosts.iter().map(|h| h.id.as_str()).collect();
        assert_eq!(ids.len(), 3, "id 必须互不相同: {ids:?}");
        assert!(cfg.hosts.iter().all(|h| !h.id.trim().is_empty()));
    }
}

#[cfg(test)]
mod include_tests {
    use super::{expand_include_spec, glob_match, parse_ssh_config, read_ssh_config_with_includes};

    #[test]
    fn glob_matches_star_and_question() {
        assert!(glob_match("config.d/*", "config.d/a.conf"));
        assert!(!glob_match("*.conf", "a.txt"));
        assert!(glob_match("ho?t", "host"));
    }

    #[test]
    fn include_is_inlined_recursively() {
        let dir = std::env::temp_dir().join(format!("ruthenium-sshcfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("config.d")).unwrap();
        std::fs::write(
            dir.join("config"),
            "Host main\n  HostName 10.0.0.1\nInclude config.d/*\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("config.d").join("extra.conf"),
            "Host extra\n  HostName 10.0.0.2\n",
        )
        .unwrap();

        let text = read_ssh_config_with_includes(&dir.join("config"), 0).unwrap();
        assert!(text.contains("Host main"), "{text}");
        assert!(text.contains("Host extra"), "Include 未展开:\n{text}");

        let hosts = parse_ssh_config(&text);
        let names: Vec<_> = hosts.iter().map(|h| h.name.clone()).collect();
        assert!(names.contains(&"main".to_string()), "{names:?}");
        assert!(names.contains(&"extra".to_string()), "{names:?}");

        let specs = expand_include_spec(&dir, "config.d/*");
        assert_eq!(specs.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
