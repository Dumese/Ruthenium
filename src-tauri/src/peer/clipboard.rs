//! 剪贴板文本 + 富格式同步（`subsystem("ruthenium-clipboard")`）。
//!
//! 支持的格式：Unicode 文本、HTML（注册格式 `HTML Format`）、RTF（注册格式
//! `Rich Text Format`）、PNG（注册格式 `PNG`）、DIB（`CF_DIBV5`/`CF_DIB`）。
//! 不做文件复制，也不做「剪贴板监听」——只在用户显式点击时拉取/发送。
//!
//! 安全边界：
//! - 持久授权 `grant_clipboard`（默认关）+ 屏幕查看会话令牌（随机 UUID）；
//! - 只搬运格式字节：**不渲染 HTML、不解析 RTF、不自动拉取任何外链**；
//! - 写入本机剪贴板前统一清洗：HTML 按白名单去脚本/链接/图片/属性并限 1 MiB，
//!   RTF 含 `\objdata`/`\pict` 等内嵌对象或图片时整段丢弃（方案 §4.5）；
//! - 单格式原始字节 ≤ 24 MiB、本地读取总量 ≤ 32 MiB，超限丢弃该格式（文本优先保留）；
//! - 线上单帧上限经 `caps` 协商：双方都声明 `rich` 时 32 MiB，否则按旧版 16 MiB；
//! - 查看端默认不同步：用户在工具栏显式开启本次会话的剪贴板通道后才可拉取/发送；
//! - 每条消息都复核会话仍然存在（查看结束/被踢后立即失效）；服务端逐条复核授权。
//!
//! 协议（4 字节大端长度 + JSON；对旧版本保持向后兼容）：
//! - 查看端 `hello` 追加 `caps: ["rich"]` 与 `maxPayload`；旧被控端按字段取值，
//!   未知字段自然忽略。
//! - 被控端 ack 仍是 `{"type":"text","text":...}`，另外携带 `formats` 与
//!   `caps`/`maxPayload`；旧查看端只读 `text`，不受影响。
//! - `get` 可带 `formats: ["text","html","rtf","png","dib"]`；响应仍以顶层 `text`
//!   为准，可选格式放在 `formats` 对象：文本类为 UTF-8 字符串，图片类为 base64。
//!   文本被协议上限截断时响应带 `truncated: true`，查看端据此提示用户。
//! - `set` 保留顶层 `text`（纯文本），新增 `formats`；旧被控端只落文本，新被控端
//!   写全部格式。
//! - `formats.text` 与顶层 `text` 等价；本实现发送时省略它以免大文本占两倍带宽，
//!   接收时若存在且顶层为空则采用。

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_util::sync::CancellationToken;

use crate::error::{AppError, AppResult};
use crate::events::{b64, b64_decode};
use crate::peer::rd::ScreenHub;
use crate::state::AppState;

pub const SUBSYSTEM: &str = "ruthenium-clipboard";

/// 旧版本协议的单帧上限（未协商 `maxPayload` 时按此处理）。
const MAX_PAYLOAD_LEGACY: usize = 16 * 1024 * 1024;
/// 富格式协商上限：双方都声明 `rich` 时生效。
const MAX_PAYLOAD_RICH: usize = 32 * 1024 * 1024;
/// 协商下限：对端声明的 `maxPayload` 小于此值时按此值处理（避免病态值）。
const MIN_PAYLOAD: usize = 64 * 1024;
/// 单个格式的原始字节上限（文本按 UTF-8 字节数，图片按原始字节数）。
const MAX_FORMAT_BYTES: usize = 24 * 1024 * 1024;
/// 本地一次读取/写入的所有格式原始字节总和上限。
const MAX_LOCAL_TOTAL_BYTES: usize = 32 * 1024 * 1024;
/// HTML 写入本机剪贴板前的字节上限（超出整段丢弃，不截断 HTML）。
const MAX_HTML_BYTES: usize = 1024 * 1024;
/// `hello.caps` 里声明的富格式能力标识。
const CAP_RICH: &str = "rich";
/// 协议格式名（顺序即优先级：text 最高，dib 最低）。
const FORMAT_NAMES: [&str; 5] = ["text", "html", "rtf", "png", "dib"];

/// 一份剪贴板内容；二进制保持原始字节，不做格式转换。
///
/// 只在 crate 内使用：跨 IPC 与跨网络时分别换成 [`ClipboardRich`]（base64）
/// 与 JSON `formats` 对象。
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ClipContent {
    text: Option<String>,
    html: Option<String>,
    rtf: Option<String>,
    png: Option<Vec<u8>>,
    dib: Option<Vec<u8>>,
}

impl ClipContent {
    /// 是否没有任何可写内容（空字符串按「无」处理）。
    pub(crate) fn is_empty(&self) -> bool {
        fn non_empty(s: &Option<String>) -> bool {
            s.as_deref().is_some_and(|x| !x.is_empty())
        }
        !non_empty(&self.text)
            && !non_empty(&self.html)
            && !non_empty(&self.rtf)
            && !self.png.as_deref().is_some_and(|x| !x.is_empty())
            && !self.dib.as_deref().is_some_and(|x| !x.is_empty())
    }

    /// 所有格式的原始字节总和。
    fn total_bytes(&self) -> usize {
        self.text.as_deref().map_or(0, str::len)
            + self.html.as_deref().map_or(0, str::len)
            + self.rtf.as_deref().map_or(0, str::len)
            + self.png.as_deref().map_or(0, |x| x.len())
            + self.dib.as_deref().map_or(0, |x| x.len())
    }

    /// 各格式字节数的日志摘要（只记长度，不落内容）。
    pub(crate) fn summary(&self) -> String {
        format!(
            "text={}B html={}B rtf={}B png={}B dib={}B",
            self.text.as_deref().map_or(0, str::len),
            self.html.as_deref().map_or(0, str::len),
            self.rtf.as_deref().map_or(0, str::len),
            self.png.as_deref().map_or(0, |x| x.len()),
            self.dib.as_deref().map_or(0, |x| x.len()),
        )
    }
}

/// IPC / 前端使用的富剪贴板载荷：文本类为 UTF-8 字符串，图片为 base64。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ClipboardRich {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub html: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rtf: Option<String>,
    /// PNG 原始字节的 base64。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub png: Option<String>,
    /// DIB 原始字节的 base64。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dib: Option<String>,
    /// 该内容来自对端且文本因协议上限被截断（仅读取结果使用；写入时忽略）。
    #[serde(skip_serializing_if = "is_false")]
    pub truncated: bool,
}

/// `skip_serializing_if` 用的布尔判断（serde 传入引用）。
fn is_false(value: &bool) -> bool {
    !*value
}

impl ClipboardRich {
    /// 内部内容 → IPC 载荷（空格式省略；图片 base64 编码）。
    pub(crate) fn from_content(content: &ClipContent) -> Self {
        Self {
            text: content.text.clone().filter(|s| !s.is_empty()),
            html: content.html.clone().filter(|s| !s.is_empty()),
            rtf: content.rtf.clone().filter(|s| !s.is_empty()),
            png: content.png.as_deref().filter(|b| !b.is_empty()).map(b64),
            dib: content.dib.as_deref().filter(|b| !b.is_empty()).map(b64),
            truncated: false,
        }
    }

    /// 是否包含文本之外的可选格式（不含 `truncated` 元数据）。
    fn has_optional_formats(&self) -> bool {
        self.html.is_some() || self.rtf.is_some() || self.png.is_some() || self.dib.is_some()
    }

    /// 校验并转换为内部内容；超限或非法 base64 返回面向用户的中文错误。
    ///
    /// 这里的上限与本机/网络侧一致（单格式 24 MiB、总量 32 MiB），
    /// 避免无确认的剪贴板写入被超大载荷滥用。
    pub(crate) fn into_content(self) -> Result<ClipContent, String> {
        let content = ClipContent {
            text: checked_text("文本", self.text)?,
            html: checked_text("HTML", self.html)?,
            rtf: checked_text("RTF", self.rtf)?,
            png: decode_binary("PNG", self.png)?,
            dib: decode_binary("DIB", self.dib)?,
        };
        let total = content.total_bytes();
        if total > MAX_LOCAL_TOTAL_BYTES {
            return Err(format!(
                "剪贴板内容过大（{total} 字节，上限 {} MiB）",
                MAX_LOCAL_TOTAL_BYTES / (1024 * 1024)
            ));
        }
        Ok(content)
    }
}

/// 文本字段校验：空串视为无；超过单格式上限报错（不静默截断）。
fn checked_text(name: &str, value: Option<String>) -> Result<Option<String>, String> {
    match value.filter(|s| !s.is_empty()) {
        Some(s) if s.len() > MAX_FORMAT_BYTES => Err(format!(
            "剪贴板{name}超过大小上限（{} MiB）",
            MAX_FORMAT_BYTES / (1024 * 1024)
        )),
        other => Ok(other),
    }
}

/// base64 图片字段校验：先按长度估算再解码，避免为超限内容分配大内存。
fn decode_binary(name: &str, value: Option<String>) -> Result<Option<Vec<u8>>, String> {
    let Some(s) = value.filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    // 4 个 base64 字符约等于 3 字节；估算超限就直接拒绝。
    if s.len() / 4 * 3 > MAX_FORMAT_BYTES {
        return Err(format!(
            "剪贴板{name}超过大小上限（{} MiB）",
            MAX_FORMAT_BYTES / (1024 * 1024)
        ));
    }
    let bytes = b64_decode(&s).map_err(|e| format!("剪贴板{name}不是合法的 base64: {e}"))?;
    if bytes.len() > MAX_FORMAT_BYTES {
        return Err(format!(
            "剪贴板{name}超过大小上限（{} MiB）",
            MAX_FORMAT_BYTES / (1024 * 1024)
        ));
    }
    Ok(Some(bytes))
}

/// 查看端打开剪贴板通道的返回：通道 id + 远端当前剪贴板内容。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClipboardStart {
    pub id: String,
    /// 远端当前剪贴板文本（旧被控端只有这一项）。
    pub text: String,
    /// 远端当前剪贴板的富格式（旧被控端 / 空剪贴板时省略；文本已合并进 `text`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub formats: Option<ClipboardRich>,
    /// 远端文本是否因协议 payload 上限被截断。
    #[serde(skip_serializing_if = "is_false")]
    pub truncated: bool,
}

/* ---------------- 大小裁剪与格式选择（纯函数，便于单测） ---------------- */

/// 按优先级（text → html → rtf → png → dib）应用单格式与总量上限。
///
/// 超限格式整体丢弃（不截断），返回保留的内容与被丢弃的格式名。
/// text 是最高优先级，只会因自身超限被丢弃，不会被其它格式挤掉。
fn apply_limits(
    content: ClipContent,
    max_each: usize,
    max_total: usize,
) -> (ClipContent, Vec<&'static str>) {
    struct Limiter {
        max_each: usize,
        max_total: usize,
        total: usize,
        dropped: Vec<&'static str>,
    }

    impl Limiter {
        fn accept(&mut self, name: &'static str, size: usize) -> bool {
            if size > self.max_each || self.total.saturating_add(size) > self.max_total {
                self.dropped.push(name);
                false
            } else {
                self.total += size;
                true
            }
        }

        fn take_text(
            &mut self,
            slot: &mut Option<String>,
            name: &'static str,
            value: Option<String>,
        ) {
            let Some(value) = value.filter(|s| !s.is_empty()) else {
                return;
            };
            if self.accept(name, value.len()) {
                *slot = Some(value);
            }
        }

        fn take_bytes(
            &mut self,
            slot: &mut Option<Vec<u8>>,
            name: &'static str,
            value: Option<Vec<u8>>,
        ) {
            let Some(value) = value.filter(|b| !b.is_empty()) else {
                return;
            };
            if self.accept(name, value.len()) {
                *slot = Some(value);
            }
        }
    }

    let mut out = ClipContent::default();
    let mut limiter = Limiter {
        max_each,
        max_total,
        total: 0,
        dropped: Vec::new(),
    };
    limiter.take_text(&mut out.text, "text", content.text);
    limiter.take_text(&mut out.html, "html", content.html);
    limiter.take_text(&mut out.rtf, "rtf", content.rtf);
    limiter.take_bytes(&mut out.png, "png", content.png);
    limiter.take_bytes(&mut out.dib, "dib", content.dib);
    (out, limiter.dropped)
}

fn log_dropped(dropped: &[&'static str], reason: &str) {
    for name in dropped {
        applog!("[peer] 剪贴板格式 {name} {reason}，已丢弃");
    }
}

/* ---------------- 写入本机剪贴板前的富格式清洗 ---------------- */

/// 允许保留的 HTML 标签：只保留排版，不含链接、图片、脚本、样式表与
/// iframe 等可携带外部内容或执行逻辑的元素；属性一律不保留。
const HTML_ALLOWED_TAGS: [&str; 28] = [
    "b", "strong", "i", "em", "u", "s", "strike", "del", "ins", "sub", "sup", "p", "br", "ul",
    "ol", "li", "blockquote", "code", "pre", "h1", "h2", "h3", "h4", "h5", "h6", "span", "div",
    "hr",
];

/// RTF 中可携带 OLE 对象或位图数据的 destination 控制词。
const RTF_DROP_DESTINATIONS: [&str; 6] = [
    "objdata",
    "pict",
    "object",
    "datastore",
    "themedata",
    "colorschememapping",
];

/// HTML 白名单清洗：去掉脚本/样式/链接/图片及所有属性，只保留排版标签。
///
/// 只用于「写入本机剪贴板」：远端 HTML 会被粘贴方应用解析，必须按方案
/// §4.5 的边界处理。清洗后超过 [`MAX_HTML_BYTES`] 时整段丢弃。
fn sanitize_html(html: &str) -> Option<String> {
    if html.len() > MAX_HTML_BYTES {
        applog!(
            "[peer] 剪贴板 HTML 超过 {} KiB，已丢弃",
            MAX_HTML_BYTES / 1024
        );
        return None;
    }
    let tags: std::collections::HashSet<&str> = HTML_ALLOWED_TAGS.into_iter().collect();
    // 不保留任何属性：排除 on*、style、href/src 等可携带行为或外部请求的入口。
    let attrs: std::collections::HashMap<&str, std::collections::HashSet<&str>> =
        std::collections::HashMap::new();
    let cleaned = ammonia::Builder::default()
        .tags(tags)
        .tag_attributes(attrs)
        .generic_attributes(std::collections::HashSet::new())
        .link_rel(None)
        .url_relative(ammonia::UrlRelative::Deny)
        .clean(html)
        .to_string();
    if cleaned.len() > MAX_HTML_BYTES {
        applog!(
            "[peer] 剪贴板 HTML 清洗后仍超过 {} KiB，已丢弃",
            MAX_HTML_BYTES / 1024
        );
        return None;
    }
    if cleaned.trim().is_empty() {
        return None;
    }
    Some(cleaned)
}

/// RTF 是否包含内嵌对象/图片 destination。
///
/// 按控制词扫描，跳过 `\\`、`\{`、`\}`、`\'hh` 转义，避免把正文里的
/// 反斜杠误判成控制词。
fn rtf_has_embedded_object(rtf: &str) -> bool {
    let bytes = rtf.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != b'\\' {
            i += 1;
            continue;
        }
        let Some(&next) = bytes.get(i + 1) else { break };
        if next == b'\\' || next == b'{' || next == b'}' {
            i += 2;
            continue;
        }
        if next == b'\'' {
            i += 4;
            continue;
        }
        if !next.is_ascii_alphabetic() {
            i += 1;
            continue;
        }
        let start = i + 1;
        let mut end = start;
        while end < bytes.len() && bytes[end].is_ascii_alphabetic() {
            end += 1;
        }
        let word = &bytes[start..end];
        if RTF_DROP_DESTINATIONS
            .iter()
            .any(|drop| word.eq_ignore_ascii_case(drop.as_bytes()))
        {
            return true;
        }
        i = end;
    }
    false
}

/// RTF 清洗：含内嵌对象/图片时整段禁用，否则保留原始排版字节。
///
/// 方案 §4.5 允许「只提取文本或整段禁用」；这里选后者，避免自己实现
/// `\objdata` / `\pict` 的二进制边界解析。纯排版 RTF（加粗、颜色等）
/// 不含这些 destination，可以原样保留。
fn sanitize_rtf(rtf: &str) -> Option<String> {
    if rtf_has_embedded_object(rtf) {
        applog!("[peer] 剪贴板 RTF 含内嵌对象或图片，已整段丢弃");
        return None;
    }
    Some(rtf.to_string())
}

/// 写入本机剪贴板前统一清洗富格式（网络接收与 IPC 写入共用）。
pub(crate) fn sanitize_content(content: &mut ClipContent) {
    if let Some(html) = content.html.take() {
        content.html = sanitize_html(&html);
    }
    if let Some(rtf) = content.rtf.take() {
        content.rtf = sanitize_rtf(&rtf);
    }
}

/* ---------------- 线上 formats 对象 <-> 内部内容 ---------------- */

/// 内容 → 线上 `formats` 对象（不含 text：顶层 `text` 已承载，避免大文本双倍占带宽）。
fn formats_to_json(content: &ClipContent) -> Map<String, Value> {
    format_values(content)
        .into_iter()
        .map(|(name, value)| (name.to_string(), value))
        .collect()
}

/// 按优先级返回 `formats` 条目（html → rtf → png → dib）。
fn format_values(content: &ClipContent) -> Vec<(&'static str, Value)> {
    let mut values: Vec<(&'static str, Value)> = Vec::new();
    if let Some(html) = content.html.as_deref().filter(|s| !s.is_empty()) {
        values.push(("html", Value::String(html.to_string())));
    }
    if let Some(rtf) = content.rtf.as_deref().filter(|s| !s.is_empty()) {
        values.push(("rtf", Value::String(rtf.to_string())));
    }
    if let Some(png) = content.png.as_deref().filter(|b| !b.is_empty()) {
        values.push(("png", Value::String(b64(png))));
    }
    if let Some(dib) = content.dib.as_deref().filter(|b| !b.is_empty()) {
        values.push(("dib", Value::String(b64(dib))));
    }
    values
}

/// 解析线上 `formats` 对象：类型不对/非法 base64/超限的条目跳过（记日志），
/// 不影响同一消息里的其它格式（尤其是顶层文本）。
fn parse_formats(obj: &Value) -> ClipContent {
    let Some(map) = obj.as_object() else {
        return ClipContent::default();
    };
    let mut content = ClipContent {
        text: map
            .get("text")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        html: map
            .get("html")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        rtf: map
            .get("rtf")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        png: decode_binary(
            "PNG",
            map.get("png").and_then(Value::as_str).map(str::to_string),
        )
        .unwrap_or_else(|e| {
            applog!("[peer] 剪贴板 PNG 字段非法或超限，已丢弃: {e}");
            None
        }),
        dib: decode_binary(
            "DIB",
            map.get("dib").and_then(Value::as_str).map(str::to_string),
        )
        .unwrap_or_else(|e| {
            applog!("[peer] 剪贴板 DIB 字段非法或超限，已丢弃: {e}");
            None
        }),
    };
    let (limited, dropped) = apply_limits(content.clone(), MAX_FORMAT_BYTES, MAX_LOCAL_TOTAL_BYTES);
    // 文本/HTML/RTF 的单格式超限也在这里被丢弃，一并记录。
    if limited.total_bytes() != content.total_bytes() {
        log_dropped(&dropped, "超过大小上限");
    }
    content = limited;
    content
}

/// 从 `type:"text"` 应答解析内容：顶层 `text` 优先，`formats` 提供可选格式。
///
/// 返回 `(内容, 文本是否被截断)`；`truncated` 由被控端在响应里显式标记。
fn content_from_text_response(v: &Value) -> (ClipContent, bool) {
    let mut content = ClipContent::default();
    if let Some(text) = v
        .get("text")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        content.text = Some(text.to_string());
    }
    if let Some(obj) = v.get("formats") {
        let parsed = parse_formats(obj);
        if content.text.is_none() {
            content.text = parsed.text;
        }
        content.html = parsed.html;
        content.rtf = parsed.rtf;
        content.png = parsed.png;
        content.dib = parsed.dib;
    }
    let truncated = v
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let (content, dropped) = apply_limits(content, MAX_FORMAT_BYTES, MAX_LOCAL_TOTAL_BYTES);
    log_dropped(&dropped, "超过大小上限");
    (content, truncated)
}

/// 把文本截断到 `max_payload` 的 JSON 应答预算内（保留 UTF-8 字符边界）。
fn fit_text_to_payload(text: &str, id: u64, max_payload: usize) -> String {
    let mut fitted = text.to_string();
    for _ in 0..8 {
        let value = serde_json::json!({ "type": "text", "id": id, "text": fitted });
        let Ok(bytes) = serde_json::to_vec(&value) else {
            return String::new();
        };
        if bytes.len() <= max_payload {
            return fitted;
        }
        let budget = fitted.len().saturating_sub(bytes.len() - max_payload + 64);
        let next = truncate_utf8(&fitted, budget).to_string();
        if next == fitted {
            return String::new();
        }
        fitted = next;
    }
    fitted
}

/// 按字节预算截断字符串，保留合法 UTF-8 边界。
fn truncate_utf8(text: &str, budget: usize) -> &str {
    if text.len() <= budget {
        return text;
    }
    let mut end = budget;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// 构造 `type:"text"` 应答：顶层文本必带；协商了 rich 时按优先级在 payload
/// 预算内追加 `formats`（放不下的格式丢弃并记日志）。
fn text_response(id: u64, content: &ClipContent, rich: bool, max_payload: usize) -> Value {
    let (content, dropped) = apply_limits(content.clone(), MAX_FORMAT_BYTES, MAX_LOCAL_TOTAL_BYTES);
    log_dropped(&dropped, "超过大小上限");
    let text = content.text.clone().unwrap_or_default();
    let fitted = fit_text_to_payload(&text, id, max_payload);
    let truncated = fitted.len() != text.len();
    if truncated {
        applog!("[peer] 剪贴板文本超过协议 payload 上限，已截断");
    }
    let mut response = serde_json::json!({ "type": "text", "id": id, "text": fitted });
    if truncated {
        response["truncated"] = Value::Bool(true);
    }
    if !rich {
        return response;
    }
    let mut formats = Map::new();
    for (name, value) in format_values(&content) {
        formats.insert(name.to_string(), value);
        response["formats"] = Value::Object(formats.clone());
        let too_big = serde_json::to_vec(&response)
            .map(|bytes| bytes.len() > max_payload)
            .unwrap_or(true);
        if too_big {
            formats.remove(name);
            response["formats"] = Value::Object(formats.clone());
            applog!(
                "[peer] 剪贴板格式 {name} 超出 {} MiB 协议 payload 上限，已丢弃",
                max_payload / (1024 * 1024)
            );
        }
    }
    response["formats"] = Value::Object(formats);
    response
}

/* ---------------- 能力协商 ---------------- */

/// 被控端视角的查看端能力；默认按旧版处理（只文本、16 MiB）。
#[derive(Debug, Clone, Copy)]
struct ViewerCaps {
    rich: bool,
    max_payload: usize,
}

impl Default for ViewerCaps {
    fn default() -> Self {
        Self {
            rich: false,
            max_payload: MAX_PAYLOAD_LEGACY,
        }
    }
}

/// 解析查看端 `hello` 的能力声明：声明了 `rich` 才启用富格式与更大上限，
/// 否则完全保持旧版语义（旧查看端不带这些字段）。
fn negotiate_caps(hello: &Value) -> ViewerCaps {
    let rich = hello
        .get("caps")
        .and_then(Value::as_array)
        .is_some_and(|items| items.iter().any(|item| item.as_str() == Some(CAP_RICH)));
    if !rich {
        return ViewerCaps::default();
    }
    let requested = hello
        .get("maxPayload")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or(MAX_PAYLOAD_RICH);
    ViewerCaps {
        rich: true,
        max_payload: requested.clamp(MIN_PAYLOAD, MAX_PAYLOAD_RICH),
    }
}

/// `get` 请求声明的格式列表（只保留协议认识的格式名）。
fn requested_formats(v: &Value) -> Option<Vec<String>> {
    let items = v.get("formats")?.as_array()?;
    Some(
        items
            .iter()
            .filter_map(Value::as_str)
            .filter(|name| FORMAT_NAMES.contains(name))
            .map(str::to_string)
            .collect(),
    )
}

/// 只保留请求列表里的可选格式（顶层文本不受影响）。
fn select_requested(mut content: ClipContent, requested: &[String]) -> ClipContent {
    let wants = |name: &str| requested.iter().any(|x| x == name);
    if !wants("html") {
        content.html = None;
    }
    if !wants("rtf") {
        content.rtf = None;
    }
    if !wants("png") {
        content.png = None;
    }
    if !wants("dib") {
        content.dib = None;
    }
    content
}

fn strip_optional_formats(content: &mut ClipContent) {
    content.html = None;
    content.rtf = None;
    content.png = None;
    content.dib = None;
}

/// 解析 `set` 请求：顶层 `text` 是旧版契约（优先）；`formats` 提供可选格式。
fn parse_set_request(v: &Value) -> ClipContent {
    let mut content = ClipContent::default();
    if let Some(text) = v.get("text").and_then(Value::as_str) {
        content.text = Some(text.to_string());
    }
    if let Some(obj) = v.get("formats") {
        let parsed = parse_formats(obj);
        if content.text.as_deref().map_or(true, str::is_empty) {
            content.text = parsed.text;
        }
        content.html = parsed.html;
        content.rtf = parsed.rtf;
        content.png = parsed.png;
        content.dib = parsed.dib;
    }
    let (content, dropped) = apply_limits(content, MAX_FORMAT_BYTES, MAX_LOCAL_TOTAL_BYTES);
    log_dropped(&dropped, "超过大小上限");
    content
}

/* ---------------- HTML Format（CF_HTML）头解析 / 重建 ---------------- */

/// CF_HTML 头字段（固定 10 位十进制；空值/非法偏移由解析侧兜底）。
fn html_header(
    start_html: usize,
    end_html: usize,
    start_fragment: usize,
    end_fragment: usize,
) -> String {
    format!(
        "Version:0.9\r\nStartHTML:{start_html:010}\r\nEndHTML:{end_html:010}\r\nStartFragment:{start_fragment:010}\r\nEndFragment:{end_fragment:010}\r\n"
    )
}

const HTML_PREFIX: &str = "<html><body><!--StartFragment-->";
const HTML_SUFFIX: &str = "<!--EndFragment--></body></html>";

/// 把 HTML fragment 包装成 Windows `HTML Format` 剪贴板载荷。
///
/// 头字段的 `StartHTML/EndHTML/StartFragment/EndFragment` 都按**字节**偏移
/// 重新计算（中文/emoji 不是字节数，不能按字符数算）。
fn build_html_clipboard(fragment: &str) -> String {
    let zero_header = html_header(0, 0, 0, 0);
    let start_html = zero_header.len();
    let start_fragment = start_html + HTML_PREFIX.len();
    let end_fragment = start_fragment + fragment.len();
    let end_html = end_fragment + HTML_SUFFIX.len();
    let header = html_header(start_html, end_html, start_fragment, end_fragment);
    format!("{header}{HTML_PREFIX}{fragment}{HTML_SUFFIX}")
}

/// 从 Windows `HTML Format` 载荷裁剪出 fragment。
///
/// 头字段缺失/偏移非法时依次回退到 `<!--StartFragment-->` 标记与整段原文，
/// 保证任何输入都不 panic、不丢内容。非 UTF-8 边界按 lossy 处理。
fn parse_html_clipboard(raw: &str) -> String {
    if raw.is_empty() {
        return String::new();
    }
    if let Some((start, end)) = html_fragment_range(raw) {
        return lossy_utf8_slice(raw, start, end);
    }
    if let Some(fragment) = fragment_between_markers(raw) {
        return fragment.to_string();
    }
    raw.trim_end_matches(['\0', '\r', '\n']).to_string()
}

/// 解析头字段并校验 fragment 字节区间；任何字段缺失或越界都返回 None。
fn html_fragment_range(raw: &str) -> Option<(usize, usize)> {
    let mut start_html = None;
    let mut end_html = None;
    let mut start_fragment = None;
    let mut end_fragment = None;
    // 头字段固定在前 5 行（CF_HTML 规范）；只扫描前 6 行，避免把正文里的
    // 「键:值」误当表头。
    for line in raw.split_inclusive('\n').take(6) {
        let line = line.trim_end_matches(['\r', '\n']);
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        match key {
            "StartHTML" => start_html = value.parse::<usize>().ok(),
            "EndHTML" => end_html = value.parse::<usize>().ok(),
            "StartFragment" => start_fragment = value.parse::<usize>().ok(),
            "EndFragment" => end_fragment = value.parse::<usize>().ok(),
            _ => {}
        }
    }
    let (start, end) = (start_fragment?, end_fragment?);
    let start_html = start_html?;
    end_html?;
    // 偏移必须落在正文内且不交叉；StartHTML 必须在 fragment 之前。
    if start < end && end <= raw.len() && start_html <= start && start_html <= raw.len() {
        Some((start, end))
    } else {
        None
    }
}

/// 在字节区间上取切片；边界非法时回退到 lossy 解码（不 panic）。
fn lossy_utf8_slice(raw: &str, start: usize, end: usize) -> String {
    let bytes = raw.as_bytes();
    let start = start.min(bytes.len());
    let end = end.min(bytes.len()).max(start);
    String::from_utf8_lossy(&bytes[start..end]).into_owned()
}

/// 按 `<!--StartFragment-->` / `<!--EndFragment-->` 标记取 fragment。
fn fragment_between_markers(raw: &str) -> Option<&str> {
    let start_marker = "<!--StartFragment-->";
    let end_marker = "<!--EndFragment-->";
    let start = raw.find(start_marker)? + start_marker.len();
    let end = raw[start..].find(end_marker)? + start;
    Some(&raw[start..end])
}

/* ---------------- 通道管理器 ---------------- */

struct ClipSession {
    tx: mpsc::Sender<Vec<u8>>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    cancel: CancellationToken,
    /// 本会话协商出的单帧上限（旧被控端固定 16 MiB）。
    max_payload: usize,
    /// 对端是否支持富格式（旧被控端为 false，此时只发纯文本）。
    rich: bool,
}

#[derive(Default, Clone)]
pub struct ClipboardManager {
    sessions: Arc<Mutex<HashMap<String, ClipSession>>>,
}

impl ClipboardManager {
    pub fn new() -> Self {
        Self::default()
    }

    async fn insert(&self, id: String, s: ClipSession) {
        self.sessions.lock().await.insert(id, s);
    }

    /// 发一次请求并等待同 id 的应答（5s 超时）。
    async fn request(&self, id: &str, msg: &Value) -> Option<Value> {
        let (tx, pending) = {
            let map = self.sessions.lock().await;
            let s = map.get(id)?;
            (s.tx.clone(), s.pending.clone())
        };
        let req_id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(1);
        let mut msg = msg.clone();
        if let Some(obj) = msg.as_object_mut() {
            obj.insert("id".to_string(), Value::from(req_id));
        }
        let (rtx, rrx) = oneshot::channel();
        pending.lock().await.insert(req_id, rtx);
        let Ok(bytes) = serde_json::to_vec(&msg) else {
            return None;
        };
        let mut framed = Vec::with_capacity(4 + bytes.len());
        framed.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        framed.extend_from_slice(&bytes);
        if tx.send(framed).await.is_err() {
            return None;
        }
        match tokio::time::timeout(std::time::Duration::from_secs(5), rrx).await {
            Ok(Ok(v)) => Some(v),
            _ => {
                pending.lock().await.remove(&req_id);
                None
            }
        }
    }

    /// 会话的 (单帧上限, 对端是否支持富格式)；通道已关闭返回 None。
    async fn session_params(&self, id: &str) -> Option<(usize, bool)> {
        let map = self.sessions.lock().await;
        map.get(id).map(|s| (s.max_payload, s.rich))
    }

    /// 拉取对端剪贴板；`rich=true` 时请求全部可选格式。
    ///
    /// 返回 `(内容, 文本是否被截断)`。
    async fn get(&self, id: &str, rich: bool) -> Result<(ClipContent, bool), String> {
        let Some((_, peer_rich)) = self.session_params(id).await else {
            return Err("剪贴板通道已关闭".to_string());
        };
        let msg = if rich && peer_rich {
            serde_json::json!({ "type": "get", "formats": FORMAT_NAMES })
        } else {
            serde_json::json!({ "type": "get" })
        };
        let v = self
            .request(id, &msg)
            .await
            .ok_or_else(|| "对端未响应剪贴板请求".to_string())?;
        if v.get("type").and_then(Value::as_str) != Some("text") {
            return Err(v
                .get("detail")
                .and_then(Value::as_str)
                .unwrap_or("对端拒绝读取剪贴板")
                .to_string());
        }
        Ok(content_from_text_response(&v))
    }

    /// 写入对端剪贴板；`rich=true` 时连同可选格式一起发送。
    async fn set(&self, id: &str, content: &ClipContent, rich: bool) -> Result<bool, String> {
        let Some((max_payload, peer_rich)) = self.session_params(id).await else {
            return Err("剪贴板通道已关闭".to_string());
        };
        let (content, dropped) =
            apply_limits(content.clone(), MAX_FORMAT_BYTES, MAX_LOCAL_TOTAL_BYTES);
        log_dropped(&dropped, "超过大小上限");
        if content.is_empty() {
            return Err("剪贴板内容为空".to_string());
        }
        let text = content.text.clone().unwrap_or_default();
        let mut formats = if rich && peer_rich {
            formats_to_json(&content)
        } else {
            Map::new()
        };
        let build = |text: &str, formats: &Map<String, Value>| serde_json::json!({ "type": "set", "text": text, "formats": formats });
        let mut payload = build(&text, &formats);
        let mut bytes =
            serde_json::to_vec(&payload).map_err(|e| format!("剪贴板请求序列化失败: {e}"))?;
        // 预算不够时从最低优先级开始丢可选格式；顶层文本绝不截断（宁可报错）。
        for name in ["dib", "png", "rtf", "html"] {
            if bytes.len() <= max_payload {
                break;
            }
            if formats.remove(name).is_some() {
                payload = build(&text, &formats);
                bytes = serde_json::to_vec(&payload)
                    .map_err(|e| format!("剪贴板请求序列化失败: {e}"))?;
                applog!(
                    "[peer] 剪贴板格式 {name} 超出 {} MiB 协议 payload 上限，已丢弃",
                    max_payload / (1024 * 1024)
                );
            }
        }
        if bytes.len() > max_payload {
            return Err(format!(
                "剪贴板内容过大（{} 字节，上限 {} MiB）",
                bytes.len(),
                max_payload / (1024 * 1024)
            ));
        }
        let v = self
            .request(id, &payload)
            .await
            .ok_or_else(|| "对端未响应剪贴板请求".to_string())?;
        if v.get("type").and_then(Value::as_str) != Some("ok") {
            return Err(v
                .get("detail")
                .and_then(Value::as_str)
                .unwrap_or("对端拒绝写入剪贴板")
                .to_string());
        }
        Ok(true)
    }

    /// 拉取对端剪贴板文本（旧命令 `peer_clipboard_get` 用）。
    pub async fn get_text(&self, id: &str) -> Result<String, String> {
        self.get(id, false)
            .await
            .map(|(content, _)| content.text.unwrap_or_default())
    }

    /// 写入对端剪贴板文本（旧命令 `peer_clipboard_set` 用）。
    pub async fn set_text(&self, id: &str, text: &str) -> Result<bool, String> {
        self.set(
            id,
            &ClipContent {
                text: Some(text.to_string()),
                ..ClipContent::default()
            },
            false,
        )
        .await
    }

    /// 拉取对端剪贴板富格式（新命令 `peer_clipboard_get_rich` 用）。
    pub(crate) async fn get_rich(&self, id: &str) -> Result<(ClipContent, bool), String> {
        self.get(id, true).await
    }

    /// 写入对端剪贴板富格式（新命令 `peer_clipboard_set_rich` 用）。
    pub(crate) async fn set_rich(&self, id: &str, content: &ClipContent) -> Result<bool, String> {
        self.set(id, content, true).await
    }

    pub async fn close(&self, id: &str) -> bool {
        match self.sessions.lock().await.remove(id) {
            Some(s) => {
                s.cancel.cancel();
                true
            }
            None => false,
        }
    }
}

/* ---------------- 查看端 ---------------- */

/// 为某条屏幕会话打开剪贴板通道（令牌 = 服务端下发的 auth token）。
pub async fn open(
    state: &AppState,
    screen_session_id: &str,
    token: Option<&str>,
) -> AppResult<ClipboardStart> {
    let token = token.filter(|s| !s.is_empty()).unwrap_or(screen_session_id);
    let peer_id = state
        .screen
        .peer_id_of(screen_session_id)
        .await
        .ok_or_else(|| AppError::EPeerNotPaired("屏幕会话不存在或已断开".to_string()))?;
    let peer = state
        .peer
        .peer_record(&peer_id)
        .await
        .ok_or_else(|| AppError::EPeerNotPaired("该设备未配对".to_string()))?;

    let identity = state.peer.identity_handle().await?;
    let mut handle = crate::peer::client::connect(state, &peer, "剪贴板同步").await?;
    crate::peer::client::authenticate(&mut handle, &identity).await?;

    // 实时复核（授权可能刚被撤销）
    let info = crate::peer::control::query_info(&handle, None).await?;
    state.peer.sync_peer_allows(&peer.id, &info).await;
    if !info.grant_clipboard {
        return Err(AppError::EPeerNotAllowed(format!(
            "{} 未允许剪贴板同步（可能已撤销授权）",
            peer.name
        )));
    }

    let mut channel = handle
        .channel_open_session()
        .await
        .map_err(|e| AppError::ESshChannel(format!("打开剪贴板通道失败: {e}")))?;
    channel
        .request_subsystem(true, SUBSYSTEM)
        .await
        .map_err(|e| AppError::EPeerNotAllowed(format!("对端拒绝剪贴板通道: {e}")))?;
    match tokio::time::timeout(std::time::Duration::from_secs(5), channel.wait()).await {
        Ok(Some(russh::ChannelMsg::Success)) => {}
        Ok(Some(russh::ChannelMsg::Failure)) => {
            return Err(AppError::EPeerNotAllowed(
                "对端拒绝剪贴板同步：未授权或授权已撤销".to_string(),
            ))
        }
        Ok(None) => {
            return Err(AppError::EPeerNotAllowed(
                "对端关闭了剪贴板通道".to_string(),
            ))
        }
        Err(_) => {
            return Err(AppError::EPeerNotAllowed(
                "对端未响应剪贴板请求".to_string(),
            ))
        }
        Ok(Some(_)) => {}
    }

    let stream = channel.into_stream();
    let (mut rd, mut wr) = tokio::io::split(stream);
    let cancel = CancellationToken::new();
    let id = uuid::Uuid::new_v4().to_string();
    let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>> =
        Arc::new(Mutex::new(HashMap::new()));

    // hello：带令牌 + 富格式能力声明；旧被控端忽略 caps/maxPayload。
    let hello = serde_json::json!({
        "type": "hello",
        "session": token,
        "id": 0,
        "caps": [CAP_RICH],
        "maxPayload": MAX_PAYLOAD_RICH,
    });
    let hello_bytes = serde_json::to_vec(&hello)
        .map_err(|e| AppError::EInvalidInput(format!("hello 序列化失败: {e}")))?;
    let mut framed = Vec::with_capacity(4 + hello_bytes.len());
    framed.extend_from_slice(&(hello_bytes.len() as u32).to_be_bytes());
    framed.extend_from_slice(&hello_bytes);
    wr.write_all(&framed)
        .await
        .map_err(|e| AppError::ESshChannel(format!("发送剪贴板握手失败: {e}")))?;
    let ack = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        read_frame(&mut rd, MAX_PAYLOAD_RICH),
    )
    .await;
    let (initial, initial_truncated, peer_rich, max_payload) = match ack {
        Ok(Some(body)) => {
            let v: Value = serde_json::from_slice(&body).unwrap_or_default();
            if v.get("type").and_then(Value::as_str) != Some("text") {
                let detail = v
                    .get("detail")
                    .and_then(Value::as_str)
                    .unwrap_or("对端拒绝剪贴板同步");
                return Err(AppError::EPeerNotAllowed(detail.to_string()));
            }
            // 对端在 ack 里回传 caps：没有 "rich" 说明它是旧版本，只走纯文本。
            let peer_rich = v
                .get("caps")
                .and_then(Value::as_array)
                .is_some_and(|items| items.iter().any(|item| item.as_str() == Some(CAP_RICH)));
            let max_payload = if peer_rich {
                let requested = v
                    .get("maxPayload")
                    .and_then(Value::as_u64)
                    .map(|n| n as usize)
                    .unwrap_or(MAX_PAYLOAD_RICH);
                requested.clamp(MIN_PAYLOAD, MAX_PAYLOAD_RICH)
            } else {
                MAX_PAYLOAD_LEGACY
            };
            let (content, truncated) = content_from_text_response(&v);
            (content, truncated, peer_rich, max_payload)
        }
        Ok(None) => {
            return Err(AppError::EPeerNotAllowed(
                "对端拒绝剪贴板同步：授权已撤销".to_string(),
            ))
        }
        Err(_) => {
            return Err(AppError::EPeerNotAllowed(
                "对端未响应剪贴板握手".to_string(),
            ))
        }
    };

    // 读端：分发应答 + 保持连接（handle 由该任务持有）
    {
        let cancel = cancel.clone();
        let pending = pending.clone();
        // 通道结束（对端断开 / 授权撤销 / 本端 stop）时通知前端收起「已开启」
        // 状态，并清掉管理器条目，后续 get/set 立即失败而不是等 5s 超时。
        let app = state.app.clone();
        let manager = state.clipboard.clone();
        let screen_id = screen_session_id.to_string();
        let clip_id = id.clone();
        tokio::spawn(async move {
            let _keep = handle;
            loop {
                let next = tokio::select! {
                    _ = cancel.cancelled() => None,
                    frame = read_frame(&mut rd, max_payload) => frame,
                };
                let Some(body) = next else { break };
                let Ok(v) = serde_json::from_slice::<Value>(&body) else {
                    continue;
                };
                if let Some(rid) = v.get("id").and_then(Value::as_u64) {
                    if let Some(tx) = pending.lock().await.remove(&rid) {
                        let _ = tx.send(v);
                    }
                }
            }
            pending.lock().await.clear();
            manager.close(&clip_id).await;
            crate::events::emit(
                &app,
                crate::events::PEER_CLIPBOARD,
                crate::events::ClipboardEndedPayload {
                    session_id: screen_id,
                    id: clip_id,
                },
            );
        });
    }
    // 写端
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<Vec<u8>>(16);
    {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    msg = cmd_rx.recv() => match msg {
                        Some(bytes) => { if wr.write_all(&bytes).await.is_err() { break; } }
                        None => break,
                    }
                }
            }
            let _ = wr.shutdown().await;
        });
    }

    state
        .clipboard
        .insert(
            id.clone(),
            ClipSession {
                tx: cmd_tx,
                pending,
                cancel,
                max_payload,
                rich: peer_rich,
            },
        )
        .await;
    let text_len = initial.text.as_deref().map_or(0, |t| t.chars().count());
    applog!(
        "[peer] 剪贴板同步已开启: {}（{}，文本 {} 字符）",
        peer.name,
        if peer_rich { "富格式" } else { "仅文本" },
        text_len
    );
    // 顶层 text 已经承载文本，formats 里不再重复放一份，避免大文本在 IPC 中翻倍。
    let mut formats = ClipboardRich::from_content(&initial);
    formats.text = None;
    Ok(ClipboardStart {
        id,
        text: initial.text.clone().unwrap_or_default(),
        formats: (peer_rich && formats.has_optional_formats()).then_some(formats),
        truncated: initial_truncated,
    })
}

/* ---------------- 受控端 ---------------- */

/// 受控端：服务剪贴板读写，直到通道关闭或会话失效。
///
/// 每条消息处理前复核 `grant_clipboard` 实时授权（撤销/删除设备即断开）。
pub async fn serve(
    hub: Arc<ScreenHub>,
    grant_check: crate::peer::server::GrantCheck,
    stream: russh::ChannelStream<russh::server::Msg>,
) -> AppResult<()> {
    let (mut rd, mut wr) = tokio::io::split(stream);
    let mut token = String::new();
    let mut screen_cancel: Option<CancellationToken> = None;
    let mut caps = ViewerCaps::default();
    loop {
        let next = match screen_cancel.as_ref() {
            Some(c) => tokio::select! {
                _ = c.cancelled() => None,
                f = read_frame(&mut rd, caps.max_payload) => f,
            },
            None => read_frame(&mut rd, caps.max_payload).await,
        };
        let Some(body) = next else { break };
        // 会话建立后逐条复核授权（hello 前没有会话，跳过）
        if !token.is_empty() && !grant_check.still_granted(|p| p.grant_clipboard).await {
            applog!("[peer] 剪贴板授权已撤销，断开通道");
            return Err(AppError::EPeerNotAllowed("剪贴板授权已撤销".to_string()));
        }
        let Ok(v) = serde_json::from_slice::<Value>(&body) else {
            continue;
        };
        let id = v.get("id").and_then(Value::as_u64).unwrap_or(0);
        match v.get("type").and_then(Value::as_str).unwrap_or("") {
            "hello" => {
                let s = v
                    .get("session")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if !hub.has_session(&s).await {
                    let _ = write_msg(
                        &mut wr,
                        &serde_json::json!({ "type": "error", "id": id, "detail": "查看会话不存在或已结束" }),
                    )
                    .await;
                    return Err(AppError::EPeerNotAllowed(
                        "查看会话不存在或已结束".to_string(),
                    ));
                }
                // hello 也必须实时复核授权：开通道时用的 peer 记录可能已过期，
                // 不能让已撤销剪贴板的设备靠新开通道读一次。
                if !grant_check.still_granted(|p| p.grant_clipboard).await {
                    let _ = write_msg(
                        &mut wr,
                        &serde_json::json!({ "type": "error", "id": id, "detail": "剪贴板授权已撤销" }),
                    )
                    .await;
                    return Err(AppError::EPeerNotAllowed("剪贴板授权已撤销".to_string()));
                }
                caps = negotiate_caps(&v);
                screen_cancel = hub.session_cancel(&s).await;
                token = s;
                applog!(
                    "[peer] 剪贴板通道已授权: {token}（{}，上限 {} MiB）",
                    if caps.rich { "富格式" } else { "仅文本" },
                    caps.max_payload / (1024 * 1024)
                );
                let content = platform::get_rich();
                let mut response = text_response(id, &content, caps.rich, caps.max_payload);
                // ack 额外回传能力声明，查看端据此决定是否发 formats。
                if let Some(obj) = response.as_object_mut() {
                    obj.insert("caps".to_string(), serde_json::json!([CAP_RICH]));
                    obj.insert(
                        "maxPayload".to_string(),
                        serde_json::json!(caps.max_payload),
                    );
                }
                let _ = write_msg(&mut wr, &response).await;
            }
            "get" => {
                if !hub.has_session(&token).await {
                    return Err(AppError::EPeerNotAllowed("查看会话已结束".to_string()));
                }
                let mut content = platform::get_rich();
                if caps.rich {
                    match requested_formats(&v) {
                        Some(names) => content = select_requested(content, &names),
                        None => strip_optional_formats(&mut content),
                    }
                } else {
                    strip_optional_formats(&mut content);
                }
                let response = text_response(id, &content, caps.rich, caps.max_payload);
                let _ = write_msg(&mut wr, &response).await;
            }
            "set" => {
                if !hub.has_session(&token).await {
                    return Err(AppError::EPeerNotAllowed("查看会话已结束".to_string()));
                }
                let content = parse_set_request(&v);
                let ok = !content.is_empty() && platform::set_rich(&content);
                let _ = write_msg(
                    &mut wr,
                    &serde_json::json!({
                        "type": if ok { "ok" } else { "error" },
                        "id": id,
                        "detail": if ok { "" } else { "写入对端剪贴板失败" },
                    }),
                )
                .await;
            }
            _ => {}
        }
    }
    Ok(())
}

async fn write_msg<W: AsyncWriteExt + Unpin>(wr: &mut W, v: &Value) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(v)?;
    let mut framed = Vec::with_capacity(4 + bytes.len());
    framed.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    framed.extend_from_slice(&bytes);
    wr.write_all(&framed).await
}

async fn read_frame<R: AsyncReadExt + Unpin>(rd: &mut R, limit: usize) -> Option<Vec<u8>> {
    let mut len = [0u8; 4];
    rd.read_exact(&mut len).await.ok()?;
    let n = u32::from_be_bytes(len) as usize;
    if n == 0 || n > limit {
        return None;
    }
    let mut buf = vec![0u8; n];
    rd.read_exact(&mut buf).await.ok()?;
    Some(buf)
}

/* ---------------- 本机剪贴板（IPC / 通道双方共用） ---------------- */

/// 读取本机剪贴板文本（旧命令 `peer_clipboard_local_get` 用）。
pub fn local_get() -> String {
    platform::get_text().unwrap_or_default()
}

/// 写入本机剪贴板文本（旧命令 `peer_clipboard_local_set` 用）。
pub fn local_set(text: &str) -> bool {
    local_set_content(&ClipContent {
        text: Some(text.to_string()),
        ..ClipContent::default()
    })
}

/// 读取本机剪贴板全部格式（新命令 `peer_clipboard_local_get_rich` 用）。
pub fn local_get_rich() -> ClipboardRich {
    ClipboardRich::from_content(&platform::get_rich())
}

/// 写入本机剪贴板全部格式（调用方已完成 IPC 大小校验）。
pub(crate) fn local_set_content(content: &ClipContent) -> bool {
    platform::set_rich(content)
}

/* ---------------- Windows 剪贴板 ---------------- */

#[cfg(windows)]
mod platform {
    use std::time::Duration;

    use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, RegisterClipboardFormatW,
        SetClipboardData,
    };
    use windows::Win32::System::Memory::{
        GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
    };
    use windows::Win32::System::Ole::{CF_DIB, CF_DIBV5, CF_UNICODETEXT};

    use super::{
        apply_limits, build_html_clipboard, parse_html_clipboard, sanitize_content, ClipContent,
        MAX_FORMAT_BYTES, MAX_LOCAL_TOTAL_BYTES,
    };

    /// `OpenClipboard` 失败后的退避重试次数与间隔。
    const OPEN_ATTEMPTS: usize = 3;
    const OPEN_RETRY_DELAY: Duration = Duration::from_millis(30);

    /// 打开剪贴板：系统同一时刻只允许一个进程打开，短暂占用时退避重试。
    fn open_clipboard_with_retry() -> bool {
        for attempt in 0..OPEN_ATTEMPTS {
            if unsafe { OpenClipboard(None) }.is_ok() {
                return true;
            }
            if attempt + 1 < OPEN_ATTEMPTS {
                std::thread::sleep(OPEN_RETRY_DELAY);
            }
        }
        false
    }

    /// 注册剪贴板格式名（同一名称总是返回同一 id）。
    fn register_format(name: &str) -> u32 {
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe { RegisterClipboardFormatW(windows::core::PCWSTR(wide.as_ptr())) }
    }

    /// 读取某个格式的 HGLOBAL，返回 (实际字节数, 内容)。
    ///
    /// `max` 是本次允许拷贝的字节上限（同时受单格式与本地总预算约束）；
    /// 超过上限时内容为空（调用方直接丢弃该格式），避免为超大内容再拷贝一次。
    /// 读取只拷贝，不长期锁、不 `GlobalFree`。
    fn read_hglobal(format: u32, max: usize) -> Option<(usize, Vec<u8>)> {
        unsafe {
            let handle = GetClipboardData(format).ok()?;
            if handle.0.is_null() {
                return None;
            }
            let hglobal = HGLOBAL(handle.0);
            let size = GlobalSize(hglobal);
            if size == 0 {
                return Some((0, Vec::new()));
            }
            if size > max {
                return Some((size, Vec::new()));
            }
            let ptr = GlobalLock(hglobal) as *const u8;
            if ptr.is_null() {
                return None;
            }
            let bytes = std::slice::from_raw_parts(ptr, size).to_vec();
            let _ = GlobalUnlock(hglobal);
            Some((size, bytes))
        }
    }

    /// 当前还能读取的字节数：单格式上限与「总量上限 - 已累计」取小。
    fn local_budget(total: usize) -> usize {
        MAX_FORMAT_BYTES.min(MAX_LOCAL_TOTAL_BYTES.saturating_sub(total))
    }

    /// 计入本地总量；超过总量上限返回 false（调用方丢弃该格式并记日志）。
    fn take_local(total: &mut usize, name: &str, len: usize) -> bool {
        if total.saturating_add(len) > MAX_LOCAL_TOTAL_BYTES {
            applog!(
                "[peer] 本机剪贴板格式 {name} 超过总大小上限（累计 {} 字节），已丢弃",
                total.saturating_add(len)
            );
            false
        } else {
            *total += len;
            true
        }
    }

    /// CF_UNICODETEXT（UTF-16LE，NUL 结尾）→ String；截断到第一个 NUL。
    fn utf16_bytes_to_string(bytes: &[u8]) -> String {
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .take_while(|unit| *unit != 0)
            .collect();
        String::from_utf16_lossy(&units)
    }

    /// 文本类注册格式的原始字节 → String（截断到第一个 NUL，非法 UTF-8 lossy）。
    fn lossy_cstr(raw: &[u8]) -> String {
        let end = raw.iter().position(|b| *b == 0).unwrap_or(raw.len());
        String::from_utf8_lossy(&raw[..end]).into_owned()
    }

    /// 读取 CF_UNICODETEXT（调用方已持有剪贴板）。
    fn read_text_locked(budget: usize) -> Option<String> {
        let (size, bytes) = read_hglobal(CF_UNICODETEXT.0 as u32, budget)?;
        if bytes.is_empty() {
            if size > 0 {
                applog!("[peer] 本机剪贴板文本超过剩余大小上限（{size} 字节），已丢弃");
            }
            return None;
        }
        let text = utf16_bytes_to_string(&bytes);
        if text.is_empty() {
            None
        } else {
            Some(text)
        }
    }

    /// 读取一个注册格式的文本字节（HTML Format / Rich Text Format）。
    fn read_registered_text(name: &str, budget: usize) -> Option<String> {
        let (size, raw) = read_hglobal(register_format(name), budget)?;
        if raw.is_empty() {
            if size > 0 {
                applog!("[peer] 本机剪贴板格式 {name} 超过剩余大小上限（{size} 字节），已丢弃");
            }
            return None;
        }
        let text = lossy_cstr(&raw);
        if text.is_empty() {
            None
        } else {
            Some(text)
        }
    }

    /// 读取一个注册格式的二进制字节（PNG）。
    fn read_registered_binary(name: &str, budget: usize) -> Option<Vec<u8>> {
        let (size, raw) = read_hglobal(register_format(name), budget)?;
        if raw.is_empty() {
            if size > 0 {
                applog!("[peer] 本机剪贴板格式 {name} 超过剩余大小上限（{size} 字节），已丢弃");
            }
            return None;
        }
        Some(raw)
    }

    /// 读取本机剪贴板全部格式（按 text → HTML → RTF → PNG → DIB 优先级）。
    ///
    /// 读每个格式前按剩余预算收窄拷贝上限，避免先把多个超大格式全读进内存
    /// 再裁剪；最终结果仍满足单格式 ≤ 24 MiB、总量 ≤ 32 MiB。
    pub fn get_rich() -> ClipContent {
        let mut content = ClipContent::default();
        let mut total = 0usize;
        unsafe {
            if !open_clipboard_with_retry() {
                applog!("[peer] 读取本机剪贴板失败：剪贴板被其它进程占用");
                return content;
            }
            if let Some(text) = read_text_locked(local_budget(total)) {
                if take_local(&mut total, "text", text.len()) {
                    content.text = Some(text);
                }
            }
            if let Some(html) = read_registered_text("HTML Format", local_budget(total)) {
                // 不渲染、不拉外链：只按头字段裁出 fragment 原样搬运。
                let fragment = parse_html_clipboard(&html);
                if !fragment.is_empty() && take_local(&mut total, "html", fragment.len()) {
                    content.html = Some(fragment);
                }
            }
            if let Some(rtf) = read_registered_text("Rich Text Format", local_budget(total)) {
                // RTF 原样传输，不解析（\objdata 等内嵌对象由接收端自行忽略）。
                if take_local(&mut total, "rtf", rtf.len()) {
                    content.rtf = Some(rtf);
                }
            }
            if let Some(png) = read_registered_binary("PNG", local_budget(total)) {
                if take_local(&mut total, "png", png.len()) {
                    content.png = Some(png);
                }
            }
            // DIB 原样传输（不转 PNG）：优先 v5，缺失时退回 CF_DIB。
            let dib = read_hglobal(CF_DIBV5.0 as u32, local_budget(total))
                .or_else(|| read_hglobal(CF_DIB.0 as u32, local_budget(total)));
            if let Some((size, raw)) = dib {
                if raw.is_empty() {
                    if size > 0 {
                        applog!("[peer] 本机剪贴板 DIB 超过剩余大小上限（{size} 字节），已丢弃");
                    }
                } else if take_local(&mut total, "dib", raw.len()) {
                    content.dib = Some(raw);
                }
            }
            let _ = CloseClipboard();
        }
        // 读取阶段已按预算裁剪；这里再跑一次纯函数兜底，守住「单格式/总量」不变量。
        let (content, dropped) = apply_limits(content, MAX_FORMAT_BYTES, MAX_LOCAL_TOTAL_BYTES);
        for name in dropped {
            applog!("[peer] 本机剪贴板格式 {name} 超过大小上限，已丢弃");
        }
        content
    }

    /// 读取本机剪贴板文本（旧命令用；不读取其它格式）。
    pub fn get_text() -> Option<String> {
        unsafe {
            if !open_clipboard_with_retry() {
                applog!("[peer] 读取本机剪贴板文本失败：剪贴板被其它进程占用");
                return None;
            }
            let text = read_text_locked(MAX_FORMAT_BYTES);
            let _ = CloseClipboard();
            text
        }
    }

    /// 文本 → CF_UNICODETEXT 字节（UTF-16LE，含结尾 NUL）。
    fn utf16_bytes(text: &str) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(text.len() * 2 + 2);
        for unit in text.encode_utf16().chain(std::iter::once(0)) {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        bytes
    }

    /// DIB 原始字节 → 剪贴板格式：BITMAPV5HEADER（`biSize = 124`）写 `CF_DIBV5`，
    /// 其余按 `CF_DIB` 原样写入（不重排像素、不转 PNG）。
    fn dib_format(dib: &[u8]) -> u32 {
        if dib.len() >= 4 && u32::from_le_bytes([dib[0], dib[1], dib[2], dib[3]]) == 124 {
            CF_DIBV5.0 as u32
        } else {
            CF_DIB.0 as u32
        }
    }

    /// 按内容准备好要写的 (格式 id, 原始字节)，全部在打开剪贴板前完成分配。
    fn prepare_items(content: &ClipContent) -> Vec<(u32, Vec<u8>)> {
        let mut items: Vec<(u32, Vec<u8>)> = Vec::new();
        if let Some(text) = content.text.as_deref().filter(|s| !s.is_empty()) {
            items.push((CF_UNICODETEXT.0 as u32, utf16_bytes(text)));
        }
        if let Some(html) = content.html.as_deref().filter(|s| !s.is_empty()) {
            items.push((
                register_format("HTML Format"),
                build_html_clipboard(html).into_bytes(),
            ));
        }
        if let Some(rtf) = content.rtf.as_deref().filter(|s| !s.is_empty()) {
            let mut bytes = rtf.as_bytes().to_vec();
            // CF_RTF 惯例以 NUL 结尾（部分应用按 C 字符串读取）。
            if !bytes.ends_with(&[0]) {
                bytes.push(0);
            }
            items.push((register_format("Rich Text Format"), bytes));
        }
        if let Some(png) = content.png.as_deref().filter(|b| !b.is_empty()) {
            items.push((register_format("PNG"), png.to_vec()));
        }
        if let Some(dib) = content.dib.as_deref().filter(|b| !b.is_empty()) {
            items.push((dib_format(dib), dib.to_vec()));
        }
        items
    }

    /// 拷贝到 `GlobalAlloc(GMEM_MOVEABLE)` 内存（供 SetClipboardData 转移所有权）。
    fn alloc_global_bytes(bytes: &[u8]) -> Option<HGLOBAL> {
        if bytes.is_empty() {
            return None;
        }
        unsafe {
            let hglobal = GlobalAlloc(GMEM_MOVEABLE, bytes.len()).ok()?;
            let ptr = GlobalLock(hglobal) as *mut u8;
            if ptr.is_null() {
                let _ = GlobalFree(Some(hglobal));
                return None;
            }
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
            let _ = GlobalUnlock(hglobal);
            Some(hglobal)
        }
    }

    /// 同时写入所有可用格式；成功返回 true。
    ///
    /// 写入前统一清洗 HTML/RTF：HTML 走白名单，RTF 含内嵌对象/图片时整段禁用。
    /// 所有权规则：`SetClipboardData` 成功后内存归系统所有，**绝不重复释放**；
    /// 失败时由本函数 `GlobalFree` 回收。
    pub fn set_rich(content: &ClipContent) -> bool {
        let mut sanitized = content.clone();
        sanitize_content(&mut sanitized);
        let items = prepare_items(&sanitized);
        if items.is_empty() {
            return false;
        }
        unsafe {
            if !open_clipboard_with_retry() {
                applog!("[peer] 写入本机剪贴板失败：剪贴板被其它进程占用");
                return false;
            }
            if EmptyClipboard().is_err() {
                applog!("[peer] 写入本机剪贴板失败：EmptyClipboard 失败");
                let _ = CloseClipboard();
                return false;
            }
            let mut written = 0usize;
            for (format, bytes) in items {
                let Some(hglobal) = alloc_global_bytes(&bytes) else {
                    continue;
                };
                match SetClipboardData(format, Some(HANDLE(hglobal.0))) {
                    Ok(_) => written += 1,
                    Err(e) => {
                        applog!("[peer] 写入剪贴板格式 0x{format:x} 失败: {e}");
                        let _ = GlobalFree(Some(hglobal));
                    }
                }
            }
            let _ = CloseClipboard();
            written > 0
        }
    }
}

#[cfg(not(windows))]
mod platform {
    use super::ClipContent;

    pub fn get_rich() -> ClipContent {
        ClipContent::default()
    }

    pub fn get_text() -> Option<String> {
        None
    }

    pub fn set_rich(_content: &ClipContent) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rich_all() -> ClipContent {
        ClipContent {
            text: Some("你好，剪贴板".to_string()),
            html: Some("<b>中文粗体</b>".to_string()),
            rtf: Some(r"{\rtf1\ansi 你好}".to_string()),
            png: Some(vec![0x89, b'P', b'N', b'G', 0x00, 0xFF]),
            dib: Some(vec![40, 0, 0, 0, 1, 0, 0, 0]),
        }
    }

    #[test]
    fn start_payload_is_camel_case() {
        let v = serde_json::to_value(ClipboardStart {
            id: "c1".into(),
            text: "hi".into(),
            formats: None,
            truncated: false,
        })
        .unwrap();
        assert_eq!(v["id"], "c1");
        assert_eq!(v["text"], "hi");
        assert!(v.get("formats").is_none(), "旧被控端不应带 formats");
        assert!(v.get("truncated").is_none(), "未截断时不带 truncated");
    }

    #[test]
    fn start_payload_includes_formats_when_rich() {
        let v = serde_json::to_value(ClipboardStart {
            id: "c2".into(),
            text: "hi".into(),
            formats: Some(ClipboardRich {
                text: Some("hi".into()),
                png: Some("AAAA".into()),
                ..ClipboardRich::default()
            }),
            truncated: true,
        })
        .unwrap();
        assert_eq!(v["formats"]["text"], "hi");
        assert_eq!(v["formats"]["png"], "AAAA");
        assert_eq!(v["truncated"], true);
    }

    #[test]
    fn clipboard_rich_omits_none_fields_on_serialize() {
        let v = serde_json::to_value(ClipboardRich {
            text: Some("t".into()),
            ..ClipboardRich::default()
        })
        .unwrap();
        assert_eq!(v["text"], "t");
        assert!(v.get("png").is_none());
        // 反序列化允许缺字段（前端只传文本时 host 也能处理）。
        let parsed: ClipboardRich =
            serde_json::from_value(serde_json::json!({ "html": "<b>x</b>" })).unwrap();
        assert_eq!(parsed.html.as_deref(), Some("<b>x</b>"));
        assert!(parsed.png.is_none());
    }

    #[test]
    fn clipboard_rich_roundtrips_base64_binary() {
        let content = rich_all();
        let rich = ClipboardRich::from_content(&content);
        assert_eq!(rich.png, Some(b64(&[0x89, b'P', b'N', b'G', 0x00, 0xFF])));
        assert_eq!(rich.into_content().unwrap(), content);
    }

    #[test]
    fn clipboard_rich_rejects_invalid_base64() {
        let rich = ClipboardRich {
            png: Some("这不是 base64!!".into()),
            ..ClipboardRich::default()
        };
        assert!(rich.into_content().is_err());
    }

    #[test]
    fn formats_json_roundtrip() {
        let content = rich_all();
        let map = formats_to_json(&content);
        assert!(map.get("text").is_none(), "text 由顶层字段承载");
        let response = serde_json::json!({
            "type": "text",
            "id": 7,
            "text": content.text.clone().unwrap(),
            "formats": Value::Object(map),
        });
        let (parsed, truncated) = content_from_text_response(&response);
        assert_eq!(parsed, content);
        assert!(!truncated);
    }

    #[test]
    fn parse_formats_skips_bad_entries_without_panic() {
        let obj = serde_json::json!({
            "html": 42,
            "rtf": null,
            "png": "不是 base64",
            "dib": "AAAA",
            "unknown": "ignored",
        });
        let content = parse_formats(&obj);
        assert!(content.html.is_none());
        assert!(content.rtf.is_none());
        assert!(content.png.is_none());
        assert_eq!(content.dib, Some(vec![0, 0, 0]));
    }

    #[test]
    fn apply_limits_keeps_text_and_drops_low_priority() {
        let content = ClipContent {
            text: Some("t".into()),
            html: Some("h".repeat(10)),
            rtf: Some("r".repeat(10)),
            png: Some(vec![0u8; 10]),
            dib: Some(vec![0u8; 10]),
        };
        let (kept, dropped) = apply_limits(content, 32, 11);
        assert_eq!(kept.text.as_deref(), Some("t"));
        assert!(kept.html.is_some(), "html 在总量内应保留");
        assert!(kept.rtf.is_none());
        assert!(kept.png.is_none());
        assert!(kept.dib.is_none());
        assert_eq!(dropped, vec!["rtf", "png", "dib"]);
    }

    #[test]
    fn apply_limits_drops_oversized_format_but_keeps_text() {
        let content = ClipContent {
            text: Some("t".into()),
            png: Some(vec![0u8; 64]),
            ..ClipContent::default()
        };
        let (kept, dropped) = apply_limits(content, 16, 1024);
        assert_eq!(kept.text.as_deref(), Some("t"));
        assert!(kept.png.is_none());
        assert_eq!(dropped, vec!["png"]);
    }

    #[test]
    fn text_response_legacy_has_no_formats() {
        let value = text_response(3, &rich_all(), false, MAX_PAYLOAD_LEGACY);
        assert_eq!(value["type"], "text");
        assert_eq!(value["id"], 3);
        assert!(value.get("formats").is_none(), "旧查看端不应收到 formats");
        assert!(value["text"].as_str().is_some_and(|s| !s.is_empty()));
    }

    #[test]
    fn text_response_rich_carries_formats() {
        let value = text_response(4, &rich_all(), true, MAX_PAYLOAD_RICH);
        assert_eq!(value["type"], "text");
        assert_eq!(value["formats"]["html"], "<b>中文粗体</b>");
        assert_eq!(value["formats"]["rtf"], r"{\rtf1\ansi 你好}");
        assert_eq!(
            value["formats"]["png"],
            b64(&[0x89, b'P', b'N', b'G', 0x00, 0xFF])
        );
        assert_eq!(value["formats"]["dib"], b64(&[40, 0, 0, 0, 1, 0, 0, 0]));
        assert_eq!(value["text"], "你好，剪贴板");
    }

    #[test]
    fn text_response_drops_formats_over_payload_budget() {
        let content = ClipContent {
            text: Some("hi".into()),
            png: Some(vec![0x42; 4096]),
            ..ClipContent::default()
        };
        let value = text_response(5, &content, true, 2048);
        let bytes = serde_json::to_vec(&value).unwrap();
        assert!(bytes.len() <= 2048, "响应必须落在 payload 预算内");
        assert_eq!(value["text"], "hi");
        assert!(
            value["formats"].get("png").is_none(),
            "放不下的图片格式应丢弃"
        );
    }

    #[test]
    fn text_response_truncates_text_over_payload_budget() {
        let content = ClipContent {
            text: Some("中".repeat(2000)),
            ..ClipContent::default()
        };
        let value = text_response(6, &content, false, 512);
        let bytes = serde_json::to_vec(&value).unwrap();
        assert!(bytes.len() <= 512, "响应必须落在 payload 预算内");
        let text = value["text"].as_str().unwrap_or("");
        assert!(!text.is_empty(), "截断后仍应保留可读前缀");
        assert!(text.chars().count() < 2000);
        assert_eq!(value["truncated"], true, "截断必须显式标记给查看端");
    }

    #[test]
    fn text_response_does_not_mark_short_text_truncated() {
        let value = text_response(7, &rich_all(), false, MAX_PAYLOAD_LEGACY);
        assert!(value.get("truncated").is_none());
    }

    #[test]
    fn negotiate_caps_defaults_to_legacy_for_old_hello() {
        let caps = negotiate_caps(&serde_json::json!({ "type": "hello", "session": "s" }));
        assert!(!caps.rich);
        assert_eq!(caps.max_payload, MAX_PAYLOAD_LEGACY);
    }

    #[test]
    fn negotiate_caps_clamps_requested_payload() {
        let caps = negotiate_caps(&serde_json::json!({
            "type": "hello",
            "caps": ["rich"],
            "maxPayload": 64 * 1024 * 1024,
        }));
        assert!(caps.rich);
        assert_eq!(caps.max_payload, MAX_PAYLOAD_RICH);

        let caps = negotiate_caps(&serde_json::json!({
            "type": "hello",
            "caps": ["rich"],
            "maxPayload": 1024 * 1024,
        }));
        assert_eq!(caps.max_payload, 1024 * 1024);

        let caps = negotiate_caps(&serde_json::json!({
            "type": "hello",
            "caps": ["rich", "other"],
        }));
        assert_eq!(caps.max_payload, MAX_PAYLOAD_RICH);
    }

    #[test]
    fn requested_formats_filters_unknown_names() {
        let requested = requested_formats(&serde_json::json!({
            "type": "get",
            "formats": ["text", "html", "bogus", 42, "png"],
        }))
        .unwrap();
        assert_eq!(
            requested,
            vec!["text".to_string(), "html".into(), "png".into()]
        );
        assert!(requested_formats(&serde_json::json!({ "type": "get" })).is_none());
    }

    #[test]
    fn select_requested_keeps_only_requested_formats() {
        let content = rich_all();
        let selected = select_requested(content.clone(), &["text".to_string(), "html".into()]);
        assert!(selected.text.is_some());
        assert!(selected.html.is_some());
        assert!(selected.rtf.is_none());
        assert!(selected.png.is_none());
        assert!(selected.dib.is_none());
    }

    #[test]
    fn parse_set_request_prefers_top_level_text_and_merges_formats() {
        let content = parse_set_request(&serde_json::json!({
            "type": "set",
            "text": "顶层文本",
            "formats": { "text": "格式文本", "html": "<b>富文本</b>" },
        }));
        assert_eq!(content.text.as_deref(), Some("顶层文本"));
        assert_eq!(content.html.as_deref(), Some("<b>富文本</b>"));

        let content = parse_set_request(&serde_json::json!({
            "type": "set",
            "formats": { "text": "只有格式文本" },
        }));
        assert_eq!(content.text.as_deref(), Some("只有格式文本"));

        let content = parse_set_request(&serde_json::json!({ "type": "set", "text": "旧版" }));
        assert_eq!(content.text.as_deref(), Some("旧版"));
        assert!(content.html.is_none());
    }

    // ---------- HTML Format 头解析 / 重建 ----------

    /// 从构建结果里按行读某个头字段的值（测试用独立实现，避免复用被测解析）。
    fn header_value(raw: &str, key: &str) -> usize {
        for line in raw.split("\r\n") {
            if let Some(value) = line.strip_prefix(&format!("{key}:")) {
                return value.parse().unwrap();
            }
        }
        panic!("缺少头字段 {key}");
    }

    #[test]
    fn html_build_parse_roundtrip_with_chinese() {
        let fragment = "中文 <b>粗体</b> &amp; <i>斜体</i>";
        let raw = build_html_clipboard(fragment);
        assert_eq!(parse_html_clipboard(&raw), fragment);
        // 偏移必须是字节偏移：直接按头字段切原始字节应恰好等于 fragment。
        let start = header_value(&raw, "StartFragment");
        let end = header_value(&raw, "EndFragment");
        assert_eq!(&raw.as_bytes()[start..end], fragment.as_bytes());
        assert_eq!(header_value(&raw, "EndHTML"), raw.len());
        let start_html = header_value(&raw, "StartHTML");
        assert!(raw[start_html..].starts_with("<html>"));
    }

    #[test]
    fn html_empty_fragment_roundtrip() {
        assert_eq!(parse_html_clipboard(""), "");
        let raw = build_html_clipboard("");
        assert_eq!(parse_html_clipboard(&raw), "");
    }

    #[test]
    fn html_parse_without_header_returns_raw() {
        assert_eq!(parse_html_clipboard("<b>原文</b>"), "<b>原文</b>");
        assert_eq!(parse_html_clipboard("  "), "  ");
    }

    #[test]
    fn html_parse_bad_offsets_falls_back_to_markers() {
        let fragment = "偏移坏了也要拿到 <b>这段</b>";
        let mut raw = build_html_clipboard(fragment);
        // 把 StartFragment 改成越界值（10 位定长，保持头长度不变）。
        let start = header_value(&raw, "StartFragment");
        raw = raw.replace(
            &format!("StartFragment:{start:010}"),
            "StartFragment:9999999999",
        );
        assert_eq!(parse_html_clipboard(&raw), fragment);
    }

    #[test]
    fn html_parse_empty_header_values_falls_back_to_markers() {
        let raw = concat!(
            "Version:0.9\r\n",
            "StartHTML:\r\n",
            "EndHTML:\r\n",
            "StartFragment:\r\n",
            "EndFragment:\r\n",
            "<html><body><!--StartFragment-->空值回退<!--EndFragment--></body></html>"
        );
        assert_eq!(parse_html_clipboard(raw), "空值回退");
    }

    #[test]
    fn html_parse_offset_inside_multibyte_char_does_not_panic() {
        let fragment = "中文内容";
        let raw = build_html_clipboard(fragment);
        let start = header_value(&raw, "StartFragment");
        let end = header_value(&raw, "EndFragment");
        // 把 fragment 起点移到某个汉字的中间字节：必须 lossy 处理而不是 panic。
        let broken = raw
            .replace(
                &format!("StartFragment:{start:010}"),
                &format!("StartFragment:{:010}", start + 1),
            )
            .replace(
                &format!("EndFragment:{end:010}"),
                &format!("EndFragment:{:010}", end - 1),
            );
        let parsed = parse_html_clipboard(&broken);
        assert!(!parsed.is_empty());
    }

    #[test]
    fn html_build_offsets_are_stable_for_long_content() {
        let fragment = "<p>".to_string() + &"字".repeat(5000) + "</p>";
        let raw = build_html_clipboard(&fragment);
        assert_eq!(parse_html_clipboard(&raw), fragment);
        assert_eq!(header_value(&raw, "EndHTML"), raw.len());
    }

    // ---------- 写入前清洗 ----------

    #[test]
    fn sanitize_html_keeps_formatting_and_drops_active_content() {
        let cleaned = sanitize_html(
            r#"<b onclick="x">粗体</b><script>alert(1)</script><img src="http://e/x.png"><a href="http://e">链接</a><iframe src="x"></iframe>"#,
        )
        .expect("清洗后仍有排版内容");
        assert!(cleaned.contains("<b>粗体</b>"), "排版标签应保留: {cleaned}");
        assert!(!cleaned.contains("<script"), "脚本必须移除: {cleaned}");
        assert!(!cleaned.contains("onclick"), "事件属性必须移除: {cleaned}");
        assert!(!cleaned.contains("<img"), "图片外链必须移除: {cleaned}");
        assert!(!cleaned.contains("href"), "链接必须移除: {cleaned}");
        assert!(!cleaned.contains("<iframe"), "iframe 必须移除: {cleaned}");
    }

    #[test]
    fn sanitize_html_rejects_oversized_input() {
        let oversized = format!("<b>{}</b>", "a".repeat(MAX_HTML_BYTES));
        assert!(sanitize_html(&oversized).is_none());
    }

    #[test]
    fn sanitize_rtf_drops_embedded_objects_entirely() {
        let with_object = r"{\rtf1\ansi 可见 {\*\objdata 0102} 结尾}";
        assert!(sanitize_rtf(with_object).is_none());
        let with_picture = r"{\rtf1\ansi 可见 {\pict\pngblip 89504e47} 结尾}";
        assert!(sanitize_rtf(with_picture).is_none());
        // 转义的反斜杠不是 destination，应保留原文。
        let escaped = r"{\rtf1\ansi \\pict 是文本}";
        assert!(sanitize_rtf(escaped).is_some());
        let plain = r"{\rtf1\ansi\b 普通粗体}";
        assert_eq!(sanitize_rtf(plain).as_deref(), Some(plain));
    }

    #[test]
    fn sanitize_content_applies_html_and_rtf_rules() {
        let mut content = ClipContent {
            html: Some("<script>x</script><i>保留</i>".to_string()),
            rtf: Some(r"{\rtf1 {\*\objdata 00}}".to_string()),
            ..ClipContent::default()
        };
        sanitize_content(&mut content);
        assert!(content.html.as_deref().is_some_and(|h| h.contains("<i>保留</i>")));
        assert!(content.rtf.is_none(), "含内嵌对象的 RTF 应整段丢弃");
    }

    /// 只读 smoke：不调用 Empty/Set，绝不覆盖用户剪贴板；打不开（被占用）时跳过。
    #[cfg(windows)]
    #[test]
    fn windows_clipboard_read_does_not_panic() {
        let _ = platform::get_rich();
        let _ = platform::get_text();
    }
}
