//! russh::client::Handler 实现：check_server_key 做 known_hosts 校验。
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use russh::client::Handler;
use russh::keys::HashAlg;

use crate::events::{self, AppErrorPayload, HostKeyPayload};
use crate::ssh::known_hosts::{self, HostKeyStatus};
use crate::state::{PendingHostKey, PendingHostKeyMap};

/// 指纹 challenge 等待用户决策的上限：超时按拒绝处理，避免握手无限挂起。
const HOSTKEY_DECISION_TIMEOUT: Duration = Duration::from_secs(120);

/// 指纹确认超时的错误码：握手已按拒绝处理，提示用户重新连接（app://error）。
const E_HOSTKEY_EXPIRED: &str = "E_HOSTKEY_EXPIRED";

/// SSH 客户端回调：握手阶段核对 known_hosts，未知/变更指纹挂起并 emit challenge。
pub struct ClientHandler {
    pub session_id: String,
    pub host_id: String,
    pub host: String,
    pub port: u16,
    /// 挂起指纹决策表（与 AppState 共享）。无挂起条目时 connect 侧超时不放宽；
    /// `app: None` 时仍会注册挂起并等待 120s（E2E 靠此完成自动信任）。
    pub pending: PendingHostKeyMap,
    /// 事件出口：None 时**不 emit 事件**（hostkey://challenge、app://error 均静默），
    /// 但指纹挂起/超时逻辑照常运行（E2E/测试场景依赖）。
    pub app: Option<tauri::AppHandle>,
    /// 已配对设备：只接受这把公钥（钉扎），不查 known_hosts、不弹指纹框。
    /// SET 后 [`check_server_key`] 走钉扎分支——对端不是这台设备就直接拒绝握手。
    pub pinned: Option<russh::keys::PublicKey>,
    /// 钉扎不匹配标记：russh 只能把握手错误变成通用错误，调用方需要靠它
    /// 区分“对端换钥/身份不匹配”和“网络不可达”。
    pub pinned_mismatch: Option<Arc<AtomicBool>>,
}

impl Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &russh::keys::PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let key = server_public_key.public_key();
        // 对等层：钉扎优先，避免把设备公钥写进 known_hosts 或弹出普通主机指纹框。
        if let Some(pinned) = &self.pinned {
            // 只比 key_data：OpenSSH 行的注释（本机写的是 `ruthenium-peer`）不参与身份判断，
            // 而 `PublicKey` 的 PartialEq 会把 comment 一起比较（会误判为不一致）。
            let ok = key.key_data() == pinned.key_data();
            if !ok {
                if let Some(flag) = &self.pinned_mismatch {
                    flag.store(true, Ordering::SeqCst);
                }
                applog!(
                    "[peer] 主机密钥与配对记录不一致，拒绝连接: {}:{}",
                    self.host,
                    self.port
                );
            }
            return Ok(ok);
        }
        let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();
        let algo = key.algorithm().to_string();
        let openssh_key = key.to_string();

        match known_hosts::check_async(self.host.clone(), self.port, fingerprint.clone()).await {
            HostKeyStatus::Known => Ok(true),
            // @revoked 命中 = 服务器密钥已被吊销，一律阻止且不弹指纹挑战。
            // russh 回调的错误类型固定为 russh::Error，无法携带 AppError，
            // 因此向 app://error 上报明确文案后按「拒绝」结束握手：
            // 上层 connect 以 E_SSH_CONNECT 失败，吊销详情经事件送达前端。
            HostKeyStatus::Revoked => {
                applog!(
                    "[ssh] {}:{} 服务器主机密钥已被吊销（@revoked），连接已阻止",
                    self.host,
                    self.port
                );
                if let Some(app) = &self.app {
                    events::emit(
                        app,
                        events::APP_ERROR,
                        AppErrorPayload {
                            code: "E_SSH_HOSTKEY_REVOKED".to_string(),
                            detail: "服务器主机密钥已被吊销（@revoked），连接已阻止".to_string(),
                            session_id: Some(self.session_id.clone()),
                        },
                    );
                }
                Ok(false)
            }
            status @ (HostKeyStatus::Unknown | HostKeyStatus::Changed) => {
                let changed = status == HostKeyStatus::Changed;
                // 挂起握手：注册决策通道，emit hostkey://challenge，等待用户决策。
                let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
                {
                    self.pending.lock().await.insert(
                        self.session_id.clone(),
                        PendingHostKey {
                            host: self.host.clone(),
                            port: self.port,
                            fingerprint: fingerprint.clone(),
                            public_key: openssh_key,
                            decision_tx: tx,
                        },
                    );
                }
                if let Some(app) = &self.app {
                    events::emit(
                        app,
                        events::HOSTKEY_CHALLENGE,
                        HostKeyPayload {
                            session_id: self.session_id.clone(),
                            host_id: self.host_id.clone(),
                            host: self.host.clone(),
                            port: self.port,
                            algo,
                            fingerprint,
                            changed,
                        },
                    );
                }
                // 最长等待 [`HOSTKEY_DECISION_TIMEOUT`]，超时视为拒绝。
                let decision = tokio::time::timeout(HOSTKEY_DECISION_TIMEOUT, rx)
                    .await
                    .ok()
                    .and_then(|r| r.ok())
                    .unwrap_or(false);
                if decision {
                    Ok(true)
                } else {
                    // 拒绝 / 超时：移除挂起记录（若仍在）。显式拒绝时 hostkey 命令
                    // 已先行移除，这里移除得到 None；还能移除到即说明是超时，
                    // 额外上报 E_HOSTKEY_EXPIRED，提示前端本次连接已作废。
                    let expired = self.pending.lock().await.remove(&self.session_id).is_some();
                    if expired {
                        applog!(
                            "[ssh] {}:{} 指纹确认超时（{}s），已按拒绝处理",
                            self.host,
                            self.port,
                            HOSTKEY_DECISION_TIMEOUT.as_secs()
                        );
                        if let Some(app) = &self.app {
                            events::emit(
                                app,
                                events::APP_ERROR,
                                AppErrorPayload {
                                    code: E_HOSTKEY_EXPIRED.to_string(),
                                    detail: "指纹确认已超时，请重新连接".to_string(),
                                    session_id: Some(self.session_id.clone()),
                                },
                            );
                        }
                    }
                    Ok(false)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    /// `peers.toml` 里存的是带注释的公钥行（`... ruthenium-peer`），
    /// 服务端回调给的是不带注释的公钥；`PublicKey` 的 `==` 会连 comment 一起比，
    /// 必须只比 key_data，否则钉扎永远判不一致（表现为 “Unknown server key”）。
    #[test]
    fn pinned_comparison_ignores_openssh_comment() {
        let base =
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIPMHhsyTXbpGp9VyWeerCr+BQMj3l63ZxHaDdVPTnfgY";
        let bare = russh::keys::PublicKey::from_openssh(base).unwrap();
        let with_comment =
            russh::keys::PublicKey::from_openssh(&format!("{base} ruthenium-peer")).unwrap();
        assert_ne!(
            bare, with_comment,
            "PublicKey 的 == 会把注释算进去（所以不能直接用）"
        );
        assert_eq!(bare.key_data(), with_comment.key_data());
    }
}
