//! 转发规则命令（仅 -L）。
use crate::error::{AppError, AppResult};
use crate::models::{TunnelKind, TunnelRule};
use crate::state::AppState;

/// 列出全部转发规则（包含未启用的）。
#[tauri::command]
pub async fn list_tunnels(state: tauri::State<'_, AppState>) -> AppResult<Vec<TunnelRule>> {
    Ok(state.config.read().await.tunnels.clone())
}

/// 新增或更新一条转发规则；只接受 kind=local，且两端地址必须含端口。
#[tauri::command]
pub async fn upsert_tunnel(
    state: tauri::State<'_, AppState>,
    tunnel: TunnelRule,
) -> AppResult<TunnelRule> {
    if tunnel.kind != TunnelKind::Local {
        return Err(AppError::EInvalidInput("MVP 仅支持 local 转发".to_string()));
    }
    if crate::tunnel::split_addr(&tunnel.local).is_none() {
        return Err(AppError::EInvalidInput(format!(
            "本地地址无效: {}",
            tunnel.local
        )));
    }
    if crate::tunnel::split_addr(&tunnel.remote).is_none() {
        return Err(AppError::EInvalidInput(format!(
            "远端地址无效: {}",
            tunnel.remote
        )));
    }
    let mut tunnel = tunnel;
    if tunnel.id.is_empty() {
        tunnel.id = uuid::Uuid::new_v4().to_string();
    }
    let id = tunnel.id.clone();
    // 编辑一条正在监听的规则时，local/remote/host 可能已经变化：只改配置而不重建监听
    // 会留下“端口仍在转发到旧目标”的幽灵监听，且 start 会因 is_active 早退，规则永远不生效。
    let was_active = state.tunnels.is_active(&id);
    state
        .mutate_config(|c| {
            if let Some(existing) = c.tunnels.iter_mut().find(|t| t.id == id) {
                *existing = tunnel.clone();
            } else {
                c.tunnels.push(tunnel.clone());
            }
        })
        .await?;
    if was_active {
        state.tunnels.stop(&id).await;
        crate::events::emit(
            &state.app,
            crate::events::TUNNEL_STATE,
            crate::events::TunnelStatePayload {
                rule_id: id.clone(),
                active: false,
                error: None,
            },
        );
    }
    // 按新配置重建监听：on=true 且该主机当前有连接时立即启动；绑定失败则把规则停用并上报。
    // 按主机当前配置解析会话（含身份校验）。
    let session_id = if tunnel.on {
        let host = state
            .config
            .read()
            .await
            .hosts
            .iter()
            .find(|h| h.id == tunnel.host)
            .cloned();
        match host {
            Some(h) => state.ssh.connected_session_for_host(&h).await,
            None => None,
        }
    } else {
        None
    };
    if let Some(session_id) = session_id {
        if let Err(e) = state
            .tunnels
            .start(
                state.ssh.clone(),
                &state.app,
                &id,
                &tunnel.local,
                &tunnel.remote,
                &session_id,
            )
            .await
        {
            state
                .mutate_config(|c| {
                    if let Some(t) = c.tunnels.iter_mut().find(|t| t.id == id) {
                        t.on = false;
                    }
                })
                .await?;
            crate::events::emit(
                &state.app,
                crate::events::TUNNEL_STATE,
                crate::events::TunnelStatePayload {
                    rule_id: id,
                    active: false,
                    error: Some(e.to_string()),
                },
            );
            return Err(e);
        }
    }
    Ok(tunnel)
}

/// 删除转发规则并停止其监听；id 不存在时返回 E_NOT_FOUND。
#[tauri::command]
pub async fn delete_tunnel(state: tauri::State<'_, AppState>, id: String) -> AppResult<()> {
    let exists = state.config.read().await.tunnels.iter().any(|t| t.id == id);
    if !exists {
        return Err(AppError::ENotFound(format!("转发规则 {id} 不存在")));
    }
    state.tunnels.stop(&id).await;
    // 删除规则时同步通知前端运行态，避免 store 里留下"仍在监听"的幽灵条目。
    crate::events::emit(
        &state.app,
        crate::events::TUNNEL_STATE,
        crate::events::TunnelStatePayload {
            rule_id: id.clone(),
            active: false,
            error: None,
        },
    );
    state
        .mutate_config(|c| c.tunnels.retain(|t| t.id != id))
        .await
}

/// 启用/停用转发；会话未连接时规则保持「待连接」，连接成功后自动拉起。
#[tauri::command]
pub async fn toggle_tunnel(
    state: tauri::State<'_, AppState>,
    id: String,
    on: bool,
) -> AppResult<()> {
    let rule = state
        .config
        .read()
        .await
        .tunnels
        .iter()
        .find(|t| t.id == id)
        .cloned()
        .ok_or_else(|| AppError::ENotFound(format!("转发规则 {id} 不存在")))?;

    if !on {
        state.tunnels.stop(&id).await;
        state
            .mutate_config(|c| {
                if let Some(t) = c.tunnels.iter_mut().find(|t| t.id == id) {
                    t.on = false;
                }
            })
            .await?;
        crate::events::emit(
            &state.app,
            crate::events::TUNNEL_STATE,
            crate::events::TunnelStatePayload {
                rule_id: id,
                active: false,
                error: None,
            },
        );
        return Ok(());
    }

    // 启用：会话未连接 → 保持「待连接」（规则 on=true，无活跃监听，连接成功后自动拉起）
    // 拉起前按主机当前配置解析会话（含身份校验，防止绑到改地址前的旧连接）。
    let host = state
        .config
        .read()
        .await
        .hosts
        .iter()
        .find(|h| h.id == rule.host)
        .cloned();
    // 按当前主机配置解析会话（含身份校验，防止绑到改地址前的旧连接）。
    let session = match host {
        Some(h) => state.ssh.connected_session_for_host(&h).await,
        None => None,
    };
    match session {
        Some(session_id) => {
            if let Err(e) = state
                .tunnels
                .start(
                    state.ssh.clone(),
                    &state.app,
                    &id,
                    &rule.local,
                    &rule.remote,
                    &session_id,
                )
                .await
            {
                // 绑定失败：规则保持停用
                state
                    .mutate_config(|c| {
                        if let Some(t) = c.tunnels.iter_mut().find(|t| t.id == id) {
                            t.on = false;
                        }
                    })
                    .await?;
                crate::events::emit(
                    &state.app,
                    crate::events::TUNNEL_STATE,
                    crate::events::TunnelStatePayload {
                        rule_id: id,
                        active: false,
                        error: Some(e.to_string()),
                    },
                );
                return Err(e);
            }
            state
                .mutate_config(|c| {
                    if let Some(t) = c.tunnels.iter_mut().find(|t| t.id == id) {
                        t.on = true;
                    }
                })
                .await?;
            Ok(())
        }
        None => {
            state
                .mutate_config(|c| {
                    if let Some(t) = c.tunnels.iter_mut().find(|t| t.id == id) {
                        t.on = true;
                    }
                })
                .await?;
            crate::events::emit(
                &state.app,
                crate::events::TUNNEL_STATE,
                crate::events::TunnelStatePayload {
                    rule_id: id,
                    active: false,
                    error: None, // 前端按「待连接」显示
                },
            );
            Ok(())
        }
    }
}

/// 某 host 的全部 on=true 规则尝试拉起（连接成功后调用）。
pub async fn start_tunnels_for_host(
    state: &tauri::State<'_, AppState>,
    host_id: &str,
) -> AppResult<()> {
    let (rules, host) = {
        let c = state.config.read().await;
        let rules: Vec<TunnelRule> = c
            .tunnels
            .iter()
            .filter(|t| t.host == host_id && t.on)
            .cloned()
            .collect();
        let host = c.hosts.iter().find(|h| h.id == host_id).cloned();
        (rules, host)
    };
    // 按当前主机配置解析会话（含身份校验）。
    let session = match host {
        Some(h) => state.ssh.connected_session_for_host(&h).await,
        None => None,
    };
    let Some(session_id) = session else {
        return Ok(());
    };
    for rule in rules {
        if state.tunnels.is_active(&rule.id) {
            continue;
        }
        if let Err(e) = state
            .tunnels
            .start(
                state.ssh.clone(),
                &state.app,
                &rule.id,
                &rule.local,
                &rule.remote,
                &session_id,
            )
            .await
        {
            // 拉起失败：规则停用并通知
            state
                .mutate_config(|c| {
                    if let Some(t) = c.tunnels.iter_mut().find(|t| t.id == rule.id) {
                        t.on = false;
                    }
                })
                .await?;
            crate::events::emit(
                &state.app,
                crate::events::TUNNEL_STATE,
                crate::events::TunnelStatePayload {
                    rule_id: rule.id,
                    active: false,
                    error: Some(e.to_string()),
                },
            );
        }
    }
    Ok(())
}

/// 关闭某 host 的全部活跃监听（关闭标签 / 重连 / 删除主机时调用）。
/// 每个被停掉的规则都补发 `tunnel://state(active:false)`，前端据此把"监听中"改回"待连接"，
/// 避免重连后 UI 仍显示启用而实际监听已失效。
pub async fn stop_tunnels_for_host(state: &tauri::State<'_, AppState>, host_id: &str) {
    let rule_ids: Vec<String> = state
        .config
        .read()
        .await
        .tunnels
        .iter()
        .filter(|t| t.host == host_id)
        .map(|t| t.id.clone())
        .collect();
    for id in rule_ids {
        state.tunnels.stop(&id).await;
        crate::events::emit(
            &state.app,
            crate::events::TUNNEL_STATE,
            crate::events::TunnelStatePayload {
                rule_id: id,
                active: false,
                error: None,
            },
        );
    }
}
