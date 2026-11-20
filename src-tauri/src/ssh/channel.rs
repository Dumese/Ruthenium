//! PTY + shell + reader task（emit term://data / conn://state）+ writer。
use russh::client;
use russh::{ChannelId, ChannelMsg, ChannelWriteHalf};
use tauri::AppHandle;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::encoding::{decode_chunk, SessionEncoding};
use crate::error::{AppError, AppResult};
use crate::events::{self, ConnStatePayload, TermDataPayload, TermExitPayload};
use crate::session_log::SessionLogger;
use crate::ssh::client::ClientHandler;

/// 终端通道写半端的类型别名（reader 任务只拿读半端）。
pub type ClientWriteHalf = ChannelWriteHalf<(ChannelId, ChannelMsg)>;

/// 写入命令：数据或窗口变更。
pub enum ChannelCmd {
    Data(Vec<u8>),
    Resize(u32, u32),
}

/// 连接后自动执行一行命令（PTY/shell 就绪、`connect` 返回前调用）。
///
/// 空串不发；只发一条，不做提示符检测、不做多命令队列。写入的是 shell 的标准输入，
/// 命令会在 shell 初始化完成后排队执行（`cd` 类命令生效），并出现在终端 scrollback 里。
pub async fn send_init_command(
    ctrl: &mpsc::Sender<ChannelCmd>,
    init_command: &str,
) -> AppResult<()> {
    if init_command.is_empty() {
        return Ok(());
    }
    let mut bytes = Vec::with_capacity(init_command.len() + 1);
    bytes.extend_from_slice(init_command.as_bytes());
    bytes.push(b'\n');
    ctrl.send(ChannelCmd::Data(bytes))
        .await
        .map_err(|_| AppError::ESshChannel("写入初始化命令失败：会话通道已关闭".to_string()))
}

/// 打开终端通道所需的会话上下文：PTY 尺寸、事件出口、取消令牌、编码与日志。
pub struct TermChannelArgs {
    pub session_id: String,
    pub cols: u16,
    pub rows: u16,
    pub app: Option<AppHandle>,
    /// 终端数据通道（官方推荐的流式 IPC：有序、按调用方隔离、无全局分发开销）。
    /// `Some` 时数据帧走 channel；`None` 回退全局 `term://data` 事件
    /// （E2E / 测试 / 旧调用方兼容路径）。
    pub data_channel: Option<tauri::ipc::Channel<TermDataPayload>>,
    pub cancel: CancellationToken,
    pub encoding: std::sync::Arc<tokio::sync::Mutex<SessionEncoding>>,
    pub logger: Option<SessionLogger>,
    /// 等服务端对 `pty` + `shell` 请求的回执（对端控制用）。
    /// 普通 SSH 主机保持 false（发完即用，不等回执）。
    pub wait_replies: bool,
    /// 等待回执期间的可选取消（关闭正在等待对端确认的标签）。
    /// `None` 表示不可取消，只受 120s 截止时间约束。
    pub wait_cancel: Option<CancellationToken>,
}

/// 终端数据帧的统一出口。Channel 存在时 `send`（webview 侧经
/// `Channel.onmessage` 直达 xterm 写队列），否则回退 `term://data` 全局事件。
/// send 失败只记日志、不回退事件——channel 半失效时再走事件可能造成重复帧。
fn deliver_term_data(
    app: &Option<AppHandle>,
    data_channel: &Option<tauri::ipc::Channel<TermDataPayload>>,
    session_id: &str,
    data: &[u8],
) {
    let payload = TermDataPayload {
        session_id: session_id.to_string(),
        data: events::b64(data),
    };
    if let Some(ch) = data_channel {
        if let Err(e) = ch.send(payload) {
            applog!("[term] channel 发送终端数据失败（可能 webview 已重载）: {e}");
        }
        return;
    }
    if let Some(app) = app {
        events::emit(app, events::TERM_DATA, payload);
    }
}

/// 打开 session channel，request_pty + request_shell，启动 reader/writer 任务。
/// 返回控制 sender（内部 writer 持有 ChannelWriteHalf）。
///
/// `logger`：在**解码之前**把 `Data` / `ExtendedData` 的原始字节 tee 进会话日志；
/// 由 reader task 持有，任务结束（关闭 / 断线）时 drop 即 flush + 关闭文件。
pub async fn open_term_channel(
    handle: &mut client::Handle<ClientHandler>,
    args: TermChannelArgs,
) -> AppResult<mpsc::Sender<ChannelCmd>> {
    let TermChannelArgs {
        session_id,
        cols,
        rows,
        app,
        data_channel,
        cancel,
        encoding,
        logger,
        wait_replies,
        wait_cancel,
    } = args;
    let mut channel = handle
        .channel_open_session()
        .await
        .map_err(|e| AppError::ESshConnect(format!("打开会话通道失败: {e}")))?;
    channel
        .request_pty(
            wait_replies,
            "xterm-256color",
            cols as u32,
            rows as u32,
            0,
            0,
            &[],
        )
        .await
        .map_err(|e| AppError::ESshChannel(format!("申请 PTY 失败: {e}")))?;
    channel
        .request_shell(wait_replies)
        .await
        .map_err(|e| AppError::ESshChannel(format!("请求 shell 失败: {e}")))?;
    if wait_replies {
        // 对端控制：服务端可能先弹「本机确认」/ Windows Hello。被拒绝（或 30s 未确认）
        // 会回 ChannelFailure；不主动等回执的话，前端只会看到一个“连上马上断开”的空标签。
        let mut ok_count = 0;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(120);
        loop {
            let waited = tokio::select! {
                _ = crate::ssh::cancelled(wait_cancel.as_ref()) => {
                    return Err(AppError::ECancelled("等待对端确认已取消".to_string()));
                }
                r = tokio::time::timeout_at(deadline, channel.wait()) => r,
            };
            match waited {
                Ok(Some(russh::ChannelMsg::Success)) => {
                    ok_count += 1;
                    if ok_count >= 2 {
                        break;
                    }
                }
                Ok(Some(russh::ChannelMsg::Failure)) => {
                    return Err(AppError::EPeerNotAllowed(
                        "对端拒绝远程终端：未授予控制权限，或未在 30 秒内确认（含 Windows Hello 取消）"
                            .to_string(),
                    ));
                }
                // 其他消息（理论上不会早于回执）忽略
                Ok(Some(_)) => {}
                Ok(None) => {
                    return Err(AppError::ESshChannel(
                        "对端在建立终端时断开连接".to_string(),
                    ));
                }
                Err(_) => {
                    return Err(AppError::EPeerConsentTimeout(
                        "等待对端确认远程终端超时".to_string(),
                    ));
                }
            }
        }
    }

    let (read_half, write_half) = channel.split();

    // writer task：mpsc → channel
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<ChannelCmd>(64);
    let writer_cancel = cancel.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = writer_cancel.cancelled() => break,
                cmd = cmd_rx.recv() => match cmd {
                    Some(ChannelCmd::Data(data)) => {
                        // 背压等待（远端发送窗口为 0）也必须可取消：否则取消/关闭后
                        // data_bytes 永久挂起，任务与待发缓冲泄漏、关闭流程被拖死。
                        let write = tokio::select! {
                            _ = writer_cancel.cancelled() => break,
                            r = write_half.data_bytes(data) => r,
                        };
                        if write.is_err() {
                            break;
                        }
                    }
                    Some(ChannelCmd::Resize(c, r)) => {
                        let _ = write_half.window_change(c, r, 0, 0).await;
                    }
                    None => break,
                }
            }
        }
        let _ = write_half.close().await;
    });

    // reader task：channel → （tee 原始字节到会话日志）→ 按会话级编码增量解码
    // → data_channel（或回退 term://data 事件）；结束 → term://exit + conn://state，
    // 并停掉该会话的 metrics 采样。
    let sid = session_id.clone();
    let app2 = app.clone();
    let data_channel2 = data_channel;
    let reader_cancel = cancel.clone();
    tokio::spawn(async move {
        let mut read_half = read_half;
        loop {
            let msg = tokio::select! {
                _ = reader_cancel.cancelled() => break,
                m = read_half.wait() => m,
            };
            match msg {
                Some(ChannelMsg::Data { data }) => {
                    // 日志记录解码前的原始字节（ANSI 原样），与 term://data 的
                    // 解码输出互不影响；会话中途切换编码不会改变日志内容。
                    if let Some(log) = logger.as_ref() {
                        log.append(&data);
                    }
                    let decoded = {
                        let mut enc = encoding.lock().await;
                        decode_chunk(&mut enc, &data)
                    };
                    deliver_term_data(&app2, &data_channel2, &sid, &decoded);
                }
                Some(ChannelMsg::ExtendedData { data, .. }) => {
                    // stderr 同样是远端原始字节流的一部分，一并记录。
                    if let Some(log) = logger.as_ref() {
                        log.append(&data);
                    }
                    let decoded = {
                        let mut enc = encoding.lock().await;
                        decode_chunk(&mut enc, &data)
                    };
                    deliver_term_data(&app2, &data_channel2, &sid, &decoded);
                }
                Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) => break,
                Some(_) => {}
                None => break,
            }
        }
        // 流结束——解码器收尾，把残留的半截多字节序列按 U+FFFD 输出给终端
        //（不进会话日志：日志记录的是解码前的原始字节，已随数据块写入）。
        {
            let mut enc = encoding.lock().await;
            let tail = crate::encoding::finish_decode(&mut enc);
            if !tail.is_empty() {
                deliver_term_data(&app2, &data_channel2, &sid, &tail);
            }
        }
        // 断线/退出：同步取消该会话的 metrics 采样，
        // 并把 connected 置 false，避免 SFTP 复用到已经断掉的终端连接。
        //
        // `close_session` 会先 cancel 再移除会话，这种「主动关闭」由命令层
        // emit 一次 closed_by_user；reader 不再补发 disconnected（否则前端会先收到
        // closed_by_user 又收到 lost，误触发自动重连）。其余退出（EOF / 通道异常）
        // 统一按 lost 上报。
        //
        // 生命周期联动：连接已死时按主机级联——该 host 全部会话（含派生
        // SFTP）标记断开并 cancel 派生 token；非主动关闭再逐个清理派生会话的
        // SFTP 客户端并上报 lost，让前端同步删掉这些 SFTP 标签。主动关闭路径
        // 拥有者记录已被 `ssh::close` 移除，`get` 失败即整体跳过，级联由命令层
        // （closed_by_user 语义）负责，避免 lost / closed_by_user 双发竞态。
        let closed_by_user = reader_cancel.is_cancelled();
        if let Some(app) = &app2 {
            {
                use tauri::Manager;
                let state: tauri::State<crate::state::AppState> = app.state();
                state.metrics.cancel(&sid).await;
                if let Ok(live) = state.ssh.get(&sid).await {
                    let host_id = live.inner.host_id.clone();
                    state.ssh.mark_host_disconnected(&host_id).await;
                    if !closed_by_user {
                        for derived in state.ssh.derived_sessions_of(&host_id).await {
                            state.sftp.drop_session(&derived).await;
                            events::emit(
                                app,
                                events::CONN_STATE,
                                ConnStatePayload {
                                    session_id: derived,
                                    state: "disconnected".to_string(),
                                    latency_ms: None,
                                    reason: Some(events::CONN_REASON_LOST.to_string()),
                                },
                            );
                        }
                    }
                }
            }
            events::emit(
                app,
                events::TERM_EXIT,
                TermExitPayload {
                    session_id: sid.clone(),
                    reason: if closed_by_user {
                        "closed by user".to_string()
                    } else {
                        "remote closed".to_string()
                    },
                },
            );
            if !closed_by_user {
                events::emit(
                    app,
                    events::CONN_STATE,
                    ConnStatePayload {
                        session_id: sid,
                        state: "disconnected".to_string(),
                        latency_ms: None,
                        reason: Some(events::CONN_REASON_LOST.to_string()),
                    },
                );
            }
        }
    });

    Ok(cmd_tx)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 空串不发；非空时原样 + `\n` 写入一次（不等待提示符、不做队列）。
    #[tokio::test]
    async fn init_command_is_sent_once_with_newline() {
        let (tx, mut rx) = mpsc::channel::<ChannelCmd>(4);
        send_init_command(&tx, "").await.expect("空串是 no-op");
        assert!(rx.try_recv().is_err(), "空串不得向 channel 写任何数据");

        send_init_command(&tx, "echo RUTHENIUM_INIT_OK")
            .await
            .expect("写入失败");
        match rx.recv().await {
            Some(ChannelCmd::Data(bytes)) => {
                assert_eq!(bytes, b"echo RUTHENIUM_INIT_OK\n".to_vec());
            }
            _ => panic!("initCommand 必须以 Data 写入"),
        }
        assert!(rx.try_recv().is_err(), "只发一条，不得重复");
    }

    /// 通道已关闭时返回 E_SSH_CHANNEL（不 panic）。
    #[tokio::test]
    async fn init_command_reports_closed_channel() {
        let (tx, rx) = mpsc::channel::<ChannelCmd>(1);
        drop(rx);
        let err = send_init_command(&tx, "cd /tmp")
            .await
            .expect_err("通道关闭应报错");
        assert!(matches!(err, AppError::ESshChannel(_)), "实际: {err:?}");
    }
}
