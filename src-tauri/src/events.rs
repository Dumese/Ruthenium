use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Runtime};

// 事件名常量
pub const TERM_DATA: &str = "term://data";
pub const TERM_EXIT: &str = "term://exit";
pub const CONN_STATE: &str = "conn://state";
pub const HOSTKEY_CHALLENGE: &str = "hostkey://challenge";
pub const SFTP_PROGRESS: &str = "sftp://progress";
pub const SFTP_CHANGED: &str = "sftp://changed";
pub const TUNNEL_STATE: &str = "tunnel://state";
pub const METRICS_SAMPLE: &str = "metrics://sample";
pub const APP_ERROR: &str = "app://error";
/// 标题栏最大化按钮的 hover 状态（bool，见 `snap.rs`）：贴靠布局的原生覆盖层独占
/// 那块矩形的鼠标，webview 收不到 `:hover`，只能由这里补一个事件。
pub const SNAP_HOVER: &str = "snap://hover";
/// 同上：按下/抬起状态（bool），用来复刻 `:active` 观感。
pub const SNAP_PRESS: &str = "snap://press";
/// 局域网发现：已发现设备列表变化（payload: DiscoveredDevice[]）
pub const PEER_DISCOVERED: &str = "peer://discovered";
/// 配对流程：弹出配对框 / 码刷新 / 结果（payload: PairEvent）
pub const PEER_PAIRING: &str = "peer://pairing";
/// 配置收件箱：收到对端推送的配置（payload: InboxNotice）
pub const PEER_INBOX: &str = "peer://inbox";
/// 控制请求：对端请求控制本机（payload: ControlRequest），需本机确认
pub const PEER_CONTROL_REQUEST: &str = "peer://control-request";
/// 受控会话开始/结束（payload: ControlSessionEvent），前端横幅据此显示
pub const PEER_SESSION: &str = "peer://session";
/// 控制被拒绝（payload: DeniedEvent）：本机拒绝/未确认/Hello 未通过
pub const PEER_DENIED: &str = "peer://denied";
/// 屏幕查看请求：对端请求查看本机屏幕（payload: ScreenRequest），需本机确认
pub const PEER_SCREEN_REQUEST: &str = "peer://screen-request";
/// 受查看会话开始/结束（payload: ScreenSessionEvent），前端横幅据此显示
pub const PEER_SCREEN_SESSION: &str = "peer://screen-session";
/// 输入通道延迟（payload: { sessionId, rttMs }）
pub const PEER_INPUT_STATS: &str = "peer://input-stats";
/// 查看端的剪贴板通道结束（对端断开 / 授权撤销）：payload: { sessionId, id }
pub const PEER_CLIPBOARD: &str = "peer://clipboard";

/// `conn://state` 的断线原因（payload.reason）：
/// 网络 / 通道异常（网络断、远端关 shell、keepalive 超限）——前端据此决定是否自动重连。
pub const CONN_REASON_LOST: &str = "lost";
/// 用户主动关闭（close_session / 关标签）——不得触发自动重连。
pub const CONN_REASON_CLOSED_BY_USER: &str = "closed_by_user";

/// 发送 Tauri 事件；发送失败只记 stderr，不影响调用方。
/// 泛型 `R: Runtime`：生产是 Wry，单测可用 MockRuntime。
pub fn emit<T: Serialize + Clone, R: Runtime>(app: &AppHandle<R>, event: &str, payload: T) {
    if let Err(e) = app.emit(event, payload) {
        applog!("[events] emit {event} failed: {e}");
    }
}

/// 终端字节 → base64（STANDARD alphabet）。
///
/// IPC 热路径：`Vec<u8>` 走 serde JSON 会序列化成 `[228,186,...]` 数字数组，
/// 64 KiB 输出膨胀到约 250 KB 文本，前端还要构造 6 万个元素的 `number[]`；
/// base64 只有约 1/5 体积，前端 `atob` 一次即可拿到二进制串，解析快一个量级。
pub fn b64(bytes: &[u8]) -> String {
    B64.encode(bytes)
}

/// base64 → 终端字节；非法输入返回错误（命令层映射成 `E_INVALID_INPUT`，绝不 panic）。
pub fn b64_decode(s: &str) -> Result<Vec<u8>, base64::DecodeError> {
    B64.decode(s)
}

/// `term://data` 负载：会话 id + 终端字节的 base64 文本。
/// 同时 derive `Deserialize`——作为 `ipc::Channel<TermDataPayload>` 的参数类型
/// 时，tauri 命令宏要求可反序列化（前端 Channel 发来的回执不需要，但类型边界统一）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TermDataPayload {
    pub session_id: String,
    /// base64（STANDARD alphabet）编码的终端字节；前端解码回 `Uint8Array` 再写 xterm。
    pub data: String,
}

/// `term://exit` 负载：会话 id + 退出原因。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TermExitPayload {
    pub session_id: String,
    pub reason: String,
}

/// `conn://state` 负载：连接状态与可选的延迟 / 断线原因。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnStatePayload {
    pub session_id: String,
    pub state: String, // connecting | connected | disconnected
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u32>,
    /// 断线原因，仅 `disconnected` 时出现。
    /// `"lost"`（网络/通道异常）| `"closed_by_user"`（用户主动关闭）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// `hostkey://challenge` 负载：待确认的主机指纹与变更标记。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostKeyPayload {
    pub session_id: String,
    pub host_id: String,
    pub host: String,
    pub port: u16,
    pub algo: String,
    pub fingerprint: String,
    pub changed: bool,
}

/// `sftp://progress` 负载：传输进度与状态（running | done | error | cancelled）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SftpProgressPayload {
    pub transfer_id: String,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub state: String, // running | done | error | cancelled
}

/// `sftp://changed` 负载：发生变更的侧（local | remote）与目录。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SftpChangedPayload {
    pub session_id: String,
    pub side: String, // local | remote
    pub path: String,
}

/// `tunnel://state` 负载：规则监听状态与可选错误。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TunnelStatePayload {
    pub rule_id: String,
    pub active: bool,
    pub error: Option<String>,
}

/// 查看端剪贴板通道结束（对端断开 / 授权撤销）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClipboardEndedPayload {
    pub session_id: String,
    pub id: String,
}

/// `app://error` 负载：错误码 + 详情。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppErrorPayload {
    pub code: String,
    pub detail: String,
    /// 关联会话（指纹超时等需要精确定位弹窗的错误）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

impl AppErrorPayload {
    /// 由错误码和 `Display` 错误构造负载。
    pub fn from_err(code: &str, detail: impl std::fmt::Display) -> Self {
        Self {
            code: code.to_string(),
            detail: detail.to_string(),
            session_id: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 空字节：base64 是空串，解码回到空 Vec（不是错误）。
    #[test]
    fn b64_empty_roundtrip() {
        let encoded = b64(&[]);
        assert_eq!(encoded, "");
        assert_eq!(
            b64_decode(&encoded).expect("空串应可解码"),
            Vec::<u8>::new()
        );
    }

    /// 全 256 个字节值（0x00-0xFF）：不受 UTF-8 合法性影响，必须无损往返。
    #[test]
    fn b64_all_byte_values_roundtrip() {
        let raw: Vec<u8> = (0u8..=255).collect();
        let encoded = b64(&raw);
        // STANDARD alphabet：只含 A-Z a-z 0-9 + / =，不含换行等空白。
        assert!(
            encoded
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'='),
            "编码结果必须是 STANDARD alphabet: {encoded}"
        );
        assert_eq!(b64_decode(&encoded).expect("应可解码"), raw);
    }

    /// 中文 UTF-8 字节（多字节序列被切在任意位置也不影响：按字节编码）。
    #[test]
    fn b64_utf8_chinese_roundtrip() {
        let raw = "终端输出：你好，Ruthenium！\r\n".as_bytes();
        let encoded = b64(raw);
        assert_eq!(b64_decode(&encoded).expect("应可解码"), raw);
        // 已知 UTF-8 字节的 base64 也能与标准实现互认（避免用错 alphabet）。
        assert_eq!(b64("你好".as_bytes()), "5L2g5aW9");
    }

    /// 非法 base64：返回 Err 而不是 panic（命令层据此回 E_INVALID_INPUT）。
    #[test]
    fn b64_decode_rejects_invalid_input() {
        for bad in ["!!!!", "a", "5L2g5aW9*"] {
            assert!(b64_decode(bad).is_err(), "非法 base64 应报错: {bad}");
        }
    }

    /// IPC 契约：payload 序列化成驼峰字段 + data 是 base64 字符串（不再是数字数组）。
    #[test]
    fn term_data_payload_serializes_base64_string() {
        let payload = TermDataPayload {
            session_id: "s1".to_string(),
            data: b64(&[0x00, 0xff, b'a']),
        };
        let json = serde_json::to_value(&payload).expect("payload 应可序列化");
        assert_eq!(json["sessionId"], "s1");
        assert_eq!(json["data"], "AP9h");
        assert!(json["data"].is_string(), "data 必须是 base64 字符串");
    }
}
