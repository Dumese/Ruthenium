//! 主机与片段 CRUD 命令。
use crate::commands::tunnel::stop_tunnels_for_host;
use crate::error::{AppError, AppResult};
use crate::models::{Host, Snippet};
use crate::state::AppState;

/// 列出全部主机。
#[tauri::command]
pub async fn list_hosts(state: tauri::State<'_, AppState>) -> AppResult<Vec<Host>> {
    Ok(state.config.read().await.hosts.clone())
}

/// 落盘前的统一规范化：
/// - 生成缺失的稳定 id；
/// - `auth != key` 时清空 `key_ref`，不在配置里留一条不会被使用的私钥路径。
///
/// 注意：不再拒绝 port 0 —— 这是「本地终端主机」的约定（前端 port===0 走本地终端，
/// 只会来自手改 hosts.toml / mock 种子）；SSH 连接侧已有自己的 port 0 守卫，
/// 前端 saveForm 也不允许把普通主机保存成 0。
fn prepare_host(mut host: Host) -> AppResult<Host> {
    if host.name.trim().is_empty() {
        return Err(AppError::EInvalidInput("主机名不能为空".to_string()));
    }
    if host.addr.trim().is_empty() {
        return Err(AppError::EInvalidInput("主机地址不能为空".to_string()));
    }
    if host.id.is_empty() {
        host.id = uuid::Uuid::new_v4().to_string();
    }
    host.normalize_auth();
    Ok(host)
}

/// 新增或更新主机；落盘前统一回填 id、清掉无用的 keyRef。
///
/// 编辑导致主机身份变化（addr/port/user/auth 任一改变）时，先关闭该主机的
/// 全部会话并释放转发监听——否则旧连接（连的是旧地址）仍会被 SFTP 复用 / 隧道拉起
/// 命中，把数据读写到错误的服务器。前端按 closed_by_user 事件把标签转为断开态。
#[tauri::command]
pub async fn upsert_host(state: tauri::State<'_, AppState>, host: Host) -> AppResult<Host> {
    let host = prepare_host(host)?;
    let id = host.id.clone();
    let identity_changed = {
        let c = state.config.read().await;
        c.hosts.iter().find(|h| h.id == id).is_some_and(|old| {
            old.addr != host.addr
                || old.port != host.port
                || old.user != host.user
                || old.auth != host.auth
        })
    };
    if identity_changed {
        applog!("[hosts] 主机 {id} 的地址/账号/认证方式已变更，关闭其全部旧会话");
        let session_ids = state.ssh.remove_if(|live| live.inner.host_id == id).await;
        for sid in &session_ids {
            state.sftp.drop_session(sid).await;
            crate::events::emit(
                &state.app,
                crate::events::CONN_STATE,
                crate::events::ConnStatePayload {
                    session_id: sid.clone(),
                    state: "disconnected".to_string(),
                    latency_ms: None,
                    reason: Some(crate::events::CONN_REASON_CLOSED_BY_USER.to_string()),
                },
            );
        }
        stop_tunnels_for_host(&state, &id).await;
    }
    state
        .mutate_config(|c| {
            if let Some(existing) = c.hosts.iter_mut().find(|h| h.id == id) {
                *existing = host.clone();
            } else {
                c.hosts.push(host.clone());
            }
        })
        .await?;
    Ok(host)
}

/// 删除主机：先关闭其全部会话、释放转发监听，再移除配置条目；
/// 最后 best-effort 清理该主机保存在钥匙串里的密码（清理失败不阻断删除流程）。
#[tauri::command]
pub async fn delete_host(state: tauri::State<'_, AppState>, id: String) -> AppResult<()> {
    let exists = state.config.read().await.hosts.iter().any(|h| h.id == id);
    if !exists {
        return Err(AppError::ENotFound(format!("主机 {id} 不存在")));
    }
    // 关闭该主机全部会话 + 释放其转发监听
    let session_ids = state.ssh.remove_if(|live| live.inner.host_id == id).await;
    for sid in &session_ids {
        state.sftp.drop_session(sid).await;
    }
    stop_tunnels_for_host(&state, &id).await;
    state
        .mutate_config(|c| c.hosts.retain(|h| h.id != id))
        .await?;
    // best-effort：凭据清不掉只记日志，配置条目已移除、删除流程不受影响。
    // keyring 是同步阻塞 API（凭据管理器可能因漫游/提供程序卡顿数秒），
    // 必须放阻塞线程池（与 commands/secret.rs 的 blocking_keyring 同一规范）。
    if let Err(e) = tokio::task::spawn_blocking({
        let id = id.clone();
        move || crate::secret::delete_password(&id)
    })
    .await
    .map_err(|e| AppError::ESecret(format!("钥匙串任务执行失败: {e}")))
    .and_then(|r| r)
    {
        applog!("[hosts] 清理主机 {id} 的钥匙串凭据失败（忽略）: {e}");
    }
    Ok(())
}

// ---------------- 片段 ----------------

/// 列出全部命令片段。
#[tauri::command]
pub async fn list_snippets(state: tauri::State<'_, AppState>) -> AppResult<Vec<Snippet>> {
    Ok(state.config.read().await.snippets.clone())
}

/// 新增或更新命令片段；名称不能为空，缺失 id 时回填。
#[tauri::command]
pub async fn upsert_snippet(
    state: tauri::State<'_, AppState>,
    snippet: Snippet,
) -> AppResult<Snippet> {
    if snippet.name.trim().is_empty() {
        return Err(AppError::EInvalidInput("片段名不能为空".to_string()));
    }
    let mut snippet = snippet;
    if snippet.id.is_empty() {
        snippet.id = uuid::Uuid::new_v4().to_string();
    }
    let id = snippet.id.clone();
    state
        .mutate_config(|c| {
            if let Some(existing) = c.snippets.iter_mut().find(|s| s.id == id) {
                *existing = snippet.clone();
            } else {
                c.snippets.push(snippet.clone());
            }
        })
        .await?;
    Ok(snippet)
}

/// 删除命令片段；id 不存在时返回 E_NOT_FOUND。
#[tauri::command]
pub async fn delete_snippet(state: tauri::State<'_, AppState>, id: String) -> AppResult<()> {
    let exists = state
        .config
        .read()
        .await
        .snippets
        .iter()
        .any(|s| s.id == id);
    if !exists {
        return Err(AppError::ENotFound(format!("片段 {id} 不存在")));
    }
    state
        .mutate_config(|c| c.snippets.retain(|s| s.id != id))
        .await
}

// ---------------- Rust 单测 ----------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::AuthMethod;

    fn sample(auth: AuthMethod, key_ref: &str) -> Host {
        Host {
            id: String::new(),
            name: "web-01".into(),
            addr: "web-01.example.com".into(),
            user: "deploy".into(),
            port: 22,
            group: "prod".into(),
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

    /// upsert_host 落盘前必须清掉非 key 认证的 keyRef。
    #[test]
    fn upsert_normalizes_key_ref_for_password_and_agent() {
        for auth in [AuthMethod::Password, AuthMethod::Agent] {
            let host = prepare_host(sample(auth, r"C:\keys\id_rsa")).unwrap();
            assert!(!host.id.is_empty(), "应回填 id");
            assert_eq!(host.key_ref, "", "{auth:?} 不应保留私钥路径");
            assert_eq!(host.auth, auth);
        }
        let key_host = prepare_host(sample(AuthMethod::Key, r"~\.ssh\id_ed25519")).unwrap();
        assert_eq!(key_host.key_ref, r"~\.ssh\id_ed25519");
    }

    /// keyRef 不进入 TOML 的规范化结果（模拟 upsert 后的磁盘内容）。
    #[test]
    fn upsert_saved_toml_has_no_stale_key_ref() {
        let host = prepare_host(sample(AuthMethod::Password, r"C:\keys\id_rsa")).unwrap();
        let mut cfg = crate::config::default_config();
        cfg.hosts.push(host);
        let saved = crate::config::serialize_config(&cfg).unwrap();
        assert!(saved.contains("auth = \"password\""), "{saved}");
        assert!(saved.contains("keyRef = \"\""), "{saved}");
        assert!(!saved.contains("id_rsa"), "{saved}");
    }

    #[test]
    fn upsert_rejects_invalid_host() {
        let mut host = sample(AuthMethod::Key, "");
        host.name = "  ".into();
        assert!(matches!(
            prepare_host(host),
            Err(AppError::EInvalidInput(_))
        ));
    }
}
