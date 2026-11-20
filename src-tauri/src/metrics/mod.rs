//! 指标命令：start_metrics / stop_metrics / sample_metrics。
//! 每 2 秒采样一帧并 emit `metrics://sample`；取消路径：stop_metrics、
//! close_session、reader 断线（conn://state disconnected）。远端无 /proc 或
//! 命令失败时停采样并按 E_SSH_CHANNEL 记日志，不 panic、不影响终端会话。
pub mod sampler;

use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tauri::Manager;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::error::AppResult;
use crate::events;
use crate::models::HostMetrics;
use crate::ssh::LiveSession;
use crate::state::AppState;
use sampler::{diff_to_metrics, parse_metrics_output, prev_from, PrevSample, RawSample};

const SAMPLE_INTERVAL: Duration = Duration::from_secs(2);

/// 挂在 AppState 上的采样任务表：session_id → 取消令牌。
#[derive(Default)]
pub struct MetricsManager {
    tasks: Mutex<HashMap<String, CancellationToken>>,
}

impl MetricsManager {
    /// 创建空的指标采样任务表。
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册（若已有旧任务则先取消），返回新令牌。
    pub async fn register(&self, session_id: &str) -> CancellationToken {
        let mut tasks = self.tasks.lock().await;
        if let Some(old) = tasks.remove(session_id) {
            old.cancel();
        }
        let token = CancellationToken::new();
        tasks.insert(session_id.to_string(), token.clone());
        token
    }

    /// 取消并移除采样任务；不存在则 no-op。
    pub async fn cancel(&self, session_id: &str) {
        if let Some(token) = self.tasks.lock().await.remove(session_id) {
            token.cancel();
        }
    }
}

/// 单次 channel.wait() 的超时：远端异常时 wait() 可能永久挂起，导致采样停摆。
const EXEC_WAIT_TIMEOUT: Duration = Duration::from_secs(15);
/// 整条指标命令的总预算：单次 wait 限时不够——被控/异常远端持续输出时
/// 逐次重置的 15s 会让采样循环长期占用；整条命令 30s 内必须结束。
const EXEC_TOTAL_BUDGET: Duration = Duration::from_secs(30);
/// 输出上限：METRICS_CMD 的正常输出 <8 KiB；超限说明远端在洪泛（cat 被
/// 替换、/proc 挂 FUSE 等），按采样失败终止，不让 out 无界增长。
const MAX_OUTPUT_BYTES: usize = 256 * 1024;

/// 复用该会话的 SSH 连接新开 channel 执行只读命令，收集 stdout 文本。
async fn exec_metrics_command(live: &LiveSession) -> AppResult<String> {
    // 关键：连接锁只在「开通道 + exec」期间持有。随后的读循环最长可等
    // EXEC_WAIT_TIMEOUT（15s），若把锁带进循环，会阻塞 SFTP 开通道、-L 新连接、
    // 新会话建 PTY 等所有需要 live.russh 的操作（每 2s 采样一次即持续卡顿）。
    let mut channel = {
        let handle = live.russh.lock().await;
        // Channel::exec 只取 &self；只有后面的 wait() 需要 &mut。
        let channel = handle
            .channel_open_session()
            .await
            .map_err(|e| crate::error::AppError::ESshChannel(format!("打开指标通道失败: {e}")))?;
        channel
            .exec(false, sampler::METRICS_CMD)
            .await
            .map_err(|e| crate::error::AppError::ESshChannel(format!("执行指标命令失败: {e}")))?;
        channel
    };
    let started = std::time::Instant::now();
    let mut out = Vec::new();
    loop {
        // 剩余预算 = 总预算 - 已耗时；单次 wait 不超过剩余预算。
        let remaining = EXEC_TOTAL_BUDGET.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            applog!("[metrics] 指标命令总预算 {EXEC_TOTAL_BUDGET:?} 用尽，视为采样失败");
            return Err(crate::error::AppError::ESshChannel(
                "指标命令整体超时".to_string(),
            ));
        }
        // 每次 wait 都限时：超时即视为采样失败（上层 None → 停采样日志路径）。
        let wait_for = EXEC_WAIT_TIMEOUT.min(remaining);
        let msg = match tokio::time::timeout(wait_for, channel.wait()).await {
            Ok(m) => m,
            Err(_) => {
                applog!("[metrics] 指标通道 {wait_for:?} 内无响应，视为采样失败并停止采样");
                return Err(crate::error::AppError::ESshChannel(
                    "指标命令输出超时".to_string(),
                ));
            }
        };
        let Some(msg) = msg else {
            break; // 通道已关闭
        };
        match msg {
            russh::ChannelMsg::Data { data } => {
                out.extend_from_slice(&data);
                if out.len() > MAX_OUTPUT_BYTES {
                    applog!(
                        "[metrics] 指标输出超过 {} KiB 上限，视为远端异常并停止采样",
                        MAX_OUTPUT_BYTES / 1024
                    );
                    return Err(crate::error::AppError::ESshChannel(
                        "指标命令输出超限".to_string(),
                    ));
                }
            }
            russh::ChannelMsg::ExtendedData { .. } => {}
            russh::ChannelMsg::Eof | russh::ChannelMsg::Close => break,
            _ => {}
        }
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 采一帧：exec → parse → 差值换算。失败返回 None（调用方停采样）。
/// 差值间隔由 PrevSample 内部的采样时刻 Instant 计算，无需外部时间参数。
async fn sample_once(
    live: &LiveSession,
    session_id: &str,
    prev: &mut Option<PrevSample>,
) -> Option<HostMetrics> {
    let text = exec_metrics_command(live).await.ok()?;
    let raw: RawSample = parse_metrics_output(&text)?;
    let metrics = diff_to_metrics(session_id, now_millis(), &raw, prev.as_ref());
    *prev = Some(prev_from(&raw));
    Some(metrics)
}

/// 采样循环：立即采一帧，随后每 2s 一帧；失败或取消即退出。
async fn sampler_loop(
    live: std::sync::Arc<LiveSession>,
    app: tauri::AppHandle,
    cancel: CancellationToken,
) {
    let session_id = live.inner.id.clone();
    let mut prev: Option<PrevSample> = None;
    loop {
        let frame = tokio::select! {
            _ = cancel.cancelled() => break,
            f = sample_once(&live, &session_id, &mut prev) => f,
        };
        match frame {
            Some(metrics) => events::emit(&app, events::METRICS_SAMPLE, metrics),
            None => {
                applog!(
                    "[metrics] E_SSH_CHANNEL: 会话 {session_id} 指标采样失败（远端无 /proc 或通道错误），停止采样"
                );
                break;
            }
        }
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tokio::time::sleep(SAMPLE_INTERVAL) => {}
        }
    }
    // 自清理：从任务表移除（仅当仍是本循环的令牌）
    app.state::<AppState>()
        .metrics
        .remove_if_done(&session_id, &cancel)
        .await;
}

/// 为会话启动周期采样（每 2s emit `metrics://sample`）；重复调用会替换旧任务。
#[tauri::command]
pub async fn start_metrics(state: tauri::State<'_, AppState>, session_id: String) -> AppResult<()> {
    let live = state.ssh.get(&session_id).await?;
    let cancel = state.metrics.register(&session_id).await;
    tokio::spawn(sampler_loop(live, state.app.clone(), cancel));
    Ok(())
}

/// 停止该会话的周期采样；任务不存在时 no-op。
#[tauri::command]
pub async fn stop_metrics(state: tauri::State<'_, AppState>, session_id: String) -> AppResult<()> {
    state.metrics.cancel(&session_id).await;
    Ok(())
}

/// 立即采样一帧；一次性请求不共享周期采样的差值基准（首帧差值项为 0）。
#[tauri::command]
pub async fn sample_metrics(
    state: tauri::State<'_, AppState>,
    session_id: String,
) -> AppResult<HostMetrics> {
    let live = state.ssh.get(&session_id).await?;
    let mut prev: Option<PrevSample> = None;
    let mut metrics = sample_once(&live, &session_id, &mut prev)
        .await
        .ok_or_else(|| crate::error::AppError::ESshChannel("指标采样失败".to_string()))?;
    // sample_metrics 是一次性请求，不与周期采样共享差值基准：保持首帧语义，
    // 但若周期采样正在跑，这里独立计算（差值项为 0），不干扰其缓存。
    metrics.sampled_at = now_millis();
    Ok(metrics)
}

impl MetricsManager {
    /// 采样循环退出后自清理：仅当映射仍指向该令牌时移除
    /// （避免误删 start_metrics 重新注册的新令牌）。
    pub async fn remove_if_done(&self, session_id: &str, token: &CancellationToken) {
        let mut tasks = self.tasks.lock().await;
        if tasks.get(session_id).is_some_and(|t| t == token) {
            tasks.remove(session_id);
        }
    }
}
