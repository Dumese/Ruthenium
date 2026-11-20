//! 认证链：严格按 `host.auth` 分支，不做「所有方式都试一遍」。
//!
//! - `key`：只用 `key_ref`；文件不存在时错误里带展开后的完整路径。
//!   RSA 只用 rsa-sha2-256/512 签名（不回退 ssh-rsa/SHA-1——OpenSSH ≥8.8
//!   已默认禁用，保留它只会在老服务器上降级签名算法）。
//! - `password`：只用 keyring 里的密码（或本次连接传入的一次性密码）；
//!   无密码时返回「未保存密码」，前端据此弹密码框。
//! - `agent`：只试 Windows OpenSSH Agent（最多 [`AGENT_IDENTITY_LIMIT`] 个身份）；
//!   不回退 key_ref（`normalize_auth` 对 auth!=key 清空 keyRef，该回退在生产
//!   不可达，只会留下误导性的错误文案）。
//!
//! 密码只存在于本次调用栈 / keyring 中，任何日志与错误详情都不得包含它。
use std::sync::Arc;

use russh::client;
use russh::keys::PrivateKeyWithHashAlg;
use zeroize::Zeroizing;

use crate::error::{AppError, AppResult};
use crate::models::{AuthMethod, Host};
use crate::secret;
use crate::ssh::client::ClientHandler;

/// agent 最多尝试的身份数：给服务器 MaxAuthTries 留余量，
/// 也避免在 agent 里塞了大量密钥时无谓地消耗认证配额。
const AGENT_IDENTITY_LIMIT: usize = 3;

/// 展开路径中的 `~`：支持 `~` 本身、`~/x` 与 Windows `~\x`（key_ref 允许这些写法），
/// 只做本机 home_dir 替换，不交给 shell。
pub fn expand_tilde(path: &str) -> String {
    let trimmed = path.trim();
    if trimmed == "~" {
        return dirs::home_dir()
            .map(|h| h.to_string_lossy().to_string())
            .unwrap_or_else(|| trimmed.to_string());
    }
    // %USERPROFILE%\... 这类写法来自示例配置，必须先展开环境变量，
    // 否则 Windows 上会把字面量当路径，报「私钥不存在」。
    let expanded = expand_env_vars(trimmed);
    let trimmed = expanded.as_str();
    let rest = trimmed
        .strip_prefix("~/")
        .or_else(|| trimmed.strip_prefix("~\\"));
    match rest {
        Some(rest) => match dirs::home_dir() {
            Some(home) => home.join(rest).to_string_lossy().to_string(),
            None => trimmed.to_string(),
        },
        None => trimmed.to_string(),
    }
}

/// 展开 `%NAME%`（Windows 风格）与 `$NAME` / `${NAME}`（POSIX 风格）环境变量。
/// 找不到的变量原样保留，便于在错误信息里看出是哪个变量没设置。
///
/// 两种形式都要展开：只处理 `%NAME%` 时 Linux/macOS 上
/// `$HOME/.ssh/id_rsa` 这类 keyRef 不会展开，会误报"私钥文件不存在"。
fn expand_env_vars(path: &str) -> String {
    fn push_var(out: &mut String, name: &str, raw: &str) {
        match std::env::var(name) {
            Ok(v) if !name.is_empty() => out.push_str(&v),
            _ => out.push_str(raw),
        }
    }

    let mut out = String::with_capacity(path.len());
    let mut i = 0usize;
    while i < path.len() {
        let ch = path[i..].chars().next().expect("i 位于字符边界");
        match ch {
            '%' => {
                if let Some(end) = path[i + 1..].find('%') {
                    let name = &path[i + 1..i + 1 + end];
                    push_var(&mut out, name, &path[i..i + 1 + end + 1]);
                    i += 1 + end + 1;
                } else {
                    out.push('%');
                    i += 1;
                }
            }
            '$' => {
                let rest = &path[i + 1..];
                if let Some(after_brace) = rest.strip_prefix('{') {
                    match after_brace.find('}') {
                        Some(end) => {
                            let name = &after_brace[..end];
                            push_var(&mut out, name, &path[i..i + 1 + 1 + end + 1]);
                            i += 1 + 1 + end + 1;
                        }
                        None => {
                            out.push('$');
                            i += 1;
                        }
                    }
                } else {
                    // `$NAME`：变量名取 [A-Za-z0-9_] 前缀
                    let name_len = rest
                        .chars()
                        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                        .count();
                    if name_len > 0 {
                        push_var(&mut out, &rest[..name_len], &path[i..i + 1 + name_len]);
                        i += 1 + name_len;
                    } else {
                        // 单独的 `$`（如 shell 风格字面量）：原样输出
                        out.push('$');
                        i += 1;
                    }
                }
            }
            c => {
                out.push(c);
                i += c.len_utf8();
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{expand_env_vars, expand_tilde};

    #[test]
    fn expands_windows_env_var() {
        std::env::set_var("RUTHENIUM_TEST_HOME", "C:\\Users\\test");
        assert_eq!(
            expand_env_vars("%RUTHENIUM_TEST_HOME%\\Documents\\id_rsa"),
            "C:\\Users\\test\\Documents\\id_rsa"
        );
        // 未定义的变量原样保留
        assert_eq!(
            expand_env_vars("%RUTHENIUM_NOT_SET%\\x"),
            "%RUTHENIUM_NOT_SET%\\x"
        );
    }

    /// `$NAME` 与 `${NAME}` 两种 POSIX 写法都要展开。
    #[test]
    fn expands_posix_env_var_forms() {
        std::env::set_var("RUTHENIUM_TEST_POSIX", "/opt/keys");
        assert_eq!(
            expand_env_vars("$RUTHENIUM_TEST_POSIX/id_rsa"),
            "/opt/keys/id_rsa"
        );
        assert_eq!(
            expand_env_vars("${RUTHENIUM_TEST_POSIX}/id_rsa"),
            "/opt/keys/id_rsa"
        );
        // 未定义：两种写法都原样保留，便于报错信息里看出是哪个变量缺失
        assert_eq!(
            expand_env_vars("$RUTHENIUM_NOT_SET/x"),
            "$RUTHENIUM_NOT_SET/x"
        );
        assert_eq!(
            expand_env_vars("${RUTHENIUM_NOT_SET}/x"),
            "${RUTHENIUM_NOT_SET}/x"
        );
        // 裸 `$`、中文路径、以及 %VAR% 与 $VAR 混写都不能破坏原有内容
        assert_eq!(expand_env_vars("a$b"), "a$b");
        assert!(expand_env_vars("密钥/$RUTHENIUM_TEST_POSIX/k").ends_with("/opt/keys/k"));
        assert_eq!(
            expand_env_vars("%RUTHENIUM_TEST_POSIX%:$RUTHENIUM_TEST_POSIX"),
            "/opt/keys:/opt/keys"
        );
        assert_eq!(expand_env_vars(""), "");
    }

    #[test]
    fn expands_forward_slash() {
        let Some(home) = dirs::home_dir() else { return };
        let got = expand_tilde("~/.ssh/id_ed25519");
        assert_eq!(
            got,
            home.join(".ssh/id_ed25519").to_string_lossy().to_string()
        );
    }

    /// Windows 写法 `~\.ssh\id_rsa`（文件选择器压缩后的形式）必须同样展开。
    #[test]
    fn expands_backslash() {
        let Some(home) = dirs::home_dir() else { return };
        let got = expand_tilde("~\\.ssh\\id_rsa");
        let expect = home.join(".ssh\\id_rsa").to_string_lossy().to_string();
        assert_eq!(got, expect);
        assert!(!got.contains('~'), "不应残留 ~: {got}");
    }

    #[test]
    fn leaves_absolute_and_relative_untouched() {
        assert_eq!(expand_tilde("C:\\keys\\id_rsa"), "C:\\keys\\id_rsa");
        assert_eq!(expand_tilde("keys/id_rsa"), "keys/id_rsa");
        assert_eq!(expand_tilde(""), "");
    }

    #[test]
    fn expands_bare_and_trimmed_tilde() {
        let Some(home) = dirs::home_dir() else { return };
        assert_eq!(expand_tilde("~"), home.to_string_lossy().to_string());
        assert_eq!(
            expand_tilde("  ~\\.ssh\\id_rsa  "),
            expand_tilde("~\\.ssh\\id_rsa")
        );
    }
}

type DynAgentClient = russh::keys::agent::client::AgentClient<
    Box<dyn russh::keys::agent::client::AgentStream + Send + Unpin>,
>;

/// 连接本机 SSH Agent。
/// Windows 优先 OpenSSH Authentication Agent 的命名管道，其次 Pageant（兼容旧环境）。
#[cfg(windows)]
async fn connect_agent() -> Option<DynAgentClient> {
    use russh::keys::agent::client::AgentClient;

    match AgentClient::connect_named_pipe(r"\\.\pipe\openssh-ssh-agent").await {
        Ok(client) => {
            applog!("[auth] 已连接 Windows OpenSSH Agent");
            return Some(client.dynamic());
        }
        Err(e) => applog!("[auth] Windows OpenSSH Agent 不可用，尝试 Pageant: {e}"),
    }
    match AgentClient::connect_pageant().await {
        Ok(client) => {
            applog!("[auth] 已连接 Pageant");
            Some(client.dynamic())
        }
        Err(e) => {
            applog!("[auth] Pageant 不可用，跳过 SSH Agent: {e}");
            None
        }
    }
}

#[cfg(not(windows))]
async fn connect_agent() -> Option<DynAgentClient> {
    use russh::keys::agent::client::AgentClient;

    match AgentClient::connect_env().await {
        Ok(client) => Some(client.dynamic()),
        Err(e) => {
            applog!("[auth] ssh-agent（SSH_AUTH_SOCK）不可用，跳过: {e}");
            None
        }
    }
}

/// agent 尝试结果：用于区分「根本没有凭据」与「有凭据但被服务器拒绝」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentOutcome {
    Success,
    /// 连不上任何 agent（OpenSSH Agent / Pageant）。
    NoAgent,
    /// agent 可用，但里面没有身份。
    NoIdentities,
    /// 尝试了 N 个身份，全部被服务器拒绝。
    Rejected(usize),
}

async fn auth_with_agent(
    handle: &mut client::Handle<ClientHandler>,
    user: &str,
    limit: usize,
) -> AgentOutcome {
    let Some(mut agent) = connect_agent().await else {
        return AgentOutcome::NoAgent;
    };
    let identities = match agent.request_identities().await {
        Ok(i) => i,
        Err(e) => {
            applog!("[auth] ssh-agent 列出身份失败，跳过: {e}");
            return AgentOutcome::NoIdentities;
        }
    };
    applog!(
        "[auth] ssh-agent 提供 {} 个身份，最多尝试 {limit} 个",
        identities.len()
    );
    if identities.is_empty() {
        return AgentOutcome::NoIdentities;
    }
    // RSA 身份必须用服务器接受的 SHA2 签名算法。russh 的 hash_alg=None 对 RSA
    // 映射成旧 ssh-rsa(SHA-1)，而 OpenSSH ≥8.8 默认禁用该算法——agent 里的 RSA 密钥
    // 会全部被拒。best_supported_rsa_hash 读取 server-sig-algs：老服务器只支持 SHA-1
    // 时返回 Some(None)（沿用兼容），支持 SHA2 时返回具体算法。
    let rsa_hash = handle
        .best_supported_rsa_hash()
        .await
        .ok()
        .flatten()
        .flatten();
    // 每次失败都会消耗服务器的 MaxAuthTries 配额，因此严格限次。
    let mut tried = 0usize;
    for key in identities.into_iter().take(limit) {
        tried += 1;
        let public_key = key.public_key().into_owned();
        let hash_alg = if matches!(public_key.algorithm(), russh::keys::Algorithm::Rsa { .. }) {
            rsa_hash
        } else {
            None
        };
        match handle
            .authenticate_publickey_with(user, public_key, hash_alg, &mut agent)
            .await
        {
            Ok(result) if result.success() => return AgentOutcome::Success,
            Ok(_) => continue,
            Err(e) => {
                // 单个身份签名失败不终止整个 agent 分支，继续尝试下一个。
                applog!("[auth] agent 身份认证请求失败，继续: {e}");
                continue;
            }
        }
    }
    AgentOutcome::Rejected(tried)
}

async fn auth_with_key_ref(
    handle: &mut client::Handle<ClientHandler>,
    user: &str,
    key_ref: &str,
) -> AppResult<bool> {
    let path = expand_tilde(key_ref);
    if !std::path::Path::new(&path).exists() {
        return Err(AppError::ESshAuth(format!(
            "私钥文件不存在：{path}（配置值：{key_ref}）"
        )));
    }
    let key = russh::keys::load_secret_key(&path, None).map_err(|e| {
        AppError::ESshAuth(format!(
            "读取私钥 {path} 失败：{e}（若私钥有口令，当前版本不支持在客户端输入，请先用 ssh-add 或去掉口令）"
        ))
    })?;
    applog!(
        "[auth] 私钥加载成功: algo={:?} path={path}",
        key.algorithm()
    );
    // RSA 密钥必须显式指定 SHA2 签名算法：默认 None 会用 ssh-rsa（SHA1），
    // OpenSSH ≥ 8.8 服务端默认禁用 SHA1，签名会被拒绝。
    // 候选只有 SHA2——不回退 None(SHA-1)。老服务器需要兼容时由管理员在
    // 服务端开启 rsa-sha2（OpenSSH 7.2+ 均支持），客户端不主动降级签名算法。
    // 私钥只加载一次，候选间复用同一个 Arc（不逐候选 clone 私钥本体）。
    let key = Arc::new(key);
    let is_rsa = matches!(key.algorithm(), russh::keys::Algorithm::Rsa { .. });
    let candidates: Vec<Option<russh::keys::HashAlg>> = if is_rsa {
        vec![
            Some(russh::keys::HashAlg::Sha256),
            Some(russh::keys::HashAlg::Sha512),
        ]
    } else {
        vec![None]
    };
    for hash in candidates {
        let with_hash = PrivateKeyWithHashAlg::new(Arc::clone(&key), hash);
        let result = handle
            .authenticate_publickey(user, with_hash)
            .await
            .map_err(|e| AppError::ESshAuth(format!("私钥认证请求失败：{e}")))?;
        applog!("[auth] 公钥认证 hash={hash:?} success={}", result.success());
        if result.success() {
            return Ok(true);
        }
    }
    Err(AppError::ESshAuth(format!(
        "服务器拒绝了私钥 {path}（该公钥可能不在远端 ~/.ssh/authorized_keys 中，或用户 {} 无权登录）",
        user
    )))
}

/// `key` 分支：只认 `key_ref`，缺路径 / 文件不存在都直接报明确原因。
async fn authenticate_with_key(
    handle: &mut client::Handle<ClientHandler>,
    host: &Host,
) -> AppResult<()> {
    if host.key_ref.trim().is_empty() {
        return Err(AppError::ESshAuth(format!(
            "未配置私钥路径：主机 {} 使用私钥认证，但 keyRef 为空",
            host.name
        )));
    }
    match auth_with_key_ref(handle, &host.user, &host.key_ref).await? {
        true => Ok(()),
        false => Err(AppError::ESshAuth(format!(
            "服务器拒绝了私钥 {}",
            host.key_ref
        ))),
    }
}

/// `password` 分支：keyring 密码，或本次连接传入的一次性密码（不落盘）。
async fn authenticate_with_password_method(
    handle: &mut client::Handle<ClientHandler>,
    host: &Host,
    ephemeral_password: Option<&str>,
) -> AppResult<()> {
    // 密码一律装进 Zeroizing——drop 时擦除堆内存，减少页面文件/内存转储残留。
    // keyring 是同步阻塞 API（凭据管理器可能因漫游/凭据提供程序卡住），
    // 必须放阻塞线程池；否则认证超时与取消令牌在阻塞期间都无法生效。
    let password: Zeroizing<String> = match ephemeral_password.filter(|p| !p.is_empty()) {
        Some(p) => Zeroizing::new(p.to_string()),
        None => {
            let host_id = host.id.clone();
            let stored = tokio::task::spawn_blocking(move || secret::get_password(&host_id))
                .await
                .map_err(|e| AppError::ESecret(format!("钥匙串任务执行失败: {e}")))?;
            match stored {
                Ok(Some(p)) if !p.is_empty() => Zeroizing::new(p),
                Ok(_) => {
                    return Err(AppError::ESshAuth(format!(
                    "未保存密码：主机 {} 使用密码认证，但系统凭据管理器中没有密码，请先输入密码",
                    host.name
                )))
                }
                Err(e) => {
                    return Err(AppError::ESshAuth(format!(
                        "未保存密码（系统凭据管理器不可用：{}）",
                        error_text(&e)
                    )))
                }
            }
        }
    };
    // 只把密码交给 russh；不打印、不拼进任何错误详情。
    let result = handle
        .authenticate_password(&host.user, password.as_str())
        .await
        .map_err(|e| AppError::ESshAuth(format!("密码认证请求失败: {e}")))?;
    if result.success() {
        Ok(())
    } else {
        Err(AppError::ESshAuth(format!(
            "密码被拒绝：服务器拒绝了用户 {} 的密码（密码可能已更改）",
            host.user
        )))
    }
}

/// `agent` 分支：只试 Windows OpenSSH Agent（限次）。
/// 不回退 `key_ref`——`normalize_auth` 对 auth!=key 清空 keyRef，
/// 该回退在生产配置下不可达（auth=agent 的主机 keyRef 恒为空），保留只会产生
/// “会尝试回退”的错误文案误导排查。
/// 「没有可用凭据」与「凭据被服务器拒绝」必须给出不同文案。
async fn authenticate_with_agent_method(
    handle: &mut client::Handle<ClientHandler>,
    host: &Host,
) -> AppResult<()> {
    let outcome = auth_with_agent(handle, &host.user, AGENT_IDENTITY_LIMIT).await;
    if outcome == AgentOutcome::Success {
        return Ok(());
    }
    let note = match outcome {
        AgentOutcome::Success => unreachable!(),
        AgentOutcome::NoAgent => {
            "未检测到可用的 SSH Agent（Windows OpenSSH Agent / Pageant 均不可用）".to_string()
        }
        AgentOutcome::NoIdentities => "SSH Agent 中没有可用的身份".to_string(),
        AgentOutcome::Rejected(n) => format!("SSH Agent 的 {n} 个身份均被服务器拒绝"),
    };
    Err(AppError::ESshAuth(format!("未找到可用凭据：{note}")))
}

/// 按 `host.auth` 严格执行认证链。
pub async fn authenticate(
    handle: &mut client::Handle<ClientHandler>,
    host: &Host,
) -> AppResult<()> {
    authenticate_with_password(handle, host, None).await
}

/// 同上，`ephemeral_password` 仅用于本次连接（用户选择「不保存」），
/// 绝不落盘、绝不写日志；`auth != password` 时忽略。
pub async fn authenticate_with_password(
    handle: &mut client::Handle<ClientHandler>,
    host: &Host,
    ephemeral_password: Option<&str>,
) -> AppResult<()> {
    applog!("[auth] 主机 {} 认证方式: {:?}", host.id, host.auth);
    match host.auth {
        AuthMethod::Key => authenticate_with_key(handle, host).await,
        AuthMethod::Password => {
            authenticate_with_password_method(handle, host, ephemeral_password).await
        }
        AuthMethod::Agent => authenticate_with_agent_method(handle, host).await,
    }
}

/// 取出 AppError 的人话信息（去掉枚举名包装）。
fn error_text(e: &AppError) -> String {
    match e {
        AppError::ESshAuth(m) => m.clone(),
        other => other.to_string(),
    }
}
