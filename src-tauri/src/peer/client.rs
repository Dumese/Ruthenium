//! 连接已配对设备（SFTP 文件互传 / 配置推送）。
//!
//! 与普通 SSH 主机的区别只在两点：
//! 1. **主机密钥钉扎**：只接受 `peers.toml` 里记录的公钥，不查 known_hosts、不弹指纹框；
//! 2. **身份密钥认证**：用本机长期 Ed25519 身份私钥认证（对方按配对记录校验）。
//!
//! 每个用途独立一条连接（配置推送用独立短连接，避免长连接上的阻塞语义），
//! 文件互传则复用一条 `SessionKind::Sftp` 会话（前端 SFTP 标签按 peer id 去重）。

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use russh::keys::{PrivateKeyWithHashAlg, PublicKey};
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, Mutex};

use crate::error::{AppError, AppResult};
use crate::models::SessionKind;
use crate::peer::identity::Identity;
use crate::peer::peers::PeerRecord;
use crate::ssh::client::ClientHandler;
use crate::ssh::{make_session_handle, new_session_id, LiveSession};
use crate::state::AppState;

/// 对端 SSH 认证用户名：服务端按公钥识别设备，不区分用户名（保留可读值便于审计）。
const PEER_SSH_USER: &str = "ruthenium";

/// 解析对端地址（`IP:port`，IPv6 用 `[..]:port`）。
fn split_addr(addr: &str) -> AppResult<(String, u16)> {
    let (host, port) = addr
        .rsplit_once(':')
        .ok_or_else(|| AppError::EInvalidInput(format!("对端地址缺少端口: {addr}")))?;
    let port: u16 = port
        .parse()
        .map_err(|_| AppError::EInvalidInput(format!("对端端口非法: {addr}")))?;
    Ok((host.trim_matches(['[', ']']).to_string(), port))
}

/// 建立一条**钉扎校验 + 身份认证**的对端连接。
///
/// 复用普通 SSH 客户端的超时/取消设施；区别只在 `ClientHandler.pinned`：
/// 有钉扎时只认 `peers.toml` 里的公钥，不查 known_hosts、不弹指纹框。
pub(crate) async fn connect(
    state: &AppState,
    peer: &PeerRecord,
    purpose: &str,
) -> AppResult<russh::client::Handle<ClientHandler>> {
    let pinned = PublicKey::from_openssh(&peer.pubkey)
        .map_err(|e| AppError::EPeerNotPaired(format!("配对记录中的公钥无法解析: {e}")))?;
    let addr =
        state.peer.peer_addr(&peer.id).await.ok_or_else(|| {
            AppError::EPeerOffline(format!("找不到设备 {} 的可连地址", peer.name))
        })?;
    let (host, port) = split_addr(&addr)?;
    let keepalive_secs = state.config.read().await.preferences.keepalive_secs;
    let mismatch = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let handler = ClientHandler {
        // 钉扎模式不会注册 hostkey 挂起；session_id 仅作占位
        session_id: new_session_id(),
        host_id: peer.id.clone(),
        host: host.clone(),
        port,
        pending: state.pending_hostkey.clone(),
        app: Some(state.app.clone()),
        pinned: Some(pinned),
        pinned_mismatch: Some(mismatch.clone()),
    };
    let handle = crate::ssh::connect_client(
        crate::ssh::client_config(keepalive_secs),
        (host, port),
        handler,
        None,
    )
    .await
    .map_err(|e| {
        if mismatch.load(std::sync::atomic::Ordering::SeqCst) {
            AppError::EPeerKeyMismatch(format!(
                "{} 的身份密钥与配对记录不一致，请重新配对（{e}）",
                peer.name
            ))
        } else {
            AppError::EPeerOffline(format!("连接 {addr} 失败: {e}"))
        }
    })?;
    // 连接成功：把这次用到的地址写回 peers.toml（发现服务关闭时下次仍能直连）
    state.peer.remember_addr(&peer.id, &addr).await;
    applog!("[peer] 已连接 {}（{addr}，{purpose}）", peer.name);
    Ok(handle)
}

/// 用本机身份私钥完成 publickey 认证。
pub(crate) async fn authenticate(
    handle: &mut russh::client::Handle<ClientHandler>,
    identity: &Identity,
) -> AppResult<()> {
    let key = Arc::new(identity.private_key().clone());
    let with_hash = PrivateKeyWithHashAlg::new(key, None);
    let ok = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        handle.authenticate_publickey(PEER_SSH_USER, with_hash),
    )
    .await
    .map_err(|_| AppError::ESshAuth("对端认证超时".to_string()))?
    .map_err(|e| AppError::ESshAuth(format!("对端认证请求失败: {e}")))?;
    if !ok.success() {
        return Err(AppError::EPeerNotPaired(
            "对端不认可本机身份（可能已解除配对），请重新配对".to_string(),
        ));
    }
    Ok(())
}

/// 打开对端 SFTP 子系统（文件互传 / 配置推送共用）。
///
/// `request_subsystem(want_reply)` 只负责发包；被拒绝时服务端回 `ChannelFailure`，
/// 若不主动等回执，客户端只会在 SFTP INIT 处超时（错误信息误导为“网络问题”）。
async fn open_sftp_channel(
    handle: &russh::client::Handle<ClientHandler>,
) -> AppResult<russh_sftp::client::SftpSession> {
    let mut channel = handle
        .channel_open_session()
        .await
        .map_err(|e| AppError::ESshChannel(format!("打开对端 SFTP 通道失败: {e}")))?;
    channel.request_subsystem(true, "sftp").await.map_err(|e| {
        AppError::EPeerNotAllowed(format!("对端拒绝 SFTP（未授予文件互传/配置推送）: {e}"))
    })?;
    match tokio::time::timeout(std::time::Duration::from_secs(5), channel.wait()).await {
        Ok(Some(russh::ChannelMsg::Success)) => {}
        Ok(Some(russh::ChannelMsg::Failure)) => {
            return Err(AppError::EPeerNotAllowed(
                "对端拒绝 SFTP：未授予文件互传/配置推送（或授权已撤销）".to_string(),
            ));
        }
        // 其他回执（理论上不会出现在 Success 之前）交给 SFTP 层处理
        Ok(Some(_)) => {}
        Ok(None) => {
            return Err(AppError::EPeerNotAllowed(
                "对端在建立 SFTP 时关闭了通道（可能已撤销授权）".to_string(),
            ));
        }
        Err(_) => {
            return Err(AppError::EPeerNotAllowed(
                "对端未响应 SFTP 请求（可能未授予权限或对端繁忙）".to_string(),
            ));
        }
    }
    russh_sftp::client::SftpSession::new(channel.into_stream())
        .await
        .map_err(|e| AppError::ESftpIo(format!("建立对端 SFTP 失败: {e}")))
}

/// 打开（或复用）设备文件互传会话，返回前端 SFTP 标签用的 sessionId。
pub async fn open_peer_sftp(state: &AppState, peer_id: &str) -> AppResult<String> {
    let peer = state
        .peer
        .peer_record(peer_id)
        .await
        .ok_or_else(|| AppError::EPeerNotPaired("该设备未配对".to_string()))?;
    // 同一设备已有活动 SFTP 会话：复用前**实时复核**对端授权。
    // 只靠建立时的授权快照会被“撤销后继续复用旧会话”绕过（服务端也会逐请求
    // 复核，这里额外给出明确错误并关掉旧标签）。
    if let Some(sid) = state.ssh.find_connected(&peer.id, SessionKind::Sftp).await {
        let identity = state.peer.identity_handle().await?;
        let mut handle = connect(state, &peer, "SFTP 授权复核").await?;
        authenticate(&mut handle, &identity).await?;
        let info = crate::peer::control::query_info(&handle, None).await?;
        state.peer.sync_peer_allows(&peer.id, &info).await;
        if !info.grant_files {
            let _ = state.ssh.close(&sid).await;
            return Err(AppError::EPeerNotAllowed(format!(
                "{} 已撤销文件互传权限",
                peer.name
            )));
        }
        return Ok(sid);
    }

    let identity = state.peer.identity_handle().await?;
    let mut handle = connect(state, &peer, "SFTP").await?;
    authenticate(&mut handle, &identity).await?;
    // 先等服务端回执：未授权时立刻报错，而不是留一个永远连不上的空标签
    let sftp = open_sftp_channel(&handle).await?;

    let session_id = new_session_id();
    let (inner, _rx) = make_session_handle(session_id.clone(), peer.id.clone(), SessionKind::Sftp);
    // SFTP 没有终端输入：保留一个已断开接收端的控制通道，使 close 的发送失败成为无害 no-op。
    let (ctrl, _ctrl_rx) = mpsc::channel::<crate::ssh::channel::ChannelCmd>(1);
    let (host, port) = split_addr(&state.peer.peer_addr(&peer.id).await.unwrap_or_default())
        .unwrap_or_else(|_| (String::new(), 0));
    let live = Arc::new(LiveSession {
        inner,
        connected: Arc::new(AtomicBool::new(true)),
        russh: Arc::new(Mutex::new(handle)),
        ctrl,
        owns_connection: true,
        addr: host,
        port,
        user: PEER_SSH_USER.to_string(),
        cols: std::sync::atomic::AtomicU16::new(0),
        rows: std::sync::atomic::AtomicU16::new(0),
    });
    state.ssh.insert(live).await;
    state.sftp.put(&session_id, Arc::new(sftp)).await;
    Ok(session_id)
}

/// 把本机配置推送（SFTP 写 `/peers/inbox/<时间戳>__<设备名>.json`），返回写入字节数。
///
/// **每次都用独立短连接**：这样对端「撤销配置推送授权」立刻生效，
/// 不会借一条早先建立的 SFTP 会话绕过。
pub async fn send_config(state: &AppState, peer_id: &str) -> AppResult<u64> {
    let peer = state
        .peer
        .peer_record(peer_id)
        .await
        .ok_or_else(|| AppError::EPeerNotPaired("该设备未配对".to_string()))?;
    let text = {
        let c = state.config.read().await;
        crate::commands::config::build_backup_json(&c)?
    };
    let name = crate::peer::inbox::outbound_name(&state.peer.effective_device_name().await);
    let remote = format!("/peers/inbox/{name}");

    let identity = state.peer.identity_handle().await?;
    let mut handle = connect(state, &peer, "配置推送").await?;
    authenticate(&mut handle, &identity).await?;
    let sftp = open_sftp_channel(&handle).await?;
    // 收件箱目录由对端在监听启动时创建；这里容忍 AlreadyExists。
    let _ = sftp.create_dir("/peers/inbox").await;
    let mut file = sftp.create(remote.as_str()).await.map_err(|e| {
        AppError::EPeerNotAllowed(format!("对端拒绝写入配置（可能已撤销授权）: {e}"))
    })?;
    file.write_all(text.as_bytes())
        .await
        .map_err(|e| AppError::ESftpIo(format!("写入配置失败: {e}")))?;
    file.sync_all()
        .await
        .map_err(|e| AppError::ESftpIo(format!("写入配置失败: {e}")))?;
    let _ = file.close().await;
    let _ = sftp.close().await;
    drop(handle);
    applog!(
        "[peer] 已推送配置到 {}: {}（{} 字节）",
        peer.name,
        remote,
        text.len()
    );
    Ok(text.len() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_addr_handles_ipv4_and_ipv6() {
        assert_eq!(
            split_addr("192.168.1.5:27410").unwrap(),
            ("192.168.1.5".to_string(), 27410)
        );
        assert_eq!(
            split_addr("[fe80::1]:27411").unwrap(),
            ("fe80::1".to_string(), 27411)
        );
        assert!(split_addr("192.168.1.5").is_err());
        assert!(split_addr("192.168.1.5:abc").is_err());
    }
}
