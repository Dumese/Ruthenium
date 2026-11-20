//! metrics 采样器：复用会话 SSH 连接新开 channel，只读采集
//! /proc/stat /proc/meminfo /proc/net/dev /proc/diskstats /proc/loadavg 与
//! df -B1 /。CPU/网络/磁盘是差值型：内部缓存上一帧 + 时间差换算每秒速率。
use crate::models::HostMetrics;

/// 只读采样命令（/proc/loadavg 纳入 cat 列表）。
pub const METRICS_CMD: &str =
    "cat /proc/stat /proc/meminfo /proc/net/dev /proc/diskstats /proc/loadavg; df -B1 /";

/// 原始采样（计数型，尚未做差值）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RawSample {
    pub cpu_total_ticks: u64,     // cpu 聚合行全部 tick 之和
    pub cpu_idle_ticks: u64,      // idle + iowait
    pub cores: u8,                // cpuN 行数
    pub mem_total_bytes: u64,     // MemTotal * 1024
    pub mem_available_bytes: u64, // MemAvailable * 1024
    pub net_rx_bytes: u64,        // /proc/net/dev 各物理网卡 rx 字节之和（不含 lo）
    pub net_tx_bytes: u64,
    pub disk_read_bytes: u64, // diskstats 扇区读 * 512 之和（不含 loop/ram）
    pub disk_write_bytes: u64,
    pub load_avg: [f32; 3], // /proc/loadavg
    pub df_used_bytes: u64, // df -B1 / 根分区已用
    pub df_total_bytes: u64,
}

/// 上一帧缓存（差值基准）。
/// `t` 记录**上一帧采样完成**的时刻：diff 时用它计算到当前帧的真实间隔。
#[derive(Debug, Clone)]
pub struct PrevSample {
    pub cpu_total: u64,
    pub cpu_idle: u64,
    pub net_rx: u64,
    pub net_tx: u64,
    pub disk_read: u64,
    pub disk_write: u64,
    pub t: std::time::Instant,
}

/// /proc/meminfo 的单位是 KiB（1024 字节），不是十进制的 1000 字节。
const KIB: u64 = 1024;
/// /proc/diskstats 的扇区固定按 512 字节计（与设备实际扇区大小无关）。
const DISK_SECTOR_BYTES: u64 = 512;

/// 解析 cat 五个 /proc 文件 + df -B1 / 的拼接输出；任一部分缺失返回 None。
pub fn parse_metrics_output(text: &str) -> Option<RawSample> {
    let mut s = RawSample::default();
    let mut have_stat = false;
    let mut have_meminfo = false;
    let mut have_net = false;
    let mut have_diskstats = false;
    let mut have_load = false;
    let mut have_df = false;

    for line in text.lines() {
        let toks: Vec<&str> = line.split_whitespace().collect();
        match toks.len() {
            // ---- /proc/stat ----
            n if line.starts_with("cpu ") && n >= 5 => {
                let ticks = parse_u64s(&line[4..]);
                if ticks.len() >= 5 {
                    s.cpu_total_ticks = ticks.iter().sum();
                    s.cpu_idle_ticks = ticks[3] + ticks[4];
                    have_stat = true;
                }
            }
            _ if line.starts_with("cpu") && !line.starts_with("cpu ") && toks.len() >= 2 => {
                s.cores = s.cores.saturating_add(1); // cpuN 行：数核数
            }
            // ---- /proc/meminfo ----
            _ if line.starts_with("MemTotal:") => {
                if let Some(v) = parse_kb_line(line) {
                    s.mem_total_bytes = v * KIB;
                    have_meminfo = true;
                }
            }
            _ if line.starts_with("MemAvailable:") => {
                if let Some(v) = parse_kb_line(line) {
                    s.mem_available_bytes = v * KIB;
                }
            }
            // ---- /proc/net/dev：iface: rx_bytes packets ... tx_bytes ...（16 个数）
            _ if !toks.is_empty() && toks[0].contains(':') => {
                let (iface, _) = toks[0].split_once(':')?;
                let nums = parse_u64s(&toks[1..].join(" "));
                if nums.len() >= 16 {
                    have_net = true;
                    if !is_virtual_iface(iface.trim()) {
                        s.net_rx_bytes += nums[0];
                        s.net_tx_bytes += nums[8];
                    }
                }
            }
            // ---- /proc/diskstats：major minor name + 计数（内核 ≥4.19 为 18 列，
            //      老内核 14 列；扩展字段追加在尾部，扇区下标不变）----
            n if (14..=20).contains(&n)
                && toks[0].parse::<u64>().is_ok()
                && toks[1].parse::<u64>().is_ok()
                && toks[3].parse::<u64>().is_ok() =>
            {
                let name = toks[2];
                if !is_partition_name(name)
                    && !(name.starts_with("loop") || name.starts_with("ram"))
                {
                    have_diskstats = true;
                    s.disk_read_bytes += toks[5].parse::<u64>().ok()? * DISK_SECTOR_BYTES;
                    s.disk_write_bytes += toks[9].parse::<u64>().ok()? * DISK_SECTOR_BYTES;
                }
            }
            // ---- /proc/loadavg：0.52 0.58 0.59 1/234 5678
            5 if !have_load
                && toks[0].parse::<f32>().is_ok()
                && toks[1].parse::<f32>().is_ok()
                && toks[2].parse::<f32>().is_ok() =>
            {
                s.load_avg = [
                    toks[0].parse::<f32>().unwrap_or(0.0),
                    toks[1].parse::<f32>().unwrap_or(0.0),
                    toks[2].parse::<f32>().unwrap_or(0.0),
                ];
                have_load = true;
            }
            // ---- df -B1 /：dev total used avail use% mountpoint
            n if n >= 6
                && toks[1].parse::<u64>().is_ok()
                && toks[2].parse::<u64>().is_ok()
                && toks[n - 1] == "/" =>
            {
                s.df_total_bytes = toks[1].parse::<u64>().unwrap_or(0);
                s.df_used_bytes = toks[2].parse::<u64>().unwrap_or(0);
                have_df = true;
            }
            _ => {} // 标题行（Inter-| Receive 等）与其他行忽略
        }
    }

    let complete = have_stat
        && have_meminfo
        && have_net
        && have_diskstats
        && have_load
        && have_df
        && s.cores > 0;
    if complete {
        Some(s)
    } else {
        None
    }
}

fn parse_u64s(line: &str) -> Vec<u64> {
    line.split_whitespace()
        .filter_map(|t| t.parse::<u64>().ok())
        .collect()
}

fn parse_kb_line(line: &str) -> Option<u64> {
    let value = line.split(':').nth(1)?;
    let head = value.split_whitespace().next()?;
    head.parse::<u64>().ok()
}

/// 统计网络速率时应排除的虚拟/回环接口。
///
/// 采样只拿到 `/proc/net/dev` 文本，无法逐条查 `/sys/class/net/<if>/device`，
/// 因此按常见前缀过滤：容器（docker/veth/br-/cni/flannel/kube）、
/// VPN（tun/tap/wg/tailscale/zt）、虚拟化（virbr/vmnet）都不代表物理网卡，
/// 计入会把主机网络速率算成数倍。
fn is_virtual_iface(name: &str) -> bool {
    const VIRTUAL_PREFIXES: &[&str] = &[
        "lo",
        "docker",
        "veth",
        "br-",
        "virbr",
        "tun",
        "tap",
        "vmnet",
        "wg",
        "zt",
        "tailscale",
        "cni",
        "flannel",
        "kube",
    ];
    VIRTUAL_PREFIXES.iter().any(|p| name.starts_with(p))
}

/// `/proc/diskstats` 中同一笔 I/O 会同时记入整盘与其分区，必须只统计整盘。
///
/// - `sd*` / `hd*` / `vd*` / `xvd*`：尾随数字即分区（sda1、vda14）；
/// - `nvme*` / `mmcblk*`：整盘无 `p`，分区形如 nvme0n1p1、mmcblk0p1；
/// - 其余（md/dm 等）按整盘处理，loop/ram 由调用方另行排除。
fn is_partition_name(name: &str) -> bool {
    if name.starts_with("nvme") || name.starts_with("mmcblk") {
        return name.contains('p');
    }
    let disk_family = ["sd", "hd", "vd", "xvd"]
        .iter()
        .any(|p| name.starts_with(p));
    disk_family && name.chars().last().is_some_and(|c| c.is_ascii_digit())
}

/// 差值换算：首帧（prev=None）差值项为 0。
pub fn diff_to_metrics(
    session_id: &str,
    sampled_at: i64,
    cur: &RawSample,
    prev: Option<&PrevSample>,
) -> HostMetrics {
    let (cpu_pct, net_rx_mbps, net_tx_mbps, disk_read_mbps, disk_write_mbps) = match prev {
        None => (0.0, 0.0, 0.0, 0.0, 0.0),
        Some(p) => {
            // 真实间隔 = 上一帧采样完成时刻到现在的流逝时间；下限 1ms 防除零。
            let dt = p.t.elapsed().as_secs_f32().max(0.001);
            let cpu = if cur.cpu_total_ticks > p.cpu_total {
                let total_d = (cur.cpu_total_ticks - p.cpu_total) as f32;
                let idle_d = cur.cpu_idle_ticks.saturating_sub(p.cpu_idle) as f32;
                (1.0 - idle_d / total_d).clamp(0.0, 1.0) * 100.0
            } else {
                0.0
            };
            let mbps = |cur_v: u64, prev_v: u64| {
                if cur_v >= prev_v {
                    (cur_v - prev_v) as f32 / dt / 1_000_000.0
                } else {
                    0.0 // 计数器回绕/会话切换：丢弃该帧差值
                }
            };
            (
                cpu,
                mbps(cur.net_rx_bytes, p.net_rx),
                mbps(cur.net_tx_bytes, p.net_tx),
                mbps(cur.disk_read_bytes, p.disk_read),
                mbps(cur.disk_write_bytes, p.disk_write),
            )
        }
    };
    HostMetrics {
        session_id: session_id.to_string(),
        sampled_at,
        cpu_pct,
        cores: cur.cores,
        load_avg: cur.load_avg,
        mem_used_gb: cur.mem_total_bytes.saturating_sub(cur.mem_available_bytes) as f32 / GB,
        mem_total_gb: cur.mem_total_bytes as f32 / GB,
        disk_used_gb: cur.df_used_bytes as f32 / GB,
        disk_total_gb: cur.df_total_bytes as f32 / GB,
        net_rx_mbps,
        net_tx_mbps,
        disk_read_mbps,
        disk_write_mbps,
    }
}

/// 由当前帧构造下一帧的差值基准（内部记录采样完成时刻 Instant::now()）。
pub fn prev_from(cur: &RawSample) -> PrevSample {
    PrevSample {
        cpu_total: cur.cpu_total_ticks,
        cpu_idle: cur.cpu_idle_ticks,
        net_rx: cur.net_rx_bytes,
        net_tx: cur.net_tx_bytes,
        disk_read: cur.disk_read_bytes,
        disk_write: cur.disk_write_bytes,
        t: std::time::Instant::now(),
    }
}

const GB: f32 = 1024.0 * 1024.0 * 1024.0;

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "cpu  100 0 50 700 50 0 0 0 0 0\ncpu0 100 0 50 700 50 0 0 0 0 0\ncpu1 60 0 30 660 50 0 0 0 0 0\nintr 1234\nMemTotal: 2048000 kB\nMemFree: 100000 kB\nMemAvailable: 1024000 kB\nSwapTotal: 0 kB\nInter-|   Receive                                                   |  Transmit\n face |0000000042 0 0 0 0 0 0 0 0000000042 0 0 0 0 0 0 0\n  lo: 100 1 0 0 0 0 0 0 200 1 0 0 0 0 0 0\n eth0: 1000000 10 0 0 0 0 0 0 500000 10 0 0 0 0 0 0\n   7       0 loop0 10 0 10 0 0 0 0 0 0 0\n   8       0 sda 100 0 1000 10 50 0 2000 20 0 0 0\n 0.52 0.58 0.59 1/234 5678\n/dev/sda1 107374182400 32212254720 75161927680 30% /\n";

    #[test]
    fn parse_full_sample() {
        let s = parse_metrics_output(SAMPLE).expect("样例文本应解析成功");
        // cpu 总 tick = 100+0+50+700+50 = 900；idle = 700+50
        assert_eq!(s.cpu_total_ticks, 900);
        assert_eq!(s.cpu_idle_ticks, 750);
        assert_eq!(s.cores, 2);
        assert_eq!(s.mem_total_bytes, 2048000 * 1024);
        assert_eq!(s.mem_available_bytes, 1024000 * 1024);
        // lo 排除，只有 eth0
        assert_eq!(s.net_rx_bytes, 1_000_000);
        assert_eq!(s.net_tx_bytes, 500_000);
        // loop 排除；sda 扇区读 1000*512、写 2000*512
        assert_eq!(s.disk_read_bytes, 1000 * 512);
        assert_eq!(s.disk_write_bytes, 2000 * 512);
        assert_eq!(s.load_avg, [0.52, 0.58, 0.59]);
        assert_eq!(s.df_total_bytes, 107374182400);
        assert_eq!(s.df_used_bytes, 32212254720);
    }

    /// 整盘 + 分区重复累加、虚拟网卡混入都会让速率虚高。
    const SAMPLE_WITH_PARTITIONS: &str = concat!(
        "cpu  100 0 50 700 50 0 0 0 0 0\n",
        "cpu0 100 0 50 700 50 0 0 0 0 0\n",
        "MemTotal: 1024 kB\n",
        "MemAvailable: 512 kB\n",
        "    lo: 10 1 0 0 0 0 0 0 20 1 0 0 0 0 0 0\n",
        "  eth0: 1000 1 0 0 0 0 0 0 2000 1 0 0 0 0 0 0\n",
        "docker0: 3000 1 0 0 0 0 0 0 4000 1 0 0 0 0 0 0\n",
        "   7 0 loop0 10 0 10 0 0 0 0 0 0 0\n",
        "   8 0 sda 100 0 1000 10 50 0 2000 20 0 0 0\n",
        "   8 1 sda1 100 0 1000 10 50 0 2000 20 0 0 0\n",
        " 259 0 nvme0n1 10 0 100 1 20 0 200 2 0 0 0\n",
        " 259 1 nvme0n1p1 10 0 100 1 20 0 200 2 0 0 0\n",
        "0.50 0.40 0.30 1/100 1234\n",
        "/dev/sda1 100 40 60 40% /\n"
    );

    #[test]
    fn partitions_and_virtual_ifaces_do_not_double_count() {
        let s = parse_metrics_output(SAMPLE_WITH_PARTITIONS).expect("样本应解析成功");
        // 只统计 eth0（lo / docker0 排除）
        assert_eq!(s.net_rx_bytes, 1000, "虚拟网卡不得计入: {s:?}");
        assert_eq!(s.net_tx_bytes, 2000);
        // 只统计整盘 sda + nvme0n1（分区 sda1 / nvme0n1p1 排除）
        assert_eq!(s.disk_read_bytes, (1000 + 100) * 512);
        assert_eq!(s.disk_write_bytes, (2000 + 200) * 512);
        assert_eq!(s.cores, 1);
    }

    #[test]
    fn partition_and_iface_classification() {
        for part in ["sda1", "sdb14", "vda1", "xvda2", "nvme0n1p1", "mmcblk0p1"] {
            assert!(is_partition_name(part), "{part} 应判定为分区");
        }
        for disk in [
            "sda", "sdb", "vda", "xvda", "nvme0n1", "mmcblk0", "md0", "dm-0",
        ] {
            assert!(!is_partition_name(disk), "{disk} 应判定为整盘");
        }
        for virt in [
            "lo",
            "docker0",
            "veth123",
            "br-abc",
            "tun0",
            "tap0",
            "wg0",
            "tailscale0",
        ] {
            assert!(is_virtual_iface(virt), "{virt} 应排除");
        }
        for phy in ["eth0", "ens33", "enp3s0", "wlan0"] {
            assert!(!is_virtual_iface(phy), "{phy} 应统计");
        }
    }

    /// 真实服务器（Ubuntu 24.04 / 内核 6.8）采集样本：/proc/diskstats 为
    /// 18 列（含 flush 字段），net/dev 头行 16 列。
    const SAMPLE_KERNEL6: &str = concat!(
        "cpu  4671 3 2807 800384 935 0 676 0 0 0\n",
        "cpu0 2281 0 1423 400360 364 0 350 0 0 0\n",
        "cpu1 2389 3 1383 400024 571 0 325 0 0 0\n",
        "intr 5294137 0 9 0\n",
        "MemTotal:        1782740 kB\n",
        "MemFree:         1070160 kB\n",
        "MemAvailable:    1393564 kB\n",
        "SwapTotal:             0 kB\n",
        "Inter-|   Receive                                                |  Transmit\n",
        " face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n",
        "    lo:   39629     368    0    0    0     0          0         0    39629     368    0    0    0     0       0         0\n",
        "  eth0: 39437775   34493    0    0    0     0          0         0  2132702   20542    0    0    0     0       0         0\n",
        "   7       0 loop0 11 0 28 0 0 0 0 0 0 0 0 0 0 0 0 0\n",
        " 253       0 vda 4231 613 165912 1384 473812 8970 6278952 12922 0 0 0 0 0 0 0 0\n",
        " 0.00 0.00 0.00 1/199 2658\n",
        "/dev/vda3      41882943488 6647119872 33300086784  17% /\n",
    );

    #[test]
    fn parse_kernel6_real_sample() {
        let s = parse_metrics_output(SAMPLE_KERNEL6).expect("内核 6.x 真实样本应解析成功");
        assert_eq!(s.cores, 2);
        // eth0 rx=39437775 tx=2132702；lo 排除
        assert_eq!(s.net_rx_bytes, 39_437_775);
        assert_eq!(s.net_tx_bytes, 2_132_702);
        // 18 列 diskstats：vda 扇区读 6278952*512（loop0 排除）
        // 注意 sectors read 在 toks[5]=? vda 行: 253 0 vda 4231 613 165912 1384 473812 ...
        // toks[5]=165912（读扇区）、toks[9]=6278952（写扇区）
        assert_eq!(s.disk_read_bytes, 165_912 * 512);
        assert_eq!(s.disk_write_bytes, 6_278_952 * 512);
        assert_eq!(s.mem_total_bytes, 1_782_740 * 1024);
        assert_eq!(s.load_avg, [0.0, 0.0, 0.0]);
        assert_eq!(s.df_total_bytes, 41_882_943_488);
        assert_eq!(s.df_used_bytes, 6_647_119_872);
        let m = diff_to_metrics("s", 0, &s, None);
        assert_eq!(m.cores, 2);
        assert!(m.mem_total_gb > 1.5 && m.mem_total_gb < 2.0);
    }

    #[test]
    fn parse_incomplete_returns_none() {
        assert!(parse_metrics_output("cpu 1 2 3 4 5").is_none());
        assert!(parse_metrics_output("").is_none());
    }

    #[test]
    fn first_frame_zero_diffs() {
        let s = parse_metrics_output(SAMPLE).unwrap();
        let m = diff_to_metrics("sess", 1_000, &s, None);
        assert_eq!(m.cpu_pct, 0.0);
        assert_eq!(m.net_rx_mbps, 0.0);
        assert_eq!(m.disk_write_mbps, 0.0);
        assert_eq!(m.cores, 2);
        // 1024000 kB 已用 = 1024000*1024 / 2^30 GB
        assert!((m.mem_used_gb - 1024000.0 * 1024.0 / (1024.0 * 1024.0 * 1024.0)).abs() < 0.01);
        assert_eq!(m.sampled_at, 1_000);
        assert_eq!(m.session_id, "sess");
    }

    #[test]
    fn second_frame_rates() {
        let s = parse_metrics_output(SAMPLE).unwrap();
        let p = prev_two_seconds_ago(&s);
        // 下一帧：cpu 全 tick +200（idle +100），net rx +2_000_000，盘写 +1_000_000
        let mut next = s.clone();
        next.cpu_total_ticks += 200;
        next.cpu_idle_ticks += 100;
        next.net_rx_bytes += 2_000_000;
        next.disk_write_bytes += 1_000_000;
        let m = diff_to_metrics("sess", 3_000, &next, Some(&p));
        assert!((m.cpu_pct - 50.0).abs() < 0.01, "cpu = {}", m.cpu_pct);
        assert!((m.net_rx_mbps - 1.0).abs() < 1e-6);
        assert_eq!(m.net_tx_mbps, 0.0);
        assert!((m.disk_write_mbps - 0.5).abs() < 1e-6);
    }

    #[test]
    fn counter_regression_gives_zero() {
        let s = parse_metrics_output(SAMPLE).unwrap();
        let p = prev_two_seconds_ago(&s);
        let mut next = s.clone();
        next.net_rx_bytes = 0; // 回绕
        let m = diff_to_metrics("sess", 3_000, &next, Some(&p));
        assert_eq!(m.net_rx_mbps, 0.0);
    }

    /// 测试辅助：构造「2 秒前采样完成」的上一帧。
    /// 语义：2s 间隔 → net rx +2_000_000 字节 → 1.0 MB/s。
    fn prev_two_seconds_ago(s: &RawSample) -> PrevSample {
        PrevSample {
            cpu_total: s.cpu_total_ticks,
            cpu_idle: s.cpu_idle_ticks,
            net_rx: s.net_rx_bytes,
            net_tx: s.net_tx_bytes,
            disk_read: s.disk_read_bytes,
            disk_write: s.disk_write_bytes,
            t: std::time::Instant::now() - std::time::Duration::from_secs(2),
        }
    }

    #[test]
    fn metrics_serde_is_camel_case() {
        let s = parse_metrics_output(SAMPLE).unwrap();
        let m = diff_to_metrics("sess", 1, &s, None);
        let json = serde_json::to_value(&m).unwrap();
        for key in [
            "sessionId",
            "sampledAt",
            "cpuPct",
            "cores",
            "loadAvg",
            "memUsedGb",
            "memTotalGb",
            "diskUsedGb",
            "diskTotalGb",
            "netRxMbps",
            "netTxMbps",
            "diskReadMbps",
            "diskWriteMbps",
        ] {
            assert!(json.get(key).is_some(), "缺少 camelCase 字段 {key}");
        }
    }
}
