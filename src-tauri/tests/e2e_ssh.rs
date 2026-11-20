//! 真实服务器 E2E 全链路：连接 → 指纹挑战/信任 → 认证 → PTY/shell →
//! 终端输入 → 编码切换 → /proc 指标采样 → SFTP 复用 → 会话关闭。
//!
//! SSH 核心已与 tauri 解耦，本测试不需要 WebView/AppHandle。
//! 运行方式（密码经环境变量注入，不落盘）：
//!   $env:RUTHENIUM_E2E_KEY='~\Documents\id_rsa'
//!   $env:RUTHENIUM_E2E_TILDE=1
//!   cargo test --test e2e_ssh -- --nocapture
//! 未设置任何凭据时本测试自动跳过。
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;

use ruthenium_lib::encoding::SessionEncoding;
use ruthenium_lib::metrics::sampler;
use ruthenium_lib::models::{AuthMethod, Host, SessionKind};
use ruthenium_lib::secret;
use ruthenium_lib::session_log::{LogStamp, SessionLogger};
use ruthenium_lib::ssh::client::ClientHandler;
use ruthenium_lib::ssh::known_hosts;
use ruthenium_lib::ssh::{
    auth,
    channel::{open_term_channel, send_init_command, ChannelCmd, TermChannelArgs},
    make_session_handle, new_session_id, LiveSession, SshManager,
};
use ruthenium_lib::state::PendingHostKeyMap;

const HOST_ID: &str = "e2e-ruthenium-test";
const PASSWORD_HOST_ID: &str = "e2e-ruthenium-password";
const MARKER_FILE: &str = "/tmp/ruthenium_e2e_marker";

/// 与生产同一份客户端配置（keepalive 30s / max 3，不用 inactivity_timeout）。
fn ssh_client_config() -> Arc<russh::client::Config> {
    ruthenium_lib::ssh::client_config(30)
}

/// E2E 服务器地址与用户：必须经环境变量注入，不内置任何默认 IP / 账号。
/// 缺失时调用方按 skip 机制提前返回。
fn e2e_server() -> (String, String) {
    let addr = std::env::var("RUTHENIUM_E2E_HOST").unwrap_or_default();
    let user = std::env::var("RUTHENIUM_E2E_USER").unwrap_or_default();
    (addr, user)
}

/// 服务器地址 / 用户是否已通过环境变量提供。
fn has_e2e_server() -> bool {
    std::env::var("RUTHENIUM_E2E_HOST").is_ok() && std::env::var("RUTHENIUM_E2E_USER").is_ok()
}

fn has_e2e_credentials() -> bool {
    std::env::var("RUTHENIUM_E2E_KEY").is_ok() || std::env::var("RUTHENIUM_E2E_PASSWORD").is_ok()
}

/// 连接 + 自动信任指纹（模拟前端 HostKeyModal「信任并连接」）。
/// 返回（新 sessionId, 已完成握手、尚未认证的 handle）。
async fn connect_with_autotrust(host: &Host) -> (String, russh::client::Handle<ClientHandler>) {
    let pending: PendingHostKeyMap = Arc::new(Mutex::new(HashMap::new()));
    let session_id = new_session_id();
    // 指纹挑战自动信任器：12s 足够覆盖首次连接；known_hosts 已有记录时不会出现挑战。
    let watcher_sid = session_id.clone();
    let watcher_pending = pending.clone();
    let watcher = tokio::spawn(async move {
        for _ in 0..60 {
            let pk = watcher_pending.lock().await.remove(&watcher_sid);
            if let Some(p) = pk {
                eprintln!(
                    "[e2e] hostkey challenge: {} {} => trust",
                    p.host, p.fingerprint
                );
                known_hosts::replace_host_entry(&p.host, p.port, &p.public_key)
                    .expect("写 known_hosts 失败");
                let _ = p.decision_tx.send(true);
                return;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        eprintln!("[e2e] 指纹挑战未出现（可能 known_hosts 已有记录）");
    });

    // 瞬时网络抖动重试：公网测试机的 KEX 阶段偶发 ConnectionReset（服务器侧无对应
    // 记录）。连接失败重试一次，避免“环境噪声”伪装成用例失败。
    let mut connected = None;
    for attempt in 1..=2u32 {
        match russh::client::connect(
            ssh_client_config(),
            (host.addr.as_str(), host.port),
            ClientHandler {
                session_id: session_id.clone(),
                host_id: host.id.clone(),
                host: host.addr.clone(),
                port: host.port,
                pending: pending.clone(),
                app: None,
                pinned: None, // E2E：不做钉扎
                pinned_mismatch: None,
            },
        )
        .await
        {
            Ok(h) => {
                connected = Some(h);
                break;
            }
            Err(e) => {
                eprintln!("[e2e] SSH 连接失败（第 {attempt} 次）: {e}");
                if attempt == 2 {
                    panic!("SSH 连接失败（已重试一次）: {e}");
                }
                tokio::time::sleep(Duration::from_millis(1500)).await;
            }
        }
    }
    let handle = connected.expect("成功分支必然设置");
    // 握手返回时挑战已经裁决（或本就没有挑战），收掉 watcher 即可。
    watcher.abort();
    (session_id, handle)
}

/// 复用会话连接新开 channel 执行命令，收集 stdout。
async fn exec_on(live: &LiveSession, cmd: &str) -> String {
    let handle = live.russh.lock().await;
    let mut channel = handle.channel_open_session().await.expect("exec 通道失败");
    channel.exec(false, cmd).await.expect("exec 失败");
    let mut out = Vec::new();
    while let Some(msg) = channel.wait().await {
        match msg {
            russh::ChannelMsg::Data { data } => out.extend_from_slice(&data),
            russh::ChannelMsg::Eof | russh::ChannelMsg::Close => break,
            _ => {}
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_real_ssh_full_flow() {
    let key_path = std::env::var("RUTHENIUM_E2E_KEY").ok();
    let password = std::env::var("RUTHENIUM_E2E_PASSWORD").ok();
    if key_path.is_none() && password.is_none() {
        eprintln!("[e2e] 未设置 RUTHENIUM_E2E_KEY / RUTHENIUM_E2E_PASSWORD，跳过真实服务器验证");
        return;
    }
    if !has_e2e_server() {
        eprintln!("[e2e] 未设置 RUTHENIUM_E2E_HOST / RUTHENIUM_E2E_USER，跳过真实服务器验证");
        return;
    }
    let (addr, user) = e2e_server();

    // 认证材料：优先私钥（keyRef 链），否则密码 → Windows 凭据管理器（测试结束删除）
    // RUTHENIUM_E2E_TILDE=1：保持 `~\Documents\id_rsa` 相对写法，验证 expand_tilde
    // RUTHENIUM_E2E_AGENT=1：auth=agent（先试 Windows OpenSSH Agent，失败回退 keyRef）
    let keep_tilde = std::env::var("RUTHENIUM_E2E_TILDE").as_deref() == Ok("1");
    let use_agent = std::env::var("RUTHENIUM_E2E_AGENT").as_deref() == Ok("1");
    let key_path = if keep_tilde {
        key_path
    } else {
        key_path.map(|p| {
            let full = std::fs::canonicalize(&p).unwrap_or_else(|_| p.into());
            let s = full.to_string_lossy().to_string();
            s.strip_prefix(r"\\?\").map(|x| x.to_string()).unwrap_or(s)
        })
    };
    let auth = match (&key_path, use_agent) {
        (Some(_), true) => AuthMethod::Agent,
        (Some(_), false) => AuthMethod::Key,
        (None, _) => AuthMethod::Password,
    };
    if let Some(p) = &key_path {
        if !keep_tilde {
            assert!(std::path::Path::new(p).exists(), "私钥不存在: {p}");
        }
        eprintln!("[e2e] 认证方式：{auth:?}，私钥 {p} (agent={use_agent})");
        // 私钥 / agent 模式清掉历史密码条目，避免认证链混入密码尝试
        let _ = secret::delete_password(HOST_ID);
    } else {
        secret::set_password(HOST_ID, password.as_ref().unwrap()).expect("写入凭据管理器失败");
    }

    let host = Host {
        id: HOST_ID.into(),
        name: "e2e-test".into(),
        addr: addr.clone(),
        user: user.clone(),
        port: 22,
        group: "测试".into(),
        tag: "dev".into(),
        auth,
        key_ref: key_path.clone().unwrap_or_default(),
        known: false,
        agent: false,
        note: "E2E".into(),
        log: false,
        init_command: String::new(),
    };

    // 1) TCP + 握手（含指纹挑战挂起）+ 按 auth 认证
    let (watcher_sid, mut handle) = connect_with_autotrust(&host).await;
    auth::authenticate(&mut handle, &host)
        .await
        .expect("认证失败");
    eprintln!("[e2e] 握手 + 指纹 + 认证 OK（{auth:?}）");

    // 2) PTY + shell + reader/writer
    let (sh, mut writer_rx) =
        make_session_handle(watcher_sid.clone(), HOST_ID.into(), SessionKind::Term);
    let ctrl = open_term_channel(
        &mut handle,
        TermChannelArgs {
            session_id: watcher_sid.clone(),
            cols: 120,
            rows: 32,
            app: None,
            data_channel: None, // Z3：E2E 无 webview，回退事件路径（app=None 即静默）
            cancel: sh.cancel.clone(),
            encoding: sh.encoding.clone(),
            logger: None, // E2E：本次不记录日志
            wait_replies: false,
            wait_cancel: None,
        },
    )
    .await
    .expect("PTY/shell 建立失败");
    let writer = sh.writer.clone();
    let live = Arc::new(LiveSession {
        inner: sh,
        connected: Arc::new(AtomicBool::new(true)),
        russh: Arc::new(Mutex::new(handle)),
        ctrl: ctrl.clone(),
        owns_connection: true,
        addr: host.addr.clone(),
        port: host.port,
        user: host.user.clone(),
        cols: std::sync::atomic::AtomicU16::new(120),
        rows: std::sync::atomic::AtomicU16::new(32),
    });
    assert!(live.connected.load(Ordering::Relaxed));

    // writer 泵：SessionHandle.writer → channel
    let pump_ctrl = ctrl.clone();
    let pump_cancel = live.inner.cancel.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = pump_cancel.cancelled() => break,
                data = writer_rx.recv() => match data {
                    Some(data) => { let _ = pump_ctrl.send(ChannelCmd::Data(data)).await; }
                    None => break,
                },
            }
        }
    });

    // 3) 终端输入链路：PTY 写入 → 远端执行 → 另开 exec 通道验证落盘
    let _ = exec_on(&live, &format!("rm -f {MARKER_FILE}")).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    writer
        .send(format!("echo E2E_MARK > {MARKER_FILE}\n").into_bytes())
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if tokio::time::Instant::now() >= deadline {
            panic!("15s 内未在远端看到 PTY 写入的标记文件（终端输入链路失败）");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
        let out = exec_on(&live, &format!("cat {MARKER_FILE} 2>/dev/null")).await;
        if out.contains("E2E_MARK") {
            break;
        }
    }
    eprintln!("[e2e] PTY/shell 输入链路 OK");

    // 4) 编码切换（会话级）：GB18030 只影响此后输出
    *live.inner.encoding.lock().await = SessionEncoding::new("GB18030").expect("GB18030 应存在");
    eprintln!(
        "[e2e] 编码切换 OK: {}",
        live.inner.encoding.lock().await.name
    );

    // 5) 指标采样：复用会话连接读 /proc（真实 Ubuntu）
    let metrics_text = exec_on(&live, sampler::METRICS_CMD).await;
    let tty_probe = exec_on(&live, "cat /proc/net/dev | awk '{print NR\": \"NF}'; echo ---; cat /proc/diskstats | wc -l; echo ---; df -B1 / | cat -A | head -3; echo ---; cat /proc/loadavg").await;
    eprintln!("[e2e] tty probe:\n{}", tty_probe);
    let raw = match sampler::parse_metrics_output(&metrics_text) {
        Some(r) => r,
        None => {
            std::fs::write("failed_metrics.txt", &metrics_text).ok();
            panic!("解析失败，已写出 failed_metrics.txt");
        }
    };
    let metrics = sampler::diff_to_metrics(&watcher_sid, 0, &raw, None);
    assert!(metrics.cores >= 1, "核数应 >= 1，实际 {}", metrics.cores);
    assert!(
        metrics.mem_total_gb > 0.5,
        "内存总量应 > 0.5GB，实际 {}",
        metrics.mem_total_gb
    );
    eprintln!(
        "[e2e] 指标采样 OK: cores={} mem={:.1}/{:.1}GB disk={:.1}/{:.1}GB",
        metrics.cores,
        metrics.mem_used_gb,
        metrics.mem_total_gb,
        metrics.disk_used_gb,
        metrics.disk_total_gb
    );

    // 6) SFTP 复用终端连接：不重新握手 / 认证，关闭复用会话后 term 仍在
    let ssh = SshManager::new();
    ssh.insert(live.clone()).await;
    let sftp_id = ssh
        .reuse_sftp_session(&host)
        .await
        .expect("已有活动终端会话时应复用连接");
    let sessions = ssh.list().await;
    assert_eq!(sessions.len(), 2, "应同时存在 term 与 sftp 两条会话");
    assert!(sessions
        .iter()
        .any(|s| s.id == watcher_sid && s.kind == SessionKind::Term));
    assert!(sessions
        .iter()
        .any(|s| s.id == sftp_id && s.kind == SessionKind::Sftp));
    let term_live = ssh.get(&watcher_sid).await.unwrap();
    let sftp_live = ssh.get(&sftp_id).await.unwrap();
    assert!(
        Arc::ptr_eq(&term_live.russh, &sftp_live.russh),
        "SFTP 会话必须持有同一个 SSH 连接（不得重新握手）"
    );
    assert!(!sftp_live.owns_connection, "复用会话不拥有底层连接");
    // 在复用连接上开 subsystem channel：若重新建连/认证，这条路径会失败。
    let stream = ssh
        .open_subsystem_channel(&sftp_id)
        .await
        .expect("复用连接打开 SFTP 通道失败");
    drop(stream);
    ssh.close(&sftp_id).await.expect("关闭复用会话失败");
    assert!(
        ssh.has(&watcher_sid).await,
        "关闭复用会话后 term 会话必须仍在"
    );
    let out = exec_on(&live, "echo E2E_SFTP_REUSE_OK").await;
    assert!(
        out.contains("E2E_SFTP_REUSE_OK"),
        "复用会话关闭后连接应仍可用: {out}"
    );
    eprintln!("[e2e] SFTP 复用 + owns_connection 关闭语义 OK");

    // 7) 关闭会话：cancel token + 通道关闭
    live.inner.cancel.cancel();
    live.connected.store(false, Ordering::Relaxed);
    eprintln!("[e2e] 会话关闭 OK");

    // 清理：远端标记文件 +（密码模式）凭据管理器测试条目
    let _ = exec_on(&live, &format!("rm -f {MARKER_FILE}")).await;
    if key_path.is_none() {
        let _ = secret::delete_password(HOST_ID);
    }
    eprintln!("[e2e] 全链路验证通过 ✓");
}

/// 诊断可读性：私钥路径写错时，错误详情必须直接说明「文件不存在」，
/// 而不是笼统的「全部认证方式失败」（GUI 没有控制台，日志看不到）。
#[tokio::test(flavor = "multi_thread")]
async fn e2e_missing_key_reports_reason() {
    if !has_e2e_credentials() {
        eprintln!("[e2e] 未设置凭据，跳过");
        return;
    }
    if !has_e2e_server() {
        eprintln!("[e2e] 未设置 RUTHENIUM_E2E_HOST / RUTHENIUM_E2E_USER，跳过");
        return;
    }
    let (addr, user) = e2e_server();

    let host = Host {
        id: "e2e-missing-key".into(),
        name: "missing-key".into(),
        addr: addr.clone(),
        user: user.clone(),
        port: 22,
        group: "测试".into(),
        tag: "dev".into(),
        auth: AuthMethod::Key,
        key_ref: "~\\nope\\definitely_missing".into(),
        known: true,
        agent: false,
        note: String::new(),
        log: false,
        init_command: String::new(),
    };

    let (_sid, mut handle) = connect_with_autotrust(&host).await;
    let err = auth::authenticate(&mut handle, &host)
        .await
        .expect_err("应返回认证错误");
    let text = format!("{err:?}");
    assert!(
        text.contains("私钥文件不存在"),
        "错误详情未包含具体原因: {text}"
    );
    assert!(
        text.contains("definitely_missing"),
        "错误详情必须带完整（展开后的）路径: {text}"
    );
    eprintln!("[e2e] 缺失私钥的诊断信息：{text}");
}

/// password 认证分支：
/// 1) keyring 无密码 → 必须报「未保存密码」（前端据此弹密码框），不能报缺私钥；
/// 2) 写入密码后再连 → 服务器已禁用密码登录，允许报「密码被拒绝」，
///    但必须证明走的是 password 分支，且错误详情绝不泄露密码。
#[tokio::test(flavor = "multi_thread")]
async fn e2e_password_auth_branch_missing_then_rejected() {
    if !has_e2e_credentials() {
        eprintln!("[e2e] 未设置凭据，跳过");
        return;
    }
    if !has_e2e_server() {
        eprintln!("[e2e] 未设置 RUTHENIUM_E2E_HOST / RUTHENIUM_E2E_USER，跳过");
        return;
    }
    let (addr, user) = e2e_server();
    let host = Host {
        id: PASSWORD_HOST_ID.into(),
        name: "e2e-password".into(),
        addr,
        user,
        port: 22,
        group: "测试".into(),
        tag: "dev".into(),
        auth: AuthMethod::Password,
        key_ref: String::new(),
        known: true,
        agent: false,
        note: "E2E password".into(),
        log: false,
        init_command: String::new(),
    };

    // 1) 无保存密码
    let _ = secret::delete_password(PASSWORD_HOST_ID);
    let (_sid, mut handle) = connect_with_autotrust(&host).await;
    let err = auth::authenticate(&mut handle, &host)
        .await
        .expect_err("无保存密码时必须失败并提示输入");
    let text = format!("{err:?}");
    assert!(text.contains("未保存密码"), "应提示未保存密码: {text}");
    assert!(
        !text.contains("私钥文件不存在"),
        "password 分支不得去读私钥: {text}"
    );
    drop(handle);

    // 2) 写入一个必然错误的密码：走 password 分支，被服务器拒绝
    let secret_password = "ruthenium-e2e-not-a-real-password";
    secret::set_password(PASSWORD_HOST_ID, secret_password).expect("写入凭据管理器失败");
    let (_sid2, mut handle2) = connect_with_autotrust(&host).await;
    let err2 = auth::authenticate(&mut handle2, &host)
        .await
        .expect_err("错误密码必须认证失败");
    let text2 = format!("{err2:?}");
    assert!(
        text2.contains("密码被拒绝") || text2.contains("密码认证请求失败"),
        "应走 password 分支并报告密码失败: {text2}"
    );
    assert!(
        !text2.contains("未保存密码"),
        "已保存密码时不应再报未保存: {text2}"
    );
    assert!(
        !text2.contains("私钥"),
        "password 分支不得尝试私钥: {text2}"
    );
    assert!(
        !text2.contains(secret_password),
        "错误详情绝不能泄露密码: {text2}"
    );
    let _ = secret::delete_password(PASSWORD_HOST_ID);
    eprintln!("[e2e] password 分支诊断 OK：{text2}");
}

// ---------------- 会话日志 / initCommand / keepalive ----------------

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// 轮询会话日志文件（写入端 `BufWriter` 每 2s 或 64 KiB flush 一次）。
async fn wait_for_log_bytes(path: &std::path::Path, needle: &[u8], timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Ok(bytes) = std::fs::read(path) {
            if contains_subslice(&bytes, needle) {
                return true;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            if let Ok(bytes) = std::fs::read(path) {
                let tail = &bytes[bytes.len().saturating_sub(600)..];
                eprintln!(
                    "[e2e] 等待日志字节超时，日志尾部: {:?}",
                    String::from_utf8_lossy(tail)
                );
            }
            return false;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// 与主链路一致地准备认证材料：优先私钥（`keyRef`，支持 `~\...` 写法与 agent），
/// 否则把密码写入 keyring。返回 (auth, keyRef)。
fn e2e_auth_material(host_id: &str) -> Option<(AuthMethod, String)> {
    if let Ok(key) = std::env::var("RUTHENIUM_E2E_KEY") {
        let _ = secret::delete_password(host_id);
        let use_agent = std::env::var("RUTHENIUM_E2E_AGENT").as_deref() == Ok("1");
        return Some((
            if use_agent {
                AuthMethod::Agent
            } else {
                AuthMethod::Key
            },
            key,
        ));
    }
    if let Ok(password) = std::env::var("RUTHENIUM_E2E_PASSWORD") {
        secret::set_password(host_id, &password).ok()?;
        return Some((AuthMethod::Password, String::new()));
    }
    None
}

/// 会话日志 / initCommand / keepalive（真实服务器）：
/// - `host.log = true` 的连接产生日志文件，内容包含本次输出且保留**解码前**的原始字节
///   （`0xff 0xfe` 若被解码会变成 U+FFFD，只有 tee 原始字节才会原样出现）；
/// - `initCommand = "echo RUTHENIUM_INIT_OK"` 在 shell 就绪后自动执行，输出进入终端流；
/// - keepalive 30s / max 3 的客户端配置仍能正常握手 + 认证 + 开 PTY。
#[tokio::test(flavor = "multi_thread")]
async fn e2e_session_log_raw_bytes_and_init_command() {
    let host_id = "e2e-ruthenium-log";
    let Some((auth_method, key_ref)) = e2e_auth_material(host_id) else {
        eprintln!("[e2e] 未设置凭据，跳过");
        return;
    };
    if !has_e2e_server() {
        eprintln!("[e2e] 未设置 RUTHENIUM_E2E_HOST / RUTHENIUM_E2E_USER，跳过");
        return;
    }
    let (addr, user) = e2e_server();
    let host = Host {
        id: host_id.into(),
        name: "e2e-log".into(),
        addr,
        user,
        port: 22,
        group: "测试".into(),
        tag: "dev".into(),
        auth: auth_method,
        key_ref,
        known: false,
        agent: false,
        note: "E2E log".into(),
        log: true,
        init_command: "echo RUTHENIUM_INIT_OK".into(),
    };

    // 覆盖 keepalive 打开时的连接路径（空闲不断连的 10 分钟断言不在此处）。
    let cfg = ssh_client_config();
    assert_eq!(
        cfg.keepalive_interval,
        Some(Duration::from_secs(30)),
        "E2E 必须使用 keepalive 配置"
    );
    assert_eq!(cfg.keepalive_max, 3);
    assert!(cfg.inactivity_timeout.is_none());

    let (session_id, mut handle) = connect_with_autotrust(&host).await;
    auth::authenticate(&mut handle, &host)
        .await
        .expect("认证失败");
    eprintln!("[e2e] keepalive 配置连接 + 认证 OK（{auth_method:?}）");

    // 日志目录用临时目录（生产路径 `<config_dir>\logs`，这里避免污染真实配置目录）。
    let log_dir = std::env::temp_dir().join(format!("ruthenium-e2e-logs-{}", uuid::Uuid::new_v4()));
    let logger =
        SessionLogger::create(&log_dir, &host.name, LogStamp::now()).expect("创建会话日志失败");
    let log_path = logger.path().to_path_buf();
    let file_name = log_path.file_name().unwrap().to_string_lossy().to_string();
    assert!(
        file_name.starts_with("e2e-log-") && file_name.ends_with(".log"),
        "日志文件名格式不符: {file_name}"
    );
    eprintln!("[e2e] 日志文件: {}", log_path.display());

    let (sh, mut writer_rx) =
        make_session_handle(session_id.clone(), host_id.into(), SessionKind::Term);
    let ctrl = open_term_channel(
        &mut handle,
        TermChannelArgs {
            session_id: session_id.clone(),
            cols: 120,
            rows: 32,
            app: None,
            data_channel: None, // Z3：E2E 无 webview，回退事件路径（app=None 即静默）
            cancel: sh.cancel.clone(),
            encoding: sh.encoding.clone(),
            logger: Some(logger),
            wait_replies: false,
            wait_cancel: None,
        },
    )
    .await
    .expect("PTY/shell 建立失败");

    // writer 泵（与生产 connect_inner 一致）
    let pump_ctrl = ctrl.clone();
    let pump_cancel = sh.cancel.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = pump_cancel.cancelled() => break,
                data = writer_rx.recv() => match data {
                    Some(data) => {
                        if pump_ctrl.send(ChannelCmd::Data(data)).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                }
            }
        }
    });

    // 与生产完全相同的写入时机与内容（PTY/shell 就绪后、返回前）。
    assert!(!host.init_command.is_empty());
    send_init_command(&ctrl, &host.init_command)
        .await
        .expect("写入 initCommand 失败");
    assert!(
        wait_for_log_bytes(&log_path, b"RUTHENIUM_INIT_OK", Duration::from_secs(15)).await,
        "15s 内日志里未出现 initCommand 的执行输出"
    );
    eprintln!("[e2e] initCommand 自动执行 OK");

    // 若干行输出 + ANSI 序列 + 非 UTF-8 原始字节（证明记录的是解码前字节）
    for i in 0..3 {
        ctrl.send(ChannelCmd::Data(
            format!("echo RUTHENIUM_LOG_LINE_{i}\n").into_bytes(),
        ))
        .await
        .unwrap();
    }
    ctrl.send(ChannelCmd::Data(
        b"printf '\\033[31mRUTHENIUM_RAW_ANSI\\033[0m\\n'\n".to_vec(),
    ))
    .await
    .unwrap();
    ctrl.send(ChannelCmd::Data(b"printf '\\377\\376'\n".to_vec()))
        .await
        .unwrap();

    assert!(
        wait_for_log_bytes(
            &log_path,
            b"\x1b[31mRUTHENIUM_RAW_ANSI",
            Duration::from_secs(15)
        )
        .await,
        "日志里未出现 ANSI 转义序列（会话日志必须原样记录）"
    );
    assert!(
        wait_for_log_bytes(&log_path, &[0xff, 0xfe], Duration::from_secs(15)).await,
        "日志里未出现 0xff 0xfe 原始字节：说明记录的是解码后文本而不是远端原始字节流"
    );

    let bytes = std::fs::read(&log_path).expect("读取会话日志失败");
    for i in 0..3 {
        let needle = format!("RUTHENIUM_LOG_LINE_{i}");
        assert!(
            contains_subslice(&bytes, needle.as_bytes()),
            "日志缺少刚才的输出: {needle}"
        );
    }
    assert!(
        contains_subslice(&bytes, b"\x1b["),
        "日志必须保留 ANSI 序列"
    );
    eprintln!("[e2e] 会话日志原始字节 OK：{} 字节", bytes.len());

    // 主动关闭：cancel → reader task 退出 → 日志 flush 并关闭。
    sh.cancel.cancel();
    tokio::time::sleep(Duration::from_millis(800)).await;
    let final_bytes = std::fs::read(&log_path).expect("读取最终日志失败");
    assert!(
        contains_subslice(&final_bytes, b"RUTHENIUM_INIT_OK"),
        "关闭会话后日志内容丢失"
    );
    assert!(
        contains_subslice(&final_bytes, &[0xff, 0xfe]),
        "关闭会话后原始字节丢失"
    );
    assert_eq!(
        std::fs::read_dir(&log_dir).unwrap().count(),
        1,
        "logs 目录应只有本次会话的日志文件（不得覆盖/追加旧日志）"
    );

    drop(ctrl);
    drop(handle);
    let _ = std::fs::remove_dir_all(&log_dir);
    if auth_method == AuthMethod::Password {
        let _ = secret::delete_password(host_id);
    }
    eprintln!("[e2e] 会话日志 + initCommand + keepalive 验证通过 ✓");
}
