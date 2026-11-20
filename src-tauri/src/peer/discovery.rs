//! 局域网发现（自定义 UDP 广播，只有本软件识别）。
//!
//! 设计：
//! - **不注册 mDNS/SSDP**，只用带 `magic=RUTHENIUM` 特征头的 UDP 报文；
//! - 只在「可发现窗口 / 常开」期间收发；窗口关闭即停止并清空列表；
//! - probe 由每个非回环 IPv4 接口的广播地址发出（另发 127.0.0.1，便于同机双实例）；
//! - 只对 `magic` + `nonce` 合法的 probe 回包（避免被通用扫描器枚举）；
//! - 回包含 SSH / 配对端口与设备指纹前 8 位，供 UI 展示与「已配对」标记。

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};
use tokio::net::UdpSocket;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::error::{AppError, AppResult};
use crate::events;

pub const DISCOVERY_PORT: u16 = 27412;
const MAGIC: &str = "RUTHENIUM";
const PROBE_SECS: u64 = 3;

/// 本机对外广播的信息（由 PeerService 在启动监听时构造）。
#[derive(Debug, Clone)]
pub struct LocalInfo {
    /// 设备 id（= 身份公钥指纹，稳定唯一）
    pub id: String,
    /// 与 PeerService 共享：设置里改设备名后 UDP 应答立即使用新值。
    pub name: Arc<StdMutex<String>>,
    /// 指纹前 8 位（不含 `SHA256:` 前缀，UI 展示用）
    pub fp8: String,
    pub ssh_port: u16,
    /// 实际配对端口；None = 本机配对服务不可用（前端禁用「配对」）。
    pub pair_port: Option<u16>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NetMsg {
    magic: String,
    v: u32,
    #[serde(rename = "type")]
    kind: String,
    id: String,
    nonce: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fp: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ssh_port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pair_port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pairing: Option<bool>,
}

/// 前端展示用的已发现设备。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredDevice {
    pub device_id: String,
    pub name: String,
    /// `ip:SSH端口`（文件互传 / 终端控制等连接目标）
    pub addr: String,
    /// `ip:配对端口`（数字比较握手目标）。
    ///
    /// 与 `addr` 是两个不同端口（SSH 27410+，配对 28410+），调用方不可混用：
    /// 混用二者会让「配对」连到 SSH 监听并握手失败。
    pub pair_addr: String,
    pub fp: String,
    pub pairing: bool,
    pub paired: bool,
}

/// 回包 → 展示用设备（顺带保证 SSH 地址与配对地址各用各自端口）。
fn device_from_reply(msg: &NetMsg, ip: &str, is_paired: bool) -> DiscoveredDevice {
    let ssh_port = msg.ssh_port.filter(|p| *p != 0).unwrap_or(27410);
    // 对端缺少 pairPort 时按标准配对端口处理，绝不回退成 SSH 端口。
    // pairPort=0 / pairing=false 表示对端配对服务不可用，前端据此禁用按钮。
    let pair_port = msg
        .pair_port
        .filter(|p| *p != 0)
        .unwrap_or(crate::peer::pairing::PAIR_PORT);
    DiscoveredDevice {
        device_id: msg.id.clone(),
        name: msg.name.clone().unwrap_or_else(|| ip.to_string()),
        addr: format_ip_port(ip, ssh_port),
        pair_addr: format_ip_port(ip, pair_port),
        paired: is_paired,
        fp: msg.fp.clone().unwrap_or_default(),
        // 对端缺少 pairing 字段时按支持配对处理（向后兼容）；
        // 新版本在配对端口不可用时显式发 Some(false)。
        pairing: msg.pairing.unwrap_or(true),
    }
}

pub struct Discovery {
    devices: Arc<Mutex<HashMap<String, DiscoveredDevice>>>,
    /// 已配对设备指纹前 8 位（与 PeerService / 配对握手共享；配对完成后即时生效）
    paired: Arc<Mutex<Vec<String>>>,
    cancel: CancellationToken,
    /// 收发任务（停止时等待退出，确保端口已释放，便于立刻重新开启）
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Discovery {
    /// 启动发现（绑定 27412 并起收发任务）。
    pub async fn start(
        app: AppHandle,
        local: LocalInfo,
        paired: Arc<Mutex<Vec<String>>>,
    ) -> AppResult<Self> {
        let socket = UdpSocket::bind(("0.0.0.0", DISCOVERY_PORT))
            .await
            .map_err(|e| {
                AppError::EPeerListen(format!("发现端口 {DISCOVERY_PORT} 绑定失败: {e}"))
            })?;
        // 广播必须显式开启；失败不致命（LAN 单播回包仍可用）
        let _ = socket.set_broadcast(true);
        let socket = Arc::new(socket);
        let cancel = CancellationToken::new();
        let devices: Arc<Mutex<HashMap<String, DiscoveredDevice>>> =
            Arc::new(Mutex::new(HashMap::new()));
        // 本机发出的 probe nonce。reply 必须回显其中一个，否则丢弃：
        // UDP 回复可被局域网内任意主机伪造，不能仅凭 magic 就接受。
        let probes: Arc<Mutex<HashMap<String, Instant>>> = Arc::new(Mutex::new(HashMap::new()));

        // 接收循环：probe → 回包；reply → 记录并推送
        let mut tasks = Vec::new();
        {
            let app = app.clone();
            let socket = socket.clone();
            let devices = devices.clone();
            let cancel = cancel.clone();
            let local = local.clone();
            let paired = paired.clone();
            let probes = probes.clone();
            tasks.push(tokio::spawn(async move {
                let mut buf = [0u8; 2048];
                loop {
                    tokio::select! {
                        _ = cancel.cancelled() => break,
                        received = socket.recv_from(&mut buf) => {
                            let Ok((n, from)) = received else { continue };
                            let Ok(msg) = serde_json::from_slice::<NetMsg>(&buf[..n]) else { continue };
                            if msg.magic != MAGIC || msg.v != 1 || msg.nonce.len() < 8 {
                                continue; // 非本软件/格式不符：不回包、不记录
                            }
                            if msg.id == local.id {
                                continue; // 自己（同机双实例时也会收到）
                            }
                            match msg.kind.as_str() {
                                "probe" => {
                                    let reply = NetMsg {
                                        magic: MAGIC.to_string(),
                                        v: 1,
                                        kind: "reply".to_string(),
                                        id: local.id.clone(),
                                        nonce: msg.nonce.clone(), // 原样回显
                                        name: Some(
                                            local
                                                .name
                                                .lock()
                                                .map(|n| n.clone())
                                                .unwrap_or_default(),
                                        ),
                                        fp: Some(local.fp8.clone()),
                                        ssh_port: Some(local.ssh_port),
                                        pair_port: local.pair_port,
                                        pairing: Some(local.pair_port.is_some()),
                                    };
                                    if let Ok(bytes) = serde_json::to_vec(&reply) {
                                        let _ = socket.send_to(&bytes, from).await;
                                    }
                                }
                                "reply" => {
                                    // 只接受对本机最近 probe 的回复；nonce 一次性使用。
                                    let valid_probe = {
                                        let mut p = probes.lock().await;
                                        p.retain(|_, at| at.elapsed() < Duration::from_secs(30));
                                        // 同一个 probe 会发往多个广播地址，必须允许
                                        // 多台设备回同一个 nonce；不能取走一次就失效。
                                        p.contains_key(&msg.nonce)
                                    };
                                    if !valid_probe {
                                        continue;
                                    }
                                    let ip = from.ip().to_string();
                                    let is_paired = {
                                        let fps = paired.lock().await;
                                        let fp = msg.fp.as_deref().unwrap_or_default();
                                        fps.iter()
                                            .any(|p| !p.is_empty() && fp.starts_with(p))
                                    };
                                    let dev = device_from_reply(&msg, &ip, is_paired);
                                    let mut map = devices.lock().await;
                                    let changed = match map.get(&dev.device_id) {
                                        Some(old) => old.name != dev.name || old.addr != dev.addr || old.pair_addr != dev.pair_addr || old.fp != dev.fp || old.paired != dev.paired,
                                        None => true,
                                    };
                                    map.insert(dev.device_id.clone(), dev);
                                    if changed {
                                        let list: Vec<_> = map.values().cloned().collect();
                                        drop(map);
                                        let _ = app.emit(events::PEER_DISCOVERED, list);
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }));
        }

        // 发送循环：每 PROBE_SECS 广播一次
        {
            let socket = socket.clone();
            let cancel = cancel.clone();
            let local = local.clone();
            let probes = probes.clone();
            tasks.push(tokio::spawn(async move {
                loop {
                    let nonce = uuid::Uuid::new_v4().to_string();
                    {
                        let mut p = probes.lock().await;
                        p.retain(|_, at| at.elapsed() < Duration::from_secs(30));
                        p.insert(nonce.clone(), Instant::now());
                    }
                    let probe = NetMsg {
                        magic: MAGIC.to_string(),
                        v: 1,
                        kind: "probe".to_string(),
                        id: local.id.clone(),
                        nonce,
                        name: None,
                        fp: None,
                        ssh_port: Some(local.ssh_port),
                        pair_port: local.pair_port,
                        pairing: Some(local.pair_port.is_some()),
                    };
                    if let Ok(bytes) = serde_json::to_vec(&probe) {
                        for target in broadcast_targets() {
                            let _ = socket
                                .send_to(
                                    &bytes,
                                    SocketAddr::new(IpAddr::V4(target), DISCOVERY_PORT),
                                )
                                .await;
                        }
                    }
                    tokio::select! {
                        _ = cancel.cancelled() => break,
                        _ = tokio::time::sleep(Duration::from_secs(PROBE_SECS)) => {}
                    }
                }
            }));
        }

        Ok(Self {
            devices,
            paired,
            cancel,
            tasks,
        })
    }

    /// 停止并等待收发任务退出：返回后 27412 端口已释放，可立刻重新开启。
    pub async fn shutdown(mut self) {
        self.cancel.cancel();
        for t in self.tasks.drain(..) {
            let _ = t.await;
        }
    }

    /// 已发现设备（`paired` 按当前已配对指纹实时计算，配对/解除后立即反映）。
    pub async fn list(&self) -> Vec<DiscoveredDevice> {
        let fps = self.paired.lock().await.clone();
        let map = self.devices.lock().await;
        let mut v: Vec<_> = map
            .values()
            .cloned()
            .map(|mut d| {
                d.paired = fps.iter().any(|p| !p.is_empty() && d.fp.starts_with(p));
                d
            })
            .collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

fn format_ip_port(ip: &str, port: u16) -> String {
    if ip.contains(':') {
        format!("[{ip}]:{port}")
    } else {
        format!("{ip}:{port}")
    }
}

#[cfg(windows)]
fn broadcast_targets() -> Vec<Ipv4Addr> {
    use windows::Win32::NetworkManagement::IpHelper::{
        GetAdaptersAddresses, GAA_FLAG_INCLUDE_PREFIX, GAA_FLAG_SKIP_ANYCAST,
        GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST, IP_ADAPTER_ADDRESSES_LH,
    };
    use windows::Win32::Networking::WinSock::{AF_INET, AF_UNSPEC, SOCKADDR_IN};

    let mut out: Vec<Ipv4Addr> = vec![Ipv4Addr::LOCALHOST];
    unsafe {
        // OnLinkPrefixLength 只有在 GAA_FLAG_INCLUDE_PREFIX 下才保证有效；
        // 缺了它会把所有 iface 的 plen 当成 0，最终只广播 127.0.0.1。
        let flags = GAA_FLAG_INCLUDE_PREFIX
            | GAA_FLAG_SKIP_ANYCAST
            | GAA_FLAG_SKIP_MULTICAST
            | GAA_FLAG_SKIP_DNS_SERVER;
        let mut size = 0u32;
        GetAdaptersAddresses(AF_UNSPEC.0 as u32, flags, None, None, &mut size);
        if size == 0 {
            return out;
        }
        // `IP_ADAPTER_ADDRESSES_LH` 含指针，要求 8 字节对齐；`Vec<u8>` 不保证，
        // 用 `Vec<u64>` 分配再强转，避免未定义行为。
        let words = (size as usize).div_ceil(std::mem::size_of::<u64>());
        let mut buf = vec![0u64; words];
        let first = buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH;
        if GetAdaptersAddresses(AF_UNSPEC.0 as u32, flags, None, Some(first), &mut size) != 0 {
            return out;
        }
        let mut cur = first;
        while !cur.is_null() {
            let a = &*cur;
            let mut ua = a.FirstUnicastAddress;
            while !ua.is_null() {
                let u = &*ua;
                let sa = &u.Address;
                if !sa.lpSockaddr.is_null() && (*sa.lpSockaddr).sa_family == AF_INET {
                    let sin = &*(sa.lpSockaddr as *const SOCKADDR_IN);
                    let b = &sin.sin_addr.S_un.S_un_b;
                    let ip = u32::from_be_bytes([b.s_b1, b.s_b2, b.s_b3, b.s_b4]);
                    let is_loopback = ip >> 24 == 127;
                    let is_link_local = ip >> 16 == 0xA9FE; // 169.254/16
                    if !is_loopback && !is_link_local && ip != 0 {
                        let plen = u.OnLinkPrefixLength as u32;
                        if plen > 0 && plen < 32 {
                            let mask: u32 = u32::MAX << (32 - plen);
                            let bcast = ip | !mask;
                            out.push(Ipv4Addr::from(bcast));
                        }
                    }
                }
                ua = u.Next;
            }
            cur = a.Next;
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

#[cfg(not(windows))]
fn broadcast_targets() -> Vec<Ipv4Addr> {
    vec![Ipv4Addr::LOCALHOST]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply() -> NetMsg {
        NetMsg {
            magic: MAGIC.to_string(),
            v: 1,
            kind: "reply".to_string(),
            id: "dev-b".to_string(),
            nonce: "0123456789abcdef".to_string(),
            name: Some("B".to_string()),
            fp: Some("abcd1234".to_string()),
            ssh_port: Some(27411),
            pair_port: Some(28411),
            pairing: Some(true),
        }
    }

    /// 从发现列表配对必须连“配对端口”，不能复用 SSH 端口（否则直连 SSH 监听、
    /// 握手解析失败）。
    #[test]
    fn reply_keeps_ssh_and_pair_addresses_apart() {
        let d = device_from_reply(&reply(), "192.168.1.7", true);
        assert_eq!(d.addr, "192.168.1.7:27411");
        assert_eq!(d.pair_addr, "192.168.1.7:28411");
        assert!(d.paired);
    }

    /// pairPort=0 视为不可用，回退标准配对端口；显式 pairing=false 保持禁用。
    #[test]
    fn reply_zero_pair_port_and_unavailable_flag() {
        let zero = NetMsg {
            pair_port: Some(0),
            ..reply()
        };
        let d = device_from_reply(&zero, "10.0.0.2", false);
        assert_eq!(d.pair_addr, "10.0.0.2:28410");

        let off = NetMsg {
            pairing: Some(false),
            ..reply()
        };
        assert!(!device_from_reply(&off, "10.0.0.2", false).pairing);
    }

    /// 对端缺少 pairing 字段时默认按支持配对处理（向后兼容）。
    #[test]
    fn reply_without_pairing_flag_is_assumed_capable() {
        let old = NetMsg {
            pairing: None,
            ..reply()
        };
        assert!(device_from_reply(&old, "10.0.0.2", false).pairing);
    }

    /// 对端只广播 sshPort 时，配对地址回退标准配对端口，而不是误用 SSH 端口。
    #[test]
    fn reply_without_pair_port_uses_standard_pair_port() {
        let old = NetMsg {
            pair_port: None,
            ..reply()
        };
        let d = device_from_reply(&old, "10.0.0.2", false);
        assert_eq!(d.pair_addr, "10.0.0.2:28410");
        assert_eq!(d.addr, "10.0.0.2:27411");
    }

    /// IPv6 地址必须加方括号，否则前端拼 `IP:端口` 会解析成裸地址。
    #[test]
    fn reply_formats_ipv6_addresses() {
        let d = device_from_reply(&reply(), "fe80::1", false);
        assert_eq!(d.addr, "[fe80::1]:27411");
        assert_eq!(d.pair_addr, "[fe80::1]:28411");
    }

    /// 事件字段 camelCase（前端 `d.pairAddr`）。
    #[test]
    fn discovered_device_fields_are_camel_case() {
        let v = serde_json::to_value(device_from_reply(&reply(), "127.0.0.1", false)).unwrap();
        assert_eq!(v["deviceId"], "dev-b");
        assert_eq!(v["pairAddr"], "127.0.0.1:28411");
        assert_eq!(v["addr"], "127.0.0.1:27411");
        assert!(v.get("pair_addr").is_none());
    }
}
