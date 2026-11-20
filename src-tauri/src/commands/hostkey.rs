//! 指纹信任 / 拒绝命令。
use crate::error::{AppError, AppResult};
use crate::ssh::known_hosts;
use crate::state::{AppState, PendingHostKey};

/// 信任挂起的主机指纹：核对 fingerprint 后追加 known_hosts 并放行握手。
#[tauri::command]
pub async fn trust_host_key(
    state: tauri::State<'_, AppState>,
    session_id: String,
    fingerprint: String,
) -> AppResult<()> {
    // 先比对再删除——指纹不一致（旧弹窗的迟到点击 / 状态错配）时必须保留
    // pending 条目：它仍由后台 120s 超时兜底清理，且用户本次连接里还能重试；
    // 先删后比会把握手直接掐死且无法再信任。
    let pending: PendingHostKey = {
        let mut map = state.pending_hostkey.lock().await;
        let entry = map
            .get(&session_id)
            .ok_or_else(|| AppError::ENotFound(format!("会话 {session_id} 无挂起的指纹校验")))?;
        if entry.fingerprint != fingerprint {
            return Err(AppError::EInvalidInput(
                "指纹与当前校验中的指纹不一致，已拒绝信任".to_string(),
            ));
        }
        map.remove(&session_id)
            .expect("刚用 get 确认过条目存在，锁内无并发移除")
    };
    // 用「删除同 host:port 旧条目 + 追加新公钥」的原子重写，而不是只追加。
    // 只追加会让旧密钥在 known_hosts 里永久保持 Known，密钥变更告警从此形同虚设。
    // 写入仍只发生在 ~/.ssh/known_hosts（NTFS 继承 ACL，不设权限位）。
    // 读+写+fsync+rename 是阻塞 I/O，放线程池执行。
    if let Err(e) = known_hosts::replace_host_entry_async(
        pending.host.clone(),
        pending.port,
        pending.public_key.clone(),
    )
    .await
    {
        // 写盘失败不能只把 pending 删掉：握手会一直等到 120s 超时，用户也无法
        // 重试。立即按“拒绝”结束本次握手（公钥没有被写入，安全语义不变）。
        let _ = pending.decision_tx.send(false);
        return Err(e);
    }
    let _ = pending.decision_tx.send(true);
    Ok(())
}

/// 拒绝挂起的主机指纹：结束握手，不写入任何文件。
#[tauri::command]
pub async fn reject_host_key(
    state: tauri::State<'_, AppState>,
    session_id: String,
) -> AppResult<()> {
    let pending: PendingHostKey = {
        let mut map = state.pending_hostkey.lock().await;
        map.remove(&session_id)
            .ok_or_else(|| AppError::ENotFound(format!("会话 {session_id} 无挂起的指纹校验")))?
    };
    // 中止连接，不写入任何内容
    let _ = pending.decision_tx.send(false);
    Ok(())
}
