//! 配对（数字比较 / Numeric Comparison）。
//!
//! 流程：
//! 1. 双方交换设备 id/名称/指纹 + **临时公钥承诺**，随后揭示 X25519 临时公钥；
//! 2. 双方独立派生同一个 6 位码（`SHA256(sort(ephA,ephB) || idA || idB) % 1e6`）并显示；
//! 3. 两端用户各点「一致 / 不一致」；
//! 4. 用 X25519 共享密钥（HKDF-SHA256）加密互换长期 Ed25519 公钥、授权与
//!    完整 transcript 签名；
//! 5. 双方写 `peers.toml`（各自记录对方的公钥与自己的授权选择）。
//!
//! 安全性质：
//! - 临时公钥先以 SHA256 承诺（commit-reveal），对方揭示后才派生 6 位码。主动 MITM
//!   不能在看到两端临时公钥后再离线搜索碰撞码；每次连接只有 1e-6 的命中概率。
//! - 长期 Ed25519 密钥对完整握手 transcript 签名，证明持有私钥并绑定 Hello 字段
//!   （设备名 / SSH 端口 / 指纹）。
//! - 6 位码仍只有约 20 bit，因此**必须**保留承诺 + 限速；用户仍需人工核对码。
//!
//! 没有「猜码」接口；发起频率限制（同源 IP 10 分钟 3 次）只约束失败/异常尝试。

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM};
use ring::agreement::{agree_ephemeral, EphemeralPrivateKey, UnparsedPublicKey};
use ring::hkdf::{Salt, HKDF_SHA256};
use ring::rand::{SecureRandom, SystemRandom};
use russh::keys::ssh_key::{LineEnding, SshSig};
use russh::keys::{HashAlg, PublicKey};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{oneshot, Mutex, RwLock};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::events;
use crate::peer::audit::{self, AuditEntry};
use crate::peer::identity::Identity;
use crate::peer::peers::{self, PeerRecord, PeersFile};

/// 配对专用端口（独立于 SSH 的 27410 与发现的 27412）。
pub const PAIR_PORT: u16 = 28410;
/// 单帧读取预算。
const PAIR_TTL: Duration = Duration::from_secs(120);
/// 用户确认窗口（一键允许或数字比对）。
const PAIR_DECISION_TTL: Duration = Duration::from_secs(60);
/// 单次握手的总预算（确认窗口 60s + I/O 余量 60s）——防“每帧不超时”的慢速拖延。
const HANDSHAKE_BUDGET: Duration = Duration::from_secs(120);
const RATE_WINDOW: Duration = Duration::from_secs(600);
const RATE_MAX: usize = 3;
const MAX_FRAME: usize = 8 * 1024;
/// 配对协议版本：3 = 临时公钥承诺 + 长期密钥 transcript 签名 + 严格模式协商。
const PAIR_PROTOCOL_VERSION: u32 = 3;
/// 长期身份密钥签名命名空间（SSHSIG 格式）。
const PAIR_SIG_NAMESPACE: &str = "ruthenium-pair";
/// 设备名/指纹等 Hello 字段的上限，避免超长帧与 UI 异常。
const MAX_HELLO_NAME_CHARS: usize = 128;
const MAX_HELLO_ID_CHARS: usize = 160;

/// 配对授权（被请求方在弹窗里勾选，默认全 false）。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Grants {
    pub files: bool,
    pub control: bool,
    pub config: bool,
    pub screen: bool,
    pub clipboard: bool,
    pub audio: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct Decision {
    pub agree: bool,
    pub grants: Grants,
}

/// 前端事件（`peer://pairing`）。
///
/// 注意：枚举上的 `rename_all` 只改**变体名**，变体里的字段要 `rename_all_fields`
/// 才会变成 camelCase（否则前端收到 `pairing_id`，`pairingId` 为 undefined，
/// `peer_pair_confirm` 会因缺参被 IPC 拒绝）。
#[derive(Debug, Clone, Serialize)]
#[serde(
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    tag = "status"
)]
pub enum PairEvent {
    /// 需要用户确认：`numeric=true` 时核对 6 位码；否则只需确认设备身份与授权。
    Prompt {
        pairing_id: String,
        code: String,
        peer_name: String,
        peer_fp: String,
        peer_addr: String,
        direction: String,
        /// 是否要求两端核对 6 位码（严格模式/手动地址/密钥变化）。
        numeric: bool,
        /// Unix 毫秒：确认窗口截止时间（前端倒计时用；后端仍以自己的预算为准）。
        expires_at_ms: u64,
    },
    /// 发起方一键模式：等待对端允许，无弹窗（可取消）。
    Waiting {
        pairing_id: String,
        peer_name: String,
        peer_addr: String,
        expires_at_ms: u64,
    },
    Ok {
        pairing_id: String,
        peer: PeerRecord,
    },
    Rejected {
        pairing_id: String,
        reason: String,
    },
    Timeout {
        pairing_id: String,
    },
    Error {
        pairing_id: String,
        detail: String,
    },
}

struct Pending {
    tx: oneshot::Sender<Decision>,
}

/// 进行中的配对会话（按 pairingId 索引，供 `peer_pair_confirm` 落决策）。
#[derive(Default)]
pub struct PairSessions {
    map: Mutex<HashMap<String, Pending>>,
    /// 每次尝试的取消令牌：一键模式没有决策会话，取消必须能打断等待对端确认。
    cancels: Mutex<HashMap<String, CancellationToken>>,
}

impl PairSessions {
    pub fn new() -> Self {
        Self::default()
    }

    async fn register(&self, pairing_id: String) -> oneshot::Receiver<Decision> {
        let (tx, rx) = oneshot::channel();
        self.map.lock().await.insert(pairing_id, Pending { tx });
        rx
    }

    async fn register_cancel(&self, pairing_id: String, token: CancellationToken) {
        self.cancels.lock().await.insert(pairing_id, token);
    }

    async fn finish_cancel(&self, pairing_id: &str) {
        self.cancels.lock().await.remove(pairing_id);
    }

    /// 取消一次尝试（含尚未弹窗的一键等待）；返回是否命中。
    pub async fn cancel_attempt(&self, pairing_id: &str) -> bool {
        match self.cancels.lock().await.remove(pairing_id) {
            Some(t) => {
                t.cancel();
                true
            }
            None => false,
        }
    }

    /// 前端点「一致/不一致」；返回是否找到该会话。
    pub async fn decide(&self, pairing_id: &str, decision: Decision) -> bool {
        let ok = match self.map.lock().await.remove(pairing_id) {
            Some(p) => p.tx.send(decision).is_ok(),
            None => false,
        };
        // 前端「一致/不一致」是否命中会话（没命中时握手会一直等到超时）
        applog!(
            "[peer] 配对决策 id={pairing_id} agree={} 命中={ok}",
            decision.agree
        );
        ok
    }

    /// 前端取消（关闭弹窗）。
    pub async fn cancel(&self, pairing_id: &str) -> bool {
        self.map.lock().await.remove(pairing_id).is_some()
    }

    /// 服务停止时清空所有挂起会话（握手随即按“本机取消”收尾，不会占着 socket
    /// 等到超时，也不会在监听已关闭后继续写 peers.toml）。
    pub async fn cancel_all(&self) -> usize {
        let n = {
            let mut map = self.map.lock().await;
            let n = map.len();
            map.clear();
            n
        };
        let mut cancels = self.cancels.lock().await;
        for (_, t) in cancels.drain() {
            t.cancel();
        }
        n
    }
}

/// 配对上下文（服务端连接与发起方共用）。
pub struct PairCtx {
    pub app: AppHandle,
    pub identity: Identity,
    pub local_id: String,
    /// 与 PeerService 共享：设置里改设备名后，入站配对也立即使用新名字。
    pub local_name: Arc<StdMutex<String>>,
    /// 本机 SSH 监听端口（写进 Hello，供对端记录可连地址）
    pub ssh_port: u16,
    pub peers: Arc<RwLock<PeersFile>>,
    /// 与发现服务共享的已配对指纹前 8 位（配对落库后即时更新列表标记）
    pub paired_fps: Arc<Mutex<Vec<String>>>,
    pub sessions: Arc<PairSessions>,
    /// 服务级取消令牌（停止监听时取消入站/出站握手与最终落库）。
    pub cancel: CancellationToken,
    /// 所有握手任务（停止监听时等待退出，避免停止后写 peers.toml）。
    pub tasks: TaskTracker,
    /// 严格配对设置（与 PeerService 共享，设置页改动即时生效）。
    pub strict_pairing: Arc<AtomicBool>,
}

impl PairCtx {
    /// 本机是否要求数字比对（严格模式）。
    pub fn strict(&self) -> bool {
        self.strict_pairing.load(Ordering::SeqCst)
    }

    /// 本机设备名（为空时回退主机名）。
    pub fn local_name(&self) -> String {
        let name = self
            .local_name
            .lock()
            .map(|n| n.clone())
            .unwrap_or_default();
        if name.trim().is_empty() {
            std::env::var("COMPUTERNAME").unwrap_or_else(|_| "Ruthenium".to_string())
        } else {
            name.trim().to_string()
        }
    }
}

// ---------- 线协议 ----------

/// Hello 缺少 strict 字段时按“要求数字比对”处理，避免被静默降级。
fn default_strict() -> bool {
    true
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "type")]
enum PairMsg {
    Hello {
        v: u32,
        id: String,
        name: String,
        fp: String,
        /// 临时公钥承诺：base64(SHA256(side_bytes))；揭示前不暴露 eph。
        /// default 只为让缺少该字段的 Hello 能解析后给出明确的版本错误（v3 必须非空）。
        #[serde(default)]
        commit: String,
        /// 本机 SSH 端口（v3 恒有；保留 Option 以便字段缺失时明确报错）
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ssh_port: Option<u16>,
        /// 本机是否要求数字比对；缺失时按 true（安全默认）。
        #[serde(default = "default_strict")]
        strict: bool,
    },
    /// 揭示临时公钥（必须在双方交换 Hello 之后发送/接收）。
    Reveal {
        eph: String,
    },
    Confirm {
        agree: bool,
    },
    Sealed {
        nonce: String,
        ct: String,
    },
    Cancel {
        reason: String,
    },
    Error {
        reason: String,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IdentityEnvelope {
    id: String,
    name: String,
    pubkey: String,
    grants: Grants,
    /// 长期 Ed25519 私钥对握手 transcript 的签名（SSHSIG PEM）。
    sig: String,
}

async fn read_frame(stream: &mut TcpStream) -> AppResult<PairMsg> {
    let mut len = [0u8; 4];
    stream
        .read_exact(&mut len)
        .await
        .map_err(|e| AppError::EPeerPairRejected(format!("读取配对帧失败: {e}")))?;
    let n = u32::from_be_bytes(len) as usize;
    if n == 0 || n > MAX_FRAME {
        return Err(AppError::EPeerPairRejected(format!("配对帧长度非法: {n}")));
    }
    let mut buf = vec![0u8; n];
    stream
        .read_exact(&mut buf)
        .await
        .map_err(|e| AppError::EPeerPairRejected(format!("读取配对帧失败: {e}")))?;
    serde_json::from_slice(&buf)
        .map_err(|e| AppError::EPeerPairRejected(format!("配对帧解析失败: {e}")))
}

async fn write_frame(stream: &mut TcpStream, msg: &PairMsg) -> AppResult<()> {
    let bytes = serde_json::to_vec(msg)
        .map_err(|e| AppError::EPeerPairRejected(format!("序列化配对帧失败: {e}")))?;
    stream
        .write_all(&(bytes.len() as u32).to_be_bytes())
        .await
        .map_err(|e| AppError::EPeerPairRejected(format!("发送配对帧失败: {e}")))?;
    stream
        .write_all(&bytes)
        .await
        .map_err(|e| AppError::EPeerPairRejected(format!("发送配对帧失败: {e}")))
}

/// 带预算与取消的读帧：停止监听时立刻结束，不必等单帧超时。
async fn read_frame_bounded(
    stream: &mut TcpStream,
    budget: Duration,
    cancel: Option<&CancellationToken>,
    what: &str,
) -> AppResult<PairMsg> {
    let fut = tokio::time::timeout(budget, read_frame(stream));
    match cancel {
        Some(c) => tokio::select! {
            r = fut => r.map_err(|_| AppError::EPeerPairTimeout(what.to_string()))?,
            _ = c.cancelled() => Err(AppError::EPeerPairRejected("监听已停止，配对已取消".to_string())),
        },
        None => fut
            .await
            .map_err(|_| AppError::EPeerPairTimeout(what.to_string()))?,
    }
}

// ---------- 密钥派生 / 加解密 ----------

struct Key32;
impl ring::hkdf::KeyType for Key32 {
    fn len(&self) -> usize {
        32
    }
}

/// 本机对对方的授权：被请求方（incoming）用弹窗勾选；发起方（outgoing）弹窗
/// 里没有授权选项，沿用既有记录；没有旧记录则全部拒绝。发起方前端即使因
/// 状态残留传来 true，也不得被采信。
fn effective_grants(direction: &str, existing: Option<Grants>, decision: Grants) -> Grants {
    if direction == "outgoing" {
        existing.unwrap_or_default()
    } else {
        decision
    }
}

/// 配对关联模型（参考蓝牙：已绑定 → 静默复用；否则按严格程度选 Just Works / Numeric）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PairMode {
    /// 已有同指纹记录：复用绑定，不再打扰用户。
    Bonded,
    /// 一键允许（TOFU，无 MITM 防护）。
    JustWorks,
    /// 两端核对 6 位码（Numeric Comparison）。
    Numeric,
}

fn pair_mode(bonded: bool, key_changed: bool, local_strict: bool, peer_strict: bool) -> PairMode {
    // 严格模式优先于“已绑定静默”：用户开启严格配对后，任何重新配对都必须
    // 人工核对 6 位码，避免设置被 Bonded 分支悄悄绕过。
    if local_strict || peer_strict || key_changed {
        PairMode::Numeric
    } else if bonded {
        PairMode::Bonded
    } else {
        PairMode::JustWorks
    }
}

/// `last_addr` 的 IP 部分（兼容 `IP:端口` 与 `[IPv6]:端口`）。
fn addr_ip(addr: &str) -> Option<&str> {
    let addr = addr.trim();
    if let Some(rest) = addr.strip_prefix('[') {
        return rest.split(']').next();
    }
    addr.rsplit_once(':').map(|(host, _)| host)
}

/// 从已配对记录取出本机对它的授权快照。
fn grants_of(p: &PeerRecord) -> Grants {
    Grants {
        files: p.grant_files,
        control: p.grant_control,
        config: p.grant_config,
        screen: p.grant_screen,
        clipboard: p.grant_clipboard,
        audio: p.grant_audio,
    }
}

/// 6 位配对码。**两端必须得到同一个值**：临时公钥与设备 id 必须成对排序
/// （只排序公钥、id 仍按「本机→对方」拼接会让两端哈希输入顺序不同，码就不一致）。
fn derive_code(eph_a: &[u8], id_a: &str, eph_b: &[u8], id_b: &str) -> String {
    // 必须按 (eph, id) 而非仅 eph 排序：两端临时公钥意外相等时，
    // 仅比较 eph 会让 A 端与 B 端的拼接顺序不同，码就不一致。
    let (e1, i1, e2, i2) = if (eph_a, id_a) <= (eph_b, id_b) {
        (eph_a, id_a, eph_b, id_b)
    } else {
        (eph_b, id_b, eph_a, id_a)
    };
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
    ctx.update(e1);
    ctx.update(i1.as_bytes());
    ctx.update(e2);
    ctx.update(i2.as_bytes());
    let digest = ctx.finish();
    let n = u64::from_be_bytes(digest.as_ref()[..8].try_into().unwrap_or([0; 8]));
    format!("{:06}", n % 1_000_000)
}

fn derive_key(shared: &[u8]) -> AppResult<[u8; 32]> {
    let salt = Salt::new(HKDF_SHA256, b"ruthenium-pair");
    let prk = salt.extract(shared);
    let info: [&[u8]; 1] = [b"ruthenium-pair-key"];
    let okm = prk
        .expand(&info, Key32)
        .map_err(|_| AppError::EPeerPairRejected("HKDF 派生失败".to_string()))?;
    let mut key = [0u8; 32];
    okm.fill(&mut key)
        .map_err(|_| AppError::EPeerPairRejected("HKDF 填充失败".to_string()))?;
    Ok(key)
}

fn seal(key: &[u8; 32], plain: &[u8]) -> AppResult<(String, String)> {
    let rng = SystemRandom::new();
    let mut nonce = [0u8; 12];
    rng.fill(&mut nonce)
        .map_err(|_| AppError::EPeerPairRejected("生成 nonce 失败".to_string()))?;
    let unbound = UnboundKey::new(&AES_256_GCM, key)
        .map_err(|_| AppError::EPeerPairRejected("AEAD 初始化失败".to_string()))?;
    let aead = LessSafeKey::new(unbound);
    let mut buf = plain.to_vec();
    aead.seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce), Aad::empty(), &mut buf)
        .map_err(|_| AppError::EPeerPairRejected("加密失败".to_string()))?;
    Ok((STANDARD.encode(nonce), STANDARD.encode(buf)))
}

fn open(key: &[u8; 32], nonce_b64: &str, ct_b64: &str) -> AppResult<Vec<u8>> {
    let nonce: [u8; 12] = STANDARD
        .decode(nonce_b64)
        .ok()
        .and_then(|v| v.try_into().ok())
        .ok_or_else(|| AppError::EPeerPairRejected("nonce 非法".to_string()))?;
    let mut buf = STANDARD
        .decode(ct_b64)
        .map_err(|_| AppError::EPeerPairRejected("密文非法".to_string()))?;
    let unbound = UnboundKey::new(&AES_256_GCM, key)
        .map_err(|_| AppError::EPeerPairRejected("AEAD 初始化失败".to_string()))?;
    let aead = LessSafeKey::new(unbound);
    let plain = aead
        .open_in_place(Nonce::assume_unique_for_key(nonce), Aad::empty(), &mut buf)
        .map_err(|_| AppError::EPeerPairRejected("解密失败（码不一致或数据被篡改）".to_string()))?;
    Ok(plain.to_vec())
}

fn fp8_of(fp: &str) -> String {
    // 与 peers::fp_short 同口径：去掉 `SHA256:` 前缀后取 8 位（直接
    // take(8) 会得到 "SHA256:X"——7 字符是固定前缀，几乎没有区分度）。
    fp.trim_start_matches("SHA256:").chars().take(8).collect()
}

/// 一端在握手 transcript 中的全部明文声明 + 临时公钥。
#[derive(Debug, Clone)]
struct SideInfo {
    v: u32,
    id: String,
    name: String,
    fp: String,
    ssh_port: u16,
    /// 本机要求数字比对（严格模式）；参与承诺/签名，防止被中间人降级为一键模式。
    strict: bool,
    eph: Vec<u8>,
}

/// 单端字节串（承诺与 transcript 共用）。长度前缀避免字段拼接歧义。
fn side_bytes(s: &SideInfo) -> Vec<u8> {
    let mut out = Vec::with_capacity(129 + s.eph.len());
    out.extend_from_slice(b"ruthenium-pair-side-v3");
    out.extend_from_slice(&s.v.to_be_bytes());
    for field in [s.id.as_str(), s.name.as_str(), s.fp.as_str()] {
        out.extend_from_slice(&(field.len() as u32).to_be_bytes());
        out.extend_from_slice(field.as_bytes());
    }
    out.extend_from_slice(&s.ssh_port.to_be_bytes());
    out.push(u8::from(s.strict));
    out.extend_from_slice(&(s.eph.len() as u32).to_be_bytes());
    out.extend_from_slice(&s.eph);
    out
}

/// 临时公钥承诺（揭示前只发送该哈希）。
fn commit_of(s: &SideInfo) -> [u8; 32] {
    let digest = ring::digest::digest(&ring::digest::SHA256, &side_bytes(s));
    let mut out = [0u8; 32];
    out.copy_from_slice(digest.as_ref());
    out
}

/// 双方 transcript 的规范字节串：按完整 SideInfo 排序，两端得到同一顺序。
/// 长期密钥签名覆盖它；同时绑定设备名、SSH 端口与指纹，防止 Hello 字段被篡改。
fn transcript_bytes(a: &SideInfo, b: &SideInfo) -> Vec<u8> {
    // 排序键必须覆盖全部字段：仅用 (eph, id) 时，若两端碰巧同 eph 同 id
    // 但名称/端口不同，两端会各自把自己放在前面，签名 transcript 就不一致。
    let key = |s: &SideInfo| {
        (
            s.eph.clone(),
            s.id.clone(),
            s.name.clone(),
            s.fp.clone(),
            s.ssh_port,
            s.strict,
        )
    };
    let (first, second) = if key(a) <= key(b) { (a, b) } else { (b, a) };
    let mut out = Vec::new();
    out.extend_from_slice(b"ruthenium-pair-transcript-v3");
    out.extend_from_slice(&side_bytes(first));
    out.extend_from_slice(&side_bytes(second));
    out
}

fn decode_32(b64: &str, what: &str) -> AppResult<Vec<u8>> {
    let v = STANDARD
        .decode(b64)
        .map_err(|_| AppError::EPeerPairRejected(format!("{what}格式非法")))?;
    if v.len() != 32 {
        return Err(AppError::EPeerPairRejected(format!("{what}长度非法")));
    }
    Ok(v)
}

fn unix_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn format_ip_port(ip: &str, port: u16) -> String {
    if ip.contains(':') {
        format!("[{ip}]:{port}")
    } else {
        format!("{ip}:{port}")
    }
}

/// 单次配对尝试的终止事件出口：保证 Prompt 之后**恰好一条**终止事件。
struct PairAttempt {
    app: AppHandle,
    pairing_id: String,
    direction: &'static str,
    prompted: AtomicBool,
    terminal: AtomicBool,
    /// 对端展示名 / 地址（Hello 后填充，用于审计）。
    peer: StdMutex<Option<(String, String)>>,
}

enum FailKind {
    Rejected(&'static str),
    Timeout,
    Canceled,
}

impl PairAttempt {
    fn new(app: AppHandle, pairing_id: String, direction: &'static str) -> Self {
        Self {
            app,
            pairing_id,
            direction,
            prompted: AtomicBool::new(false),
            terminal: AtomicBool::new(false),
            peer: StdMutex::new(None),
        }
    }

    fn set_peer(&self, name: String, addr: String) {
        if let Ok(mut p) = self.peer.lock() {
            *p = Some((name, addr));
        }
    }

    /// 配对审计只写元数据（谁/方向/结果），不写码、密钥或授权内容。
    fn audit(&self, reason: &str) {
        let (peer_name, peer_addr) = self
            .peer
            .lock()
            .ok()
            .and_then(|p| p.clone())
            .unwrap_or_default();
        audit::append(&AuditEntry {
            at: audit::stamp(),
            kind: "pair".to_string(),
            direction: self.direction.to_string(),
            peer_name,
            peer_addr,
            quality: None,
            width: None,
            height: None,
            frames: None,
            bytes: None,
            seconds: 0,
            reason: Some(reason.to_string()),
        });
    }

    fn prompt(
        &self,
        code: String,
        peer_name: String,
        peer_fp: String,
        peer_addr: String,
        numeric: bool,
        expires_at_ms: u64,
    ) {
        self.prompted.store(true, Ordering::SeqCst);
        let _ = self.app.emit(
            events::PEER_PAIRING,
            PairEvent::Prompt {
                pairing_id: self.pairing_id.clone(),
                code,
                peer_name,
                peer_fp,
                peer_addr,
                direction: self.direction.to_string(),
                numeric,
                expires_at_ms,
            },
        );
    }

    /// 发起方一键模式：无弹窗，只提示正在等待对端允许。
    fn waiting(&self, peer_name: String, peer_addr: String, expires_at_ms: u64) {
        let _ = self.app.emit(
            events::PEER_PAIRING,
            PairEvent::Waiting {
                pairing_id: self.pairing_id.clone(),
                peer_name,
                peer_addr,
                expires_at_ms,
            },
        );
    }

    fn ok(&self, peer: PeerRecord) {
        self.terminal.store(true, Ordering::SeqCst);
        self.audit("ok");
        let _ = self.app.emit(
            events::PEER_PAIRING,
            PairEvent::Ok {
                pairing_id: self.pairing_id.clone(),
                peer,
            },
        );
    }

    /// 明确的用户/对端终止：发对应事件并返回原错误（供上层记录）。
    fn fail(&self, kind: FailKind, err: AppError) -> AppError {
        if self.terminal.swap(true, Ordering::SeqCst) {
            return err;
        }
        let (ev, reason) = match kind {
            FailKind::Rejected(reason) => (
                PairEvent::Rejected {
                    pairing_id: self.pairing_id.clone(),
                    reason: reason.to_string(),
                },
                reason,
            ),
            FailKind::Timeout => (
                PairEvent::Timeout {
                    pairing_id: self.pairing_id.clone(),
                },
                "timeout",
            ),
            FailKind::Canceled => (
                PairEvent::Rejected {
                    pairing_id: self.pairing_id.clone(),
                    reason: "canceled".to_string(),
                },
                "canceled",
            ),
        };
        self.audit(reason);
        let _ = self.app.emit(events::PEER_PAIRING, ev);
        err
    }

    /// 停止监听导致的终止：已提示过或本机发起时发 Rejected(canceled)，
    /// 入站预提示失败保持静默。
    fn canceled(&self) {
        if self.terminal.swap(true, Ordering::SeqCst) {
            return;
        }
        if !self.prompted.load(Ordering::SeqCst) && self.direction != "outgoing" {
            return;
        }
        self.audit("canceled");
        let _ = self.app.emit(
            events::PEER_PAIRING,
            PairEvent::Rejected {
                pairing_id: self.pairing_id.clone(),
                reason: "canceled".to_string(),
            },
        );
    }

    /// 非明确终止（解密/身份校验/保存失败等）：仅在用户已经看到 Prompt，
    /// 或本次是本机发起的出站配对时发 Error；入站预提示失败只记日志。
    fn error(&self, err: &AppError) {
        if self.terminal.swap(true, Ordering::SeqCst) {
            return;
        }
        if !self.prompted.load(Ordering::SeqCst) && self.direction != "outgoing" {
            return;
        }
        self.audit(&err.to_string());
        let _ = self.app.emit(
            events::PEER_PAIRING,
            PairEvent::Error {
                pairing_id: self.pairing_id.clone(),
                detail: err.to_string(),
            },
        );
    }
}

// ---------- 握手 ----------

/// 实际配对端口：默认 [`PAIR_PORT`]；测试可用 `RUTHENIUM_PAIR_PORT` 覆盖
/// （同机双实例需要不同端口）。
pub fn pair_port() -> u16 {
    std::env::var("RUTHENIUM_PAIR_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(PAIR_PORT)
}

async fn handshake(
    stream: TcpStream,
    ctx: Arc<PairCtx>,
    direction: &'static str,
    pairing_id: String,
    manual_strict: bool,
) -> AppResult<()> {
    let attempt = PairAttempt::new(ctx.app.clone(), pairing_id.clone(), direction);
    // 每次尝试一个子令牌：一键模式没有决策会话，前端取消也能立刻打断等待。
    let attempt_cancel = ctx.cancel.child_token();
    ctx.sessions
        .register_cancel(pairing_id.clone(), attempt_cancel.clone())
        .await;
    let result = handshake_inner(
        stream,
        ctx.clone(),
        direction,
        pairing_id.clone(),
        &attempt_cancel,
        manual_strict,
        &attempt,
    )
    .await;
    ctx.sessions.finish_cancel(&pairing_id).await;
    if let Err(e) = &result {
        // 停止监听/用户取消导致的读帧/写帧错误统一按“已取消”收尾，而不是“配对失败”。
        if ctx.cancel.is_cancelled() || attempt_cancel.is_cancelled() {
            attempt.canceled();
        } else {
            attempt.error(e);
        }
    }
    result
}

async fn handshake_inner(
    mut stream: TcpStream,
    ctx: Arc<PairCtx>,
    direction: &'static str,
    pairing_id: String,
    cancel: &CancellationToken,
    manual_strict: bool,
    attempt: &PairAttempt,
) -> AppResult<()> {
    let canceled = || cancel.is_cancelled() || ctx.cancel.is_cancelled();
    let stop_err = || AppError::EPeerPairRejected("监听已停止，配对已取消".to_string());
    if canceled() {
        return Err(stop_err());
    }
    // 整个握手的总预算。PAIR_TTL 只包单次 read_frame，恶意对端可以
    // “每帧刚好不超时”拖住本机 task 与连接；总预算从进入本函数起算。
    let total_deadline = tokio::time::Instant::now() + HANDSHAKE_BUDGET;
    // 对端可连地址：连接来源 IP + 对端 Hello 里声明的 SSH 端口
    // （不能直接用 socket 的源端口——那是临时端口，重连时早已失效）
    let peer_ip = stream.peer_addr().ok().map(|a| a.ip().to_string());
    let rng = SystemRandom::new();
    let eph = EphemeralPrivateKey::generate(&ring::agreement::X25519, &rng)
        .map_err(|_| AppError::EPeerPairRejected("生成临时密钥失败".to_string()))?;
    let eph_pub = eph
        .compute_public_key()
        .map_err(|_| AppError::EPeerPairRejected("计算临时公钥失败".to_string()))?
        .as_ref()
        .to_vec();

    let local_fp = fp8_of(&ctx.identity.fingerprint());
    let local_name = ctx.local_name();
    // 手动地址默认严格（地址来源不可信）；设置里开启后所有配对都严格。
    let local_strict = manual_strict || ctx.strict();
    let local_side = SideInfo {
        v: PAIR_PROTOCOL_VERSION,
        id: ctx.local_id.clone(),
        name: local_name.clone(),
        fp: local_fp.clone(),
        ssh_port: ctx.ssh_port,
        strict: local_strict,
        eph: eph_pub.clone(),
    };
    let my_hello = PairMsg::Hello {
        v: PAIR_PROTOCOL_VERSION,
        id: ctx.local_id.clone(),
        name: local_name.clone(),
        fp: local_fp,
        commit: STANDARD.encode(commit_of(&local_side)),
        ssh_port: Some(ctx.ssh_port),
        strict: local_strict,
    };
    // 双方都先发 Hello 再收 Hello（全双工，不会死锁）
    write_frame(&mut stream, &my_hello).await?;
    let frame_budget = total_deadline
        .saturating_duration_since(tokio::time::Instant::now())
        .min(PAIR_TTL);
    let peer_hello = read_frame_bounded(
        &mut stream,
        frame_budget,
        Some(cancel),
        "等待对方配对请求超时",
    )
    .await?;
    let (peer_id, peer_name, peer_fp, peer_ssh_port, peer_commit, peer_strict, mut peer_side) =
        match peer_hello {
            PairMsg::Hello {
                v,
                id,
                name,
                fp,
                commit,
                ssh_port,
                strict,
            } => {
                if v != PAIR_PROTOCOL_VERSION {
                    return Err(AppError::EPeerPairRejected(format!(
                        "不支持的配对协议版本 {v}（需要 {PAIR_PROTOCOL_VERSION}，请双方升级）"
                    )));
                }
                if id.is_empty() || id.chars().count() > MAX_HELLO_ID_CHARS {
                    return Err(AppError::EPeerPairRejected("对方设备 id 非法".to_string()));
                }
                if name.chars().count() > MAX_HELLO_NAME_CHARS {
                    return Err(AppError::EPeerPairRejected("对方设备名过长".to_string()));
                }
                if fp.is_empty() || fp.chars().count() > 16 {
                    return Err(AppError::EPeerPairRejected("对方指纹前缀非法".to_string()));
                }
                let commit: [u8; 32] =
                    decode_32(&commit, "对方临时公钥承诺")?
                        .try_into()
                        .map_err(|_| {
                            AppError::EPeerPairRejected("对方临时公钥承诺长度非法".to_string())
                        })?;
                let ssh_port = ssh_port.filter(|p| *p != 0).unwrap_or(27410);
                let side = SideInfo {
                    v,
                    id: id.clone(),
                    name: name.clone(),
                    fp: fp.clone(),
                    ssh_port,
                    strict,
                    eph: Vec::new(),
                };
                (id, name, fp, ssh_port, commit, strict, side)
            }
            PairMsg::Cancel { reason } => {
                let _ = write_frame(
                    &mut stream,
                    &PairMsg::Cancel {
                        reason: reason.clone(),
                    },
                )
                .await;
                return Err(AppError::EPeerPairRejected(format!("对方取消: {reason}")));
            }
            _ => return Err(AppError::EPeerPairRejected("配对握手顺序错误".to_string())),
        };

    // 自配对没有意义（也容易把本机当成“设备”写进 peers.toml）
    if peer_id == ctx.local_id {
        return Err(AppError::EPeerPairRejected(
            "不能与本机配对（对方设备 id 与本机身份相同）".to_string(),
        ));
    }
    attempt.set_peer(
        peer_name.clone(),
        peer_ip
            .as_deref()
            .map(|ip| format_ip_port(ip, peer_ssh_port))
            .unwrap_or_default(),
    );

    // commit-reveal：双方都先发 Reveal，再读对方 Reveal；揭示后才校验承诺。
    if canceled() {
        return Err(stop_err());
    }
    write_frame(
        &mut stream,
        &PairMsg::Reveal {
            eph: STANDARD.encode(&eph_pub),
        },
    )
    .await?;
    let frame_budget = total_deadline
        .saturating_duration_since(tokio::time::Instant::now())
        .min(PAIR_TTL);
    let peer_reveal = read_frame_bounded(
        &mut stream,
        frame_budget,
        Some(cancel),
        "等待对方揭示临时公钥超时",
    )
    .await?;
    let peer_eph = match peer_reveal {
        PairMsg::Reveal { eph } => decode_32(&eph, "对方临时公钥")?,
        PairMsg::Cancel { reason } => {
            return Err(AppError::EPeerPairRejected(format!("对方取消: {reason}")));
        }
        PairMsg::Error { reason } => {
            return Err(AppError::EPeerPairRejected(format!("对方拒绝: {reason}")));
        }
        _ => return Err(AppError::EPeerPairRejected("配对揭示顺序错误".to_string())),
    };
    peer_side.eph = peer_eph.clone();
    if commit_of(&peer_side) != peer_commit {
        return Err(AppError::EPeerPairRejected(
            "对方临时公钥与承诺不一致（疑似中间人，已拒绝）".to_string(),
        ));
    }

    let code = derive_code(&eph_pub, &ctx.local_id, &peer_eph, &peer_id);
    // 监听可能刚被关闭：不要再弹配对确认框
    if canceled() {
        return Err(stop_err());
    }

    // Bonding：已有同 id 且同指纹的记录 → 静默刷新地址/授权，不再打扰用户。
    // 仍需后面的 transcript 签名校验：没有长期私钥的冒充者无法走到落库。
    let (existing, prior) = {
        let file = ctx.peers.read().await;
        let existing = file.peers.iter().find(|p| p.id == peer_id).cloned();
        // 设备换钥后 id（完整指纹）会变，不能靠 id 命中旧记录；用名称/来源 IP
        // 找“很可能是同一台设备”的旧记录，强制数字比对。误判只会多弹一次确认，
        // 不会被用来绕过信任（真正落库仍要 transcript 签名校验）。
        let prior = file
            .peers
            .iter()
            .find(|p| {
                p.id != peer_id
                    && ((!peer_name.trim().is_empty() && p.name == peer_name)
                        || peer_ip
                            .as_deref()
                            .is_some_and(|ip| p.last_addr.as_deref().and_then(addr_ip) == Some(ip)))
            })
            .cloned();
        (existing, prior)
    };
    let existing_fp = existing.as_ref().map(|p| peers::fp_short(&p.pubkey));
    let bonded = !peer_fp.is_empty() && existing_fp.as_deref() == Some(peer_fp.as_str());
    // 同名/同来源 IP 但 id 不同，或同 id 但指纹不符：视为换钥/记录被改，必须人工确认。
    let key_changed = prior.is_some() || (existing.is_some() && !bonded);
    let mode = pair_mode(bonded, key_changed, local_strict, peer_strict);
    let numeric = mode == PairMode::Numeric;

    let decision_budget = total_deadline
        .saturating_duration_since(tokio::time::Instant::now())
        .min(PAIR_DECISION_TTL);
    let expires_at_ms = unix_ms_now() + decision_budget.as_millis() as u64;
    let peer_addr = peer_ip.clone().unwrap_or_default();

    let decision = if mode == PairMode::Bonded {
        applog!("[peer] 已绑定设备重新配对（静默刷新）: {peer_name}");
        Decision {
            agree: true,
            grants: Grants::default(),
        }
    } else if mode == PairMode::JustWorks && direction == "outgoing" {
        // 一键模式：发起方不弹窗，只等对端允许（可取消）。
        attempt.waiting(peer_name.clone(), peer_addr.clone(), expires_at_ms);
        Decision {
            agree: true,
            grants: Grants::default(),
        }
    } else {
        let rx = ctx.sessions.register(pairing_id.clone()).await;
        // 注册与 stop 的 cancel_all 可能交错：再检查一次，避免监听停止后仍弹出提示。
        if canceled() {
            ctx.sessions.cancel(&pairing_id).await;
            return Err(stop_err());
        }
        attempt.prompt(
            code,
            peer_name.clone(),
            peer_fp.clone(),
            peer_addr,
            numeric,
            expires_at_ms,
        );
        let waited = tokio::select! {
            r = tokio::time::timeout(decision_budget, rx) => r,
            _ = cancel.cancelled() => {
                ctx.sessions.cancel(&pairing_id).await;
                let _ = write_frame(
                    &mut stream,
                    &PairMsg::Cancel { reason: "stopped".to_string() },
                )
                .await;
                return Err(attempt.fail(FailKind::Canceled, stop_err()));
            }
        };
        match waited {
            Ok(Ok(d)) => d,
            Ok(Err(_)) => Decision {
                agree: false,
                grants: Grants::default(),
            },
            Err(_) => {
                // 超时：清掉挂起会话，避免条目滞留（用户随后关弹窗也不会再命中）
                ctx.sessions.cancel(&pairing_id).await;
                let _ = write_frame(
                    &mut stream,
                    &PairMsg::Cancel {
                        reason: "timeout".to_string(),
                    },
                )
                .await;
                return Err(attempt.fail(
                    FailKind::Timeout,
                    AppError::EPeerPairTimeout("配对确认超时".to_string()),
                ));
            }
        }
    };

    if !decision.agree {
        let _ = write_frame(
            &mut stream,
            &PairMsg::Cancel {
                reason: "user".to_string(),
            },
        )
        .await;
        return Err(attempt.fail(
            FailKind::Rejected("canceled"),
            AppError::EPeerPairRejected("本机取消了配对".to_string()),
        ));
    }

    write_frame(&mut stream, &PairMsg::Confirm { agree: true }).await?;

    // 等对方确认
    let frame_budget = total_deadline
        .saturating_duration_since(tokio::time::Instant::now())
        .min(PAIR_TTL);
    let peer_msg =
        read_frame_bounded(&mut stream, frame_budget, Some(cancel), "等待对方确认超时").await?;
    match peer_msg {
        PairMsg::Confirm { agree: true } => { /* 继续 */ }
        PairMsg::Confirm { agree: false } | PairMsg::Cancel { .. } => {
            return Err(attempt.fail(
                FailKind::Rejected("peer_declined"),
                AppError::EPeerPairRejected("对方未确认配对".to_string()),
            ));
        }
        _ => return Err(AppError::EPeerPairRejected("配对确认顺序错误".to_string())),
    }

    // 交换长期公钥（加密）
    let shared = {
        let peer_key = UnparsedPublicKey::new(&ring::agreement::X25519, peer_eph.as_slice());
        agree_ephemeral(eph, &peer_key, |secret| secret.to_vec())
            .map_err(|_| AppError::EPeerPairRejected("密钥协商失败".to_string()))?
    };
    let key = derive_key(&shared)?;

    // 本机对对方的授权：只有静默 Bonded 保持原样；明确确认（一键/严格/换钥）时
    // 发起方沿用既有记录、被请求方以弹窗为准。必须在构造信封之前计算，否则发给
    // 对方的 peer_allows_* 快照会被清零。
    let grants = if mode == PairMode::Bonded {
        // 静默刷新：不打扰用户，也不改变原有授权。
        existing.as_ref().map(grants_of).unwrap_or_default()
    } else {
        // 明确确认（含严格模式下的已绑定设备）：被请求方以本次弹窗勾选为准；
        // 发起方仍沿用既有本地授权。
        let existing_grants = {
            let file = ctx.peers.read().await;
            file.peers.iter().find(|p| p.id == peer_id).map(grants_of)
        };
        // 换钥后 peer_id 会变，按 id 找不到旧记录；但 prior 已按设备名/来源 IP
        // 找到“很可能是同一台设备”的旧记录。发起方应沿用它，否则一次换钥
        // 会把既有的本地授权全部清空。
        let outgoing_grants = if direction == "outgoing" && existing_grants.is_none() {
            prior.as_ref().map(grants_of)
        } else {
            None
        };
        effective_grants(
            direction,
            existing_grants.or(outgoing_grants),
            decision.grants,
        )
    };

    let transcript = transcript_bytes(&local_side, &peer_side);
    let sig = ctx
        .identity
        .private_key()
        .sign(PAIR_SIG_NAMESPACE, HashAlg::Sha256, &transcript)
        .map_err(|e| AppError::EPeerPairRejected(format!("生成本机握手签名失败: {e}")))?;
    let sig = sig
        .to_pem(LineEnding::LF)
        .map_err(|e| AppError::EPeerPairRejected(format!("编码本机握手签名失败: {e}")))?;
    let envelope = IdentityEnvelope {
        id: ctx.local_id.clone(),
        name: local_name,
        pubkey: ctx.identity.public_line(),
        grants,
        sig,
    };
    let plain = serde_json::to_vec(&envelope)
        .map_err(|e| AppError::EPeerPairRejected(format!("序列化身份失败: {e}")))?;
    let (nonce, ct) = seal(&key, &plain)?;
    write_frame(&mut stream, &PairMsg::Sealed { nonce, ct }).await?;

    let frame_budget = total_deadline
        .saturating_duration_since(tokio::time::Instant::now())
        .min(PAIR_TTL);
    let peer_sealed =
        read_frame_bounded(&mut stream, frame_budget, Some(cancel), "等待对方身份超时").await?;
    let peer_env: IdentityEnvelope = match peer_sealed {
        PairMsg::Sealed { nonce, ct } => {
            let plain = open(&key, &nonce, &ct)?;
            serde_json::from_slice(&plain)
                .map_err(|e| AppError::EPeerPairRejected(format!("对方身份解析失败: {e}")))?
        }
        PairMsg::Cancel { .. } | PairMsg::Error { .. } => {
            return Err(AppError::EPeerPairRejected(
                "对方在交换身份时退出".to_string(),
            ));
        }
        _ => return Err(AppError::EPeerPairRejected("配对交换顺序错误".to_string())),
    };

    // 身份绑定校验——加密信封里的设备 id 必须与握手 Hello 声明的一致，
    // 且信封公钥的指纹前 8 位要与 Hello.fp 相符。
    if peer_env.id != peer_id {
        return Err(AppError::EPeerPairRejected(format!(
            "对方身份不一致：握手声明 {peer_id}，信封携带 {}（疑似中间人，已拒绝）",
            peer_env.id
        )));
    }
    let env_fp8 = peers::fp_short(&peer_env.pubkey);
    if env_fp8.is_empty() {
        return Err(AppError::EPeerPairRejected(
            "对方公钥无法解析（已拒绝）".to_string(),
        ));
    }
    if env_fp8 != peer_fp {
        return Err(AppError::EPeerPairRejected(
            "对方公钥与握手声明不一致（疑似中间人，已拒绝）".to_string(),
        ));
    }
    // 名字不被 transcript 签名覆盖（签名只覆盖 Hello 里的 SideInfo）；必须
    // 与 Hello 声明一致，否则用户看到的设备名与落库/审计记录可能不同。
    if peer_env.name != peer_side.name {
        return Err(AppError::EPeerPairRejected(
            "对方设备名在握手与身份信封中不一致（已拒绝）".to_string(),
        ));
    }
    // 设备 id 的契约是「身份公钥的完整 SHA256 指纹」。
    let peer_pub = PublicKey::from_openssh(&peer_env.pubkey)
        .map_err(|_| AppError::EPeerPairRejected("对方公钥无法解析（已拒绝）".to_string()))?;
    let sealed_fp = peer_pub.fingerprint(HashAlg::Sha256).to_string();
    if sealed_fp != peer_id {
        return Err(AppError::EPeerPairRejected(format!(
            "对方设备 id 不是其公钥指纹：声明 {peer_id}，实际 {sealed_fp}（疑似中间人，已拒绝）"
        )));
    }
    // 长期密钥持有证明：签名覆盖完整 transcript（双方设备名/端口/指纹/临时公钥），
    // 既证明对方持有信封公钥的私钥，也防止 Hello 字段在传输中被篡改。
    let peer_sig = SshSig::from_pem(peer_env.sig.as_bytes())
        .map_err(|_| AppError::EPeerPairRejected("对方握手签名格式非法".to_string()))?;
    peer_pub
        .verify(PAIR_SIG_NAMESPACE, &transcript, &peer_sig)
        .map_err(|_| {
            AppError::EPeerPairRejected("对方握手签名校验失败（疑似中间人，已拒绝）".to_string())
        })?;

    // 停止监听后不得再写 peers.toml（入站/出站同一条规则）。
    if canceled() {
        return Err(attempt.fail(FailKind::Canceled, stop_err()));
    }

    let now = crate::session_log::LogStamp::now();
    let last_addr = peer_ip.map(|ip| format_ip_port(&ip, peer_ssh_port));
    let record = PeerRecord {
        id: peer_env.id.clone(),
        name: peer_env.name.clone(),
        pubkey: peer_env.pubkey.clone(),
        last_addr,
        paired_at: format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            now.year, now.month, now.day, now.hour, now.minute, now.second
        ),
        last_seen: None,
        grant_files: grants.files,
        grant_control: grants.control,
        grant_config: grants.config,
        grant_screen: grants.screen,
        grant_clipboard: grants.clipboard,
        grant_audio: grants.audio,
        peer_allows_files: peer_env.grants.files,
        peer_allows_control: peer_env.grants.control,
        peer_allows_config: peer_env.grants.config,
        peer_allows_screen: peer_env.grants.screen,
        peer_allows_clipboard: peer_env.grants.clipboard,
        peer_allows_audio: peer_env.grants.audio,
    };

    // 先落盘、成功后再替换内存：保存失败时 SSH 服务不得提前信任该设备。
    {
        let mut file = ctx.peers.write().await;
        let mut next = file.clone();
        peers::upsert_in(&mut next, record.clone());
        peers::save(&next)?;
        *file = next;
    }
    // 发现列表的「已配对」标记即时生效（否则要等发现服务重启）
    {
        let fp = peers::fp_short(&record.pubkey);
        if !fp.is_empty() {
            let mut fps = ctx.paired_fps.lock().await;
            if !fps.iter().any(|p| p == &fp) {
                fps.push(fp);
            }
        }
    }

    applog!(
        "[peer] 配对完成: {}（{}）grants files={} control={} config={}",
        record.name,
        peer_fp,
        record.grant_files,
        record.grant_control,
        record.grant_config
    );
    attempt.ok(record);
    Ok(())
}

// ---------- 服务端（被请求方） ----------

async fn allowed(rate: &Arc<Mutex<HashMap<IpAddr, Vec<Instant>>>>, ip: IpAddr) -> bool {
    let mut map = rate.lock().await;
    // 长期运行会累积无数来源 IP：条目多时顺手清掉已过期的空桶
    if map.len() > 64 {
        map.retain(|_, v| {
            v.retain(|t| t.elapsed() < RATE_WINDOW);
            !v.is_empty()
        });
    }
    let entry = map.entry(ip).or_default();
    entry.retain(|t| t.elapsed() < RATE_WINDOW);
    if entry.len() >= RATE_MAX {
        return false;
    }
    entry.push(Instant::now());
    true
}

/// 握手成功：把刚才占用的限速额度还回去（限速应只约束失败/异常尝试，
/// 否则正常给第 4 台设备配对会被自己的成功记录挡住）。
async fn rate_success(rate: &Arc<Mutex<HashMap<IpAddr, Vec<Instant>>>>, ip: IpAddr) {
    let mut map = rate.lock().await;
    if let Some(v) = map.get_mut(&ip) {
        v.pop();
        if v.is_empty() {
            map.remove(&ip);
        }
    }
}

/// 绑定配对监听端口：默认端口被占用时在同段内退避（与 SSH 监听一致）。
pub async fn bind_listener() -> AppResult<(TcpListener, u16)> {
    let base = pair_port();
    let last = base.saturating_add(9);
    let mut err = String::new();
    for port in base..=last {
        match TcpListener::bind(("0.0.0.0", port)).await {
            Ok(l) => return Ok((l, port)),
            Err(e) => err = format!("{port}: {e}"),
        }
    }
    Err(AppError::EPeerListen(format!(
        "配对端口 {base}–{last} 均不可用（{err}）"
    )))
}

/// 配对服务端 accept 循环（与 SSH 监听同生命周期）。
///
/// 监听 socket 由调用方提前绑定，这样发现报文广播的是**实际**端口。
pub async fn serve(
    listener: TcpListener,
    bound_port: u16,
    ctx: Arc<PairCtx>,
    cancel: CancellationToken,
) -> AppResult<()> {
    applog!("[peer] 配对服务监听 0.0.0.0:{bound_port}");
    let rate: Arc<Mutex<HashMap<IpAddr, Vec<Instant>>>> = Arc::new(Mutex::new(HashMap::new()));
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            accepted = listener.accept() => {
                let (stream, addr) = match accepted {
                    Ok(v) => v,
                    Err(e) => {
                        applog!("[peer] 配对 accept 失败: {e}");
                        continue;
                    }
                };
                if !allowed(&rate, addr.ip()).await {
                    applog!("[peer] 配对请求被限速: {}", addr.ip());
                    drop(stream);
                    continue;
                }
                let ctx = ctx.clone();
                let rate = rate.clone();
                // 由服务级 TaskTracker 跟踪：stop 时取消令牌 + 等待所有握手退出，
                // 避免停止监听后仍有任务写 peers.toml。
                let tasks = ctx.tasks.clone();
                tasks.spawn(async move {
                    let id = uuid::Uuid::new_v4().to_string();
                    // 入站是否严格由本机设置决定（manual=false）。
                    let result = handshake(stream, ctx.clone(), "incoming", id, false).await;
                    match result {
                        Ok(()) => rate_success(&rate, addr.ip()).await,
                        Err(e) => applog!("[peer] 配对（入站）结束: {e}"),
                    }
                });
            }
        }
    }
    applog!("[peer] 配对服务已停止");
    Ok(())
}

// ---------- 发起方（本机发起配对） ----------

/// 解析配对目标：`IP` / `host` / `IP:端口` / `[IPv6]:端口`。
///
/// 返回 `(host, 显式端口)`：裸 IP/主机名（包括未加方括号的 IPv6）没有端口，
/// 由调用方在默认端口段内探测（对方默认端口可能被占用而退避）。
/// 只有最后一段能解析为端口时才按 `host:port` 拆（否则 `fe80::1` 会被误拆成
/// host=`fe80:`、port=`1`）。
fn parse_target(target: &str) -> (String, Option<u16>) {
    let t = target.trim();
    if t.parse::<IpAddr>().is_ok() {
        return (t.to_string(), None);
    }
    match t.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() => match p.parse::<u16>() {
            Ok(port) => (h.trim_matches(&['[', ']'][..]).to_string(), Some(port)),
            Err(_) => (t.trim_matches(&['[', ']'][..]).to_string(), None),
        },
        _ => (t.trim_matches(&['[', ']'][..]).to_string(), None),
    }
}

/// 同机 e2e 钩子：调试构建下把“手填地址”也按发现列表处理（一键模式）。
/// 发布构建永远返回 false；不影响严格设置与入站策略。
fn justworks_test_override() -> bool {
    cfg!(debug_assertions) && std::env::var("RUTHENIUM_PAIR_JUSTWORKS").is_ok_and(|v| v == "1")
}

/// 发起配对：连接对方配对端口，握手在后台任务里进行（结果走事件）。
///
/// `manual=true` 表示地址是手填的（来源不可信），强制走数字比对；
/// 从发现列表发起的配对走一键允许（Just Works）。
pub async fn pair_with(target: &str, ctx: Arc<PairCtx>, manual: bool) -> AppResult<String> {
    let manual = manual && !justworks_test_override();
    let (host, explicit_port) = parse_target(target);
    // 目标可以是 IP 或主机名（占位符写的是 “IP / host name”）；主机名走系统解析。
    // 解析可能返回多个地址（如 localhost 的 ::1 + 127.0.0.1）：IPv4 优先，
    // 因为配对监听绑定的是 0.0.0.0。
    let mut ips: Vec<IpAddr> = match host.parse::<IpAddr>() {
        Ok(ip) => vec![ip],
        Err(_) => {
            let resolved = tokio::time::timeout(
                Duration::from_secs(3),
                tokio::net::lookup_host((host.as_str(), 0)),
            )
            .await
            .map_err(|_| AppError::EPeerOffline(format!("解析主机名 {host} 超时")))?
            .map_err(|e| AppError::EPeerOffline(format!("解析主机名 {host} 失败: {e}")))?;
            resolved.map(|a| a.ip()).collect()
        }
    };
    if ips.is_empty() {
        return Err(AppError::EPeerOffline(format!("无法解析主机名 {host}")));
    }
    ips.sort_by_key(|ip| !ip.is_ipv4());
    ips.dedup();
    // 显式端口只试一次；裸目标在默认端口段内探测（对方默认端口被占用时会退避）。
    let base = pair_port();
    let ports: Vec<u16> = match explicit_port {
        Some(0) => return Err(AppError::EInvalidInput(format!("配对端口非法: {target}"))),
        Some(p) => vec![p],
        None => (base..=base.saturating_add(9)).collect(),
    };
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut last = String::new();
    let mut connected = None;
    'ips: for ip in ips {
        for &port in &ports {
            let remain = deadline.saturating_duration_since(Instant::now());
            if remain.is_zero() {
                break 'ips;
            }
            let addr = SocketAddr::new(ip, port);
            let attempt = remain.min(Duration::from_millis(1500));
            match tokio::time::timeout(attempt, TcpStream::connect(addr)).await {
                Ok(Ok(stream)) => {
                    applog!("[peer] 配对目标 {target} 命中 {addr}");
                    connected = Some(stream);
                    break 'ips;
                }
                Ok(Err(e)) => {
                    applog!("[peer] 配对端口探测 {addr} 失败: {e}");
                    last = format!("{addr}: {e}");
                }
                Err(_) => {
                    applog!("[peer] 配对端口探测 {addr} 超时");
                    last = format!("{addr}: 超时");
                }
            }
        }
    }
    let Some(stream) = connected else {
        return Err(AppError::EPeerOffline(format!(
            "连接 {target} 失败（{last}）"
        )));
    };
    // 连接期间监听可能已停止：已关闭的 TaskTracker 会直接丢弃新任务，
    // 这里明确返回错误，避免前端只拿到 pairingId 却永远收不到事件。
    if ctx.cancel.is_cancelled() || ctx.tasks.is_closed() {
        return Err(AppError::EPeerWindowClosed(
            "局域网已停止，配对已取消".to_string(),
        ));
    }
    let pairing_id = Uuid::new_v4().to_string();
    let ctx2 = ctx.clone();
    let id2 = pairing_id.clone();
    // 出站也必须受服务级取消令牌约束：停止监听后不得继续握手/写库。
    ctx.tasks.spawn(async move {
        if let Err(e) = handshake(stream, ctx2.clone(), "outgoing", id2, manual).await {
            applog!("[peer] 配对（出站）结束: {e}");
        }
    });
    Ok(pairing_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 数字比较的前提：两端（参数顺序相反）必须得到同一个 6 位码。
    #[test]
    fn derive_code_is_symmetric() {
        let eph_a = [7u8; 32];
        let eph_b = [9u8; 32];
        let a = derive_code(&eph_a, "SHA256:aaa", &eph_b, "SHA256:bbb");
        let b = derive_code(&eph_b, "SHA256:bbb", &eph_a, "SHA256:aaa");
        assert_eq!(a, b);
        assert_eq!(a.len(), 6);
        assert!(a.chars().all(|c| c.is_ascii_digit()));
    }

    /// 换掉任一输入（临时公钥或设备 id）都必须换码：MITM 会提供不同临时公钥。
    #[test]
    fn derive_code_binds_all_inputs() {
        let eph_a = [7u8; 32];
        let eph_b = [9u8; 32];
        let base = derive_code(&eph_a, "id-a", &eph_b, "id-b");
        assert_ne!(base, derive_code(&eph_a, "id-a", &eph_b, "id-c"));
        assert_ne!(base, derive_code(&eph_a, "id-a", &[8u8; 32], "id-b"));
    }

    /// 即使两端临时公钥意外相等（反射/异常 RNG），码也必须对称。
    #[test]
    fn derive_code_is_symmetric_when_eph_equal() {
        let eph = [7u8; 32];
        let a = derive_code(&eph, "SHA256:aaa", &eph, "SHA256:bbb");
        let b = derive_code(&eph, "SHA256:bbb", &eph, "SHA256:aaa");
        assert_eq!(a, b);
    }

    fn side(name: &str, id: &str, eph: u8) -> SideInfo {
        SideInfo {
            v: PAIR_PROTOCOL_VERSION,
            id: id.to_string(),
            name: name.to_string(),
            fp: "12345678".to_string(),
            ssh_port: 27410,
            strict: false,
            eph: [eph; 32].to_vec(),
        }
    }

    /// 长期密钥签名/验签与 PEM 往返（配对 v3 的持有证明）。
    #[test]
    fn transcript_signature_roundtrip() {
        use rand_core::{Infallible, TryCryptoRng, TryRng};
        use russh::keys::{Algorithm, PrivateKey};

        struct TestRng(ring::rand::SystemRandom);
        impl TryRng for TestRng {
            type Error = Infallible;
            fn try_next_u32(&mut self) -> Result<u32, Self::Error> {
                let mut b = [0u8; 4];
                self.0.fill(&mut b).expect("rng");
                Ok(u32::from_ne_bytes(b))
            }
            fn try_next_u64(&mut self) -> Result<u64, Self::Error> {
                let mut b = [0u8; 8];
                self.0.fill(&mut b).expect("rng");
                Ok(u64::from_ne_bytes(b))
            }
            fn try_fill_bytes(&mut self, dst: &mut [u8]) -> Result<(), Self::Error> {
                self.0.fill(dst).expect("rng");
                Ok(())
            }
        }
        impl TryCryptoRng for TestRng {}

        let a = side("A", "id-a", 7);
        let b = side("B", "id-b", 9);
        let transcript = transcript_bytes(&a, &b);
        let key = PrivateKey::random(
            &mut TestRng(ring::rand::SystemRandom::new()),
            Algorithm::Ed25519,
        )
        .expect("key");
        let sig = key
            .sign(PAIR_SIG_NAMESPACE, HashAlg::Sha256, &transcript)
            .expect("sign");
        let pem = sig.to_pem(LineEnding::LF).expect("pem");
        let parsed = SshSig::from_pem(pem.as_bytes()).expect("parse");
        key.public_key()
            .verify(PAIR_SIG_NAMESPACE, &transcript, &parsed)
            .expect("verify");

        let mut tampered = transcript.clone();
        tampered.push(0);
        assert!(key
            .public_key()
            .verify(PAIR_SIG_NAMESPACE, &tampered, &parsed)
            .is_err());
    }

    /// 承诺必须绑定单端全部字段；transcript 必须与参数顺序无关。
    #[test]
    fn commit_and_transcript_are_canonical() {
        let a = side("A", "id-a", 7);
        let b = side("B", "id-b", 9);
        assert_eq!(commit_of(&a), commit_of(&a.clone()));
        assert_ne!(commit_of(&a), commit_of(&b));
        assert_eq!(transcript_bytes(&a, &b), transcript_bytes(&b, &a));
        // 极端情况下 eph/id 都相同、名称不同，也必须与参数顺序无关。
        let same_key = side("A2", "id-a", 7);
        assert_eq!(
            transcript_bytes(&a, &same_key),
            transcript_bytes(&same_key, &a)
        );

        let mut changed = a.clone();
        changed.ssh_port = 27411;
        assert_ne!(commit_of(&a), commit_of(&changed));
    }

    /// 拒绝确认的重复提示等不影响：发起方重新配对必须保留既有授权。
    #[test]
    fn outgoing_repair_keeps_existing_grants() {
        let existing = Grants {
            files: true,
            control: true,
            config: false,
            screen: true,
            clipboard: false,
            audio: false,
        };
        let decision = Grants::default();
        let kept = effective_grants("outgoing", Some(existing), decision);
        assert!(kept.files && kept.control && kept.screen && !kept.config);

        // 被请求方仍以弹窗勾选为准
        let picked = effective_grants(
            "incoming",
            Some(existing),
            Grants {
                files: true,
                ..Default::default()
            },
        );
        assert!(picked.files && !picked.control);

        // 新配对没有旧记录：即使前端残留 true，也必须全 false
        assert!(
            !effective_grants(
                "outgoing",
                None,
                Grants {
                    files: true,
                    control: true,
                    ..Default::default()
                }
            )
            .files
        );
    }

    /// 同一 pairingId 只能落一次决策；第二次必须返回 false（前端据此不再推进队列）。
    #[tokio::test]
    async fn pair_session_decide_is_single_shot() {
        let sessions = PairSessions::new();
        let rx = sessions.register("p1".to_string()).await;
        let decision = Decision {
            agree: true,
            grants: Grants::default(),
        };
        assert!(sessions.decide("p1", decision).await);
        assert!(!sessions.decide("p1", decision).await);
        assert!(rx.await.expect("decision").agree);
    }

    /// 成功的配对不应占用限速额度（否则连续给第 4 台设备配对会被自己的成功记录挡住）。
    #[tokio::test]
    async fn successful_pairs_release_rate_limit_slot() {
        let rate: Arc<Mutex<HashMap<IpAddr, Vec<Instant>>>> = Arc::new(Mutex::new(HashMap::new()));
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        for _ in 0..RATE_MAX {
            assert!(allowed(&rate, ip).await);
        }
        assert!(!allowed(&rate, ip).await, "超过上限应被限速");
        rate_success(&rate, ip).await;
        assert!(allowed(&rate, ip).await, "成功配对应归还额度");
    }

    /// 关联模型选择：已绑定静默；严格/密钥变化走数字比对；其余一键允许。
    #[test]
    fn pair_mode_matches_bluetooth_style_policy() {
        // 已绑定但双方都非严格：静默复用（蓝牙 reconnection）。
        assert_eq!(pair_mode(true, false, false, false), PairMode::Bonded);
        // 严格模式必须覆盖 Bonded，否则设置页开关被静默绕过。
        assert_eq!(pair_mode(true, false, true, true), PairMode::Numeric);
        // 默认：发现列表 + 双方都非严格 → 一键允许。
        assert_eq!(pair_mode(false, false, false, false), PairMode::JustWorks);
        // 任一端严格 → 数字比对。
        assert_eq!(pair_mode(false, false, true, false), PairMode::Numeric);
        assert_eq!(pair_mode(false, false, false, true), PairMode::Numeric);
        // 同一设备换了密钥 → 必须人工确认。
        assert_eq!(pair_mode(false, true, false, false), PairMode::Numeric);
    }

    #[test]
    fn addr_ip_handles_v4_and_v6() {
        assert_eq!(addr_ip("192.168.1.5:27410"), Some("192.168.1.5"));
        assert_eq!(addr_ip("[fe80::1]:27410"), Some("fe80::1"));
        assert_eq!(addr_ip("fe80::1:27410"), Some("fe80::1"));
    }

    /// Hello 缺 strict 字段时按“要求数字比对”处理，防止被静默降级。
    #[test]
    fn hello_without_strict_defaults_to_numeric() {
        let msg: PairMsg = serde_json::from_str(
            r#"{"type":"hello","v":3,"id":"i","name":"n","fp":"f","commit":"","sshPort":27410}"#,
        )
        .expect("parse hello");
        match msg {
            PairMsg::Hello { strict, .. } => assert!(strict),
            _ => panic!("expected hello"),
        }
    }

    /// strict 位必须进入承诺/签名，中间人不能把严格模式改成静默一键。
    #[test]
    fn strict_bit_is_bound_into_commitment() {
        let a = side("a", "id-a", 1);
        let mut b = a.clone();
        b.strict = !a.strict;
        assert_ne!(commit_of(&a), commit_of(&b));
    }

    /// 目标解析：裸 IP/主机名（含未加方括号的 IPv6）不指定端口；显式端口照用。
    #[test]
    fn parse_target_handles_bare_ip_and_explicit_port() {
        let (h, p) = parse_target("192.168.1.5");
        assert_eq!((h.as_str(), p), ("192.168.1.5", None));

        // 裸 IPv6 不能被误拆成 host=fe80: / port=1
        let (h, p) = parse_target("fe80::1");
        assert_eq!((h.as_str(), p), ("fe80::1", None));

        let (h, p) = parse_target("[fe80::1]:28411");
        assert_eq!((h.as_str(), p), ("fe80::1", Some(28411)));

        let (h, p) = parse_target("10.0.0.9:28412");
        assert_eq!((h.as_str(), p), ("10.0.0.9", Some(28412)));

        let (h, p) = parse_target("  10.0.0.9:28413  ");
        assert_eq!((h.as_str(), p), ("10.0.0.9", Some(28413)));

        // 主机名与显式端口：host 原样保留，端口单独返回
        let (h, p) = parse_target("peer-pc.local:28414");
        assert_eq!((h.as_str(), p), ("peer-pc.local", Some(28414)));
    }

    /// 前端事件字段必须是 camelCase（只写在枚举上的 rename_all 不覆盖变体字段，
    /// 否则 `pairingId` 会是 undefined、确认接口因缺参被拒）。
    #[test]
    fn pair_event_fields_are_camel_case() {
        let v = serde_json::to_value(PairEvent::Prompt {
            pairing_id: "p1".into(),
            code: "123456".into(),
            peer_name: "peer".into(),
            peer_fp: "fp".into(),
            peer_addr: "10.0.0.2".into(),
            direction: "incoming".into(),
            numeric: true,
            expires_at_ms: 123,
        })
        .expect("serialize prompt");
        assert_eq!(v["status"], "prompt");
        assert_eq!(v["pairingId"], "p1");
        assert_eq!(v["peerName"], "peer");
        assert_eq!(v["peerFp"], "fp");
        assert_eq!(v["peerAddr"], "10.0.0.2");
        assert_eq!(v["numeric"], true);
        assert_eq!(v["expiresAtMs"], 123);
        assert!(v.get("pairing_id").is_none());

        let waiting = serde_json::to_value(PairEvent::Waiting {
            pairing_id: "p2".into(),
            peer_name: "peer".into(),
            peer_addr: "10.0.0.3".into(),
            expires_at_ms: 456,
        })
        .expect("serialize waiting");
        assert_eq!(waiting["status"], "waiting");
        assert_eq!(waiting["pairingId"], "p2");
        assert_eq!(waiting["peerName"], "peer");
        assert_eq!(waiting["peerAddr"], "10.0.0.3");

        let ok = serde_json::to_value(PairEvent::Ok {
            pairing_id: "p1".into(),
            peer: PeerRecord {
                id: "id".into(),
                name: "n".into(),
                pubkey: "ssh-ed25519 AAAA".into(),
                last_addr: None,
                paired_at: String::new(),
                last_seen: None,
                grant_files: false,
                grant_control: false,
                grant_config: false,
                grant_screen: false,
                grant_clipboard: false,
                grant_audio: false,
                peer_allows_files: false,
                peer_allows_control: false,
                peer_allows_config: false,
                peer_allows_screen: false,
                peer_allows_clipboard: false,
                peer_allows_audio: false,
            },
        })
        .expect("serialize ok");
        assert_eq!(ok["status"], "ok");
        assert_eq!(ok["pairingId"], "p1");
        assert_eq!(ok["peer"]["grantFiles"], false);
    }
}
