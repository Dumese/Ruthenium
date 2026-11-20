//! 技术验证 spike（远程桌面 + 局域网对等层的关键未知项）
//!
//! 子命令：
//!   capture [out.jpg] [quality] [frames]   DXGI Desktop Duplication → GDI+ JPEG，打印分段耗时
//!   serve   [port] [root]                  russh SSH 服务端：任意公钥登录 + 真实文件系统 SFTP + ConPTY shell
//!   adapters                               枚举 IPv4 接口与广播地址（发现层用）
//!
//! 用法（仓库根目录）：
//!   cargo run --manifest-path src-tauri/Cargo.toml --example peer_spike -- capture out.jpg 65 20
//!   cargo run --manifest-path src-tauri/Cargo.toml --example peer_spike -- serve 27499 .
//!   cargo run --manifest-path src-tauri/Cargo.toml --example peer_spike -- adapters
//!
//! serve 用系统自带 OpenSSH 客户端验证（任意 key 都能过，spike 简化）：
//!   sftp -i key -P 27499 -o StrictHostKeyChecking=no -o UserKnownHostsFile=NUL ruthenium@127.0.0.1
//!   ssh  -i key -p 27499 -o StrictHostKeyChecking=no -o UserKnownHostsFile=NUL ruthenium@127.0.0.1

#[cfg(not(windows))]
fn main() {
    eprintln!("peer_spike 仅实现 Windows 路径");
}

#[cfg(windows)]
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(|s| s.as_str()).unwrap_or("help");
    let result = match mode {
        "capture" => {
            let out = args
                .get(2)
                .cloned()
                .unwrap_or_else(|| "spike-capture.jpg".to_string());
            let quality: u8 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(65);
            let frames: u32 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(20);
            capture::run(&out, quality, frames)
        }
        "serve" => {
            let port: u16 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(27499);
            let root = args.get(3).cloned().unwrap_or_else(|| ".".to_string());
            serve::run(port, root)
        }
        "adapters" => adapters::run(),
        _ => {
            eprintln!("用法: peer_spike <capture|serve|adapters> [...]");
            Ok(())
        }
    };
    if let Err(e) = result {
        eprintln!("ERROR: {e:?}");
        std::process::exit(1);
    }
}

#[cfg(windows)]
mod capture {
    use std::ffi::c_void;
    use std::time::Instant;
    use windows::core::{Interface, PCWSTR};
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0};
    use windows::Win32::Graphics::Direct3D11::{
        D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D,
        D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_FLAG, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ,
        D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
    };
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, IDXGIAdapter, IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication,
        IDXGIResource, DXGI_OUTDUPL_FRAME_INFO,
    };
    use windows::Win32::Graphics::GdiPlus::{
        EncoderParameter, EncoderParameterValueTypeLong, EncoderParameters, EncoderQuality,
        GdipCreateBitmapFromScan0, GdipDisposeImage, GdipGetImageEncoders,
        GdipGetImageEncodersSize, GdipSaveImageToFile, GdiplusShutdown, GdiplusStartup,
        GdiplusStartupInput, GpBitmap, GpImage, ImageCodecInfo, Ok as GpOk, Status,
    };
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

    fn status_ok(s: Status, what: &str) -> anyhow::Result<()> {
        if s == GpOk {
            Ok(())
        } else {
            anyhow::bail!("{what} 失败: Status({})", s.0)
        }
    }

    /// GDI+ JPEG 编码：输入 BGRA8（stride 为 RowPitch）。
    fn encode_jpeg(
        bytes: &[u8],
        w: u32,
        h: u32,
        stride: u32,
        quality: u8,
        path: &std::path::Path,
    ) -> anyhow::Result<()> {
        unsafe {
            let mut token = 0usize;
            let input = GdiplusStartupInput {
                GdiplusVersion: 1,
                ..Default::default()
            };
            status_ok(
                GdiplusStartup(&mut token, &input, std::ptr::null_mut()),
                "GdiplusStartup",
            )?;

            let mut bmp: *mut GpBitmap = std::ptr::null_mut();
            // PixelFormat32bppARGB = 0x26200A（windows-rs 未导出该常量）
            status_ok(
                GdipCreateBitmapFromScan0(
                    w as i32,
                    h as i32,
                    stride as i32,
                    0x26200A,
                    Some(bytes.as_ptr()),
                    &mut bmp,
                ),
                "GdipCreateBitmapFromScan0",
            )?;

            let mut num = 0u32;
            let mut size = 0u32;
            status_ok(
                GdipGetImageEncodersSize(&mut num, &mut size),
                "GdipGetImageEncodersSize",
            )?;
            let mut buf = vec![0u8; size as usize];
            status_ok(
                GdipGetImageEncoders(num, size, buf.as_mut_ptr() as *mut ImageCodecInfo),
                "GdipGetImageEncoders",
            )?;
            let infos =
                std::slice::from_raw_parts(buf.as_ptr() as *const ImageCodecInfo, num as usize);
            let jpeg = infos
                .iter()
                .find(|i| {
                    i.MimeType
                        .to_string()
                        .map(|s| s.eq_ignore_ascii_case("image/jpeg"))
                        .unwrap_or(false)
                })
                .ok_or_else(|| anyhow::anyhow!("找不到 JPEG 编码器"))?;

            let mut q: i32 = quality as i32;
            let params = EncoderParameters {
                Count: 1,
                Parameter: [EncoderParameter {
                    Guid: EncoderQuality,
                    NumberOfValues: 1,
                    Type: EncoderParameterValueTypeLong.0 as u32,
                    Value: &mut q as *mut i32 as *mut c_void,
                }],
            };
            let wide: Vec<u16> = std::os::windows::ffi::OsStrExt::encode_wide(path.as_os_str())
                .chain(std::iter::once(0))
                .collect();
            let st = GdipSaveImageToFile(
                bmp as *mut GpImage,
                PCWSTR(wide.as_ptr()),
                &jpeg.Clsid,
                &params,
            );
            let _ = GdipDisposeImage(bmp as *mut GpImage);
            GdiplusShutdown(token);
            status_ok(st, "GdipSaveImageToFile")
        }
    }

    struct Grabber {
        dup: IDXGIOutputDuplication,
        ctx: ID3D11DeviceContext,
        device: ID3D11Device,
        staging: Option<ID3D11Texture2D>,
        size: (u32, u32),
        format: DXGI_FORMAT,
    }

    impl Grabber {
        fn new() -> anyhow::Result<Self> {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
                let mut chosen: Option<(
                    ID3D11Device,
                    ID3D11DeviceContext,
                    IDXGIOutputDuplication,
                )> = None;
                let mut last_err = None;
                'outer: for ai in 0..4 {
                    let adapter = match factory.EnumAdapters1(ai) {
                        Ok(a) => a,
                        Err(_) => break,
                    };
                    for oi in 0..4 {
                        let output = match adapter.EnumOutputs(oi) {
                            Ok(o) => o,
                            Err(_) => break,
                        };
                        let mut device: Option<ID3D11Device> = None;
                        let mut ctx: Option<ID3D11DeviceContext> = None;
                        let base_adapter: IDXGIAdapter = adapter.cast()?;
                        let create = D3D11CreateDevice(
                            &base_adapter,
                            D3D_DRIVER_TYPE_UNKNOWN,
                            HMODULE::default(),
                            D3D11_CREATE_DEVICE_FLAG(0),
                            Some(&[D3D_FEATURE_LEVEL_11_0]),
                            D3D11_SDK_VERSION,
                            Some(&mut device),
                            None,
                            Some(&mut ctx),
                        );
                        if create.is_err() {
                            last_err = Some(format!("D3D11CreateDevice: {:?}", create.err()));
                            continue;
                        }
                        let output1: IDXGIOutput1 = output.cast()?;
                        let dev = device.unwrap();
                        match output1.DuplicateOutput(&dev) {
                            Ok(dup) => {
                                chosen = Some((dev, ctx.unwrap(), dup));
                                break 'outer;
                            }
                            Err(e) => {
                                last_err =
                                    Some(format!("DuplicateOutput(adapter {ai}, output {oi}): {e}"))
                            }
                        }
                    }
                }
                let (device, ctx, dup): (
                    ID3D11Device,
                    ID3D11DeviceContext,
                    IDXGIOutputDuplication,
                ) = chosen
                    .ok_or_else(|| anyhow::anyhow!("没有可用的输出复制设备: {:?}", last_err))?;
                let od = dup.GetDesc();
                Ok(Self {
                    dup,
                    ctx,
                    device,
                    staging: None,
                    size: (od.ModeDesc.Width, od.ModeDesc.Height),
                    format: od.ModeDesc.Format,
                })
            }
        }

        /// 抓一帧并编码为 JPEG；返回 (宽, 高, 字节数, 各阶段耗时)
        fn grab_jpeg(
            &mut self,
            quality: u8,
            path: &std::path::Path,
        ) -> anyhow::Result<(u32, u32, u64, [u128; 3])> {
            unsafe {
                let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
                let mut resource: Option<IDXGIResource> = None;
                let t0 = Instant::now();
                self.dup.AcquireNextFrame(2000, &mut info, &mut resource)?;
                let tex: ID3D11Texture2D = match resource {
                    Some(r) => r.cast()?,
                    None => anyhow::bail!("AcquireNextFrame 未返回资源"),
                };
                let t1 = Instant::now();

                if self.staging.is_none() {
                    let desc = D3D11_TEXTURE2D_DESC {
                        Width: self.size.0,
                        Height: self.size.1,
                        MipLevels: 1,
                        ArraySize: 1,
                        Format: self.format,
                        SampleDesc: DXGI_SAMPLE_DESC {
                            Count: 1,
                            Quality: 0,
                        },
                        Usage: D3D11_USAGE_STAGING,
                        BindFlags: 0,
                        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                        MiscFlags: 0,
                    };
                    let mut staging: Option<ID3D11Texture2D> = None;
                    self.device
                        .CreateTexture2D(&desc, None, Some(&mut staging))?;
                    self.staging = staging;
                }
                let staging = self.staging.as_ref().unwrap();
                let src: ID3D11Resource = tex.cast()?;
                let dst: ID3D11Resource = staging.cast()?;
                self.ctx.CopyResource(&dst, &src);
                self.ctx.Flush();

                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                self.ctx
                    .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
                let (w, h) = self.size;
                let slice = std::slice::from_raw_parts(
                    mapped.pData as *const u8,
                    (mapped.RowPitch * h) as usize,
                );
                let t2 = Instant::now();
                encode_jpeg(slice, w, h, mapped.RowPitch, quality, path)?;
                let t3 = Instant::now();
                self.ctx.Unmap(staging, 0);
                self.dup.ReleaseFrame()?;
                let len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
                Ok((
                    w,
                    h,
                    len,
                    [
                        (t1 - t0).as_micros(),
                        (t2 - t1).as_micros(),
                        (t3 - t2).as_micros(),
                    ],
                ))
            }
        }
    }

    pub fn run(out: &str, quality: u8, frames: u32) -> anyhow::Result<()> {
        let path = std::path::PathBuf::from(out);
        let mut g = Grabber::new()?;
        println!(
            "INFO  输出 {}x{} format={}（{} 帧，质量 {}）",
            g.size.0, g.size.1, g.format.0, frames, quality
        );
        let mut best = (u128::MAX, u128::MAX, u128::MAX);
        let mut total_bytes = 0u64;
        let mut ok_frames = 0u32;
        for i in 0..frames {
            match g.grab_jpeg(quality, &path) {
                Ok((w, h, len, [acq, map, enc])) => {
                    ok_frames += 1;
                    total_bytes += len;
                    best.0 = best.0.min(acq);
                    best.1 = best.1.min(map);
                    best.2 = best.2.min(enc);
                    if i % 5 == 0 || i + 1 == frames {
                        println!(
                            "frame {i:>3}: {w}x{h} {len:>7} B | acquire {acq:>5}µs map {map:>5}µs encode {enc:>6}µs"
                        );
                    }
                }
                Err(e) => println!("frame {i:>3}: 跳过（{e}）"),
            }
        }
        println!(
            "RESULT frames={ok_frames}/{frames} avg_bytes={} best_acquire={}µs best_map={}µs best_encode={}µs",
            if ok_frames > 0 { total_bytes / ok_frames as u64 } else { 0 },
            best.0, best.1, best.2
        );
        Ok(())
    }
}

#[cfg(windows)]
mod adapters {
    use windows::Win32::NetworkManagement::IpHelper::{
        GetAdaptersAddresses, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER,
        GAA_FLAG_SKIP_MULTICAST, IP_ADAPTER_ADDRESSES_LH,
    };
    use windows::Win32::Networking::WinSock::{AF_INET, AF_UNSPEC, SOCKADDR_IN};

    pub fn run() -> anyhow::Result<()> {
        unsafe {
            let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
            let mut size = 0u32;
            GetAdaptersAddresses(AF_UNSPEC.0 as u32, flags, None, None, &mut size);
            let mut buf = vec![0u8; size as usize];
            let first = buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH;
            let rc = GetAdaptersAddresses(AF_UNSPEC.0 as u32, flags, None, Some(first), &mut size);
            anyhow::ensure!(rc == 0, "GetAdaptersAddresses rc={rc}");

            let mut cur = first;
            while !cur.is_null() {
                let a = &*cur;
                let name = a.FriendlyName.to_string().unwrap_or_default();
                let mut ua = a.FirstUnicastAddress;
                while !ua.is_null() {
                    let u = &*ua;
                    let sa = &u.Address;
                    if !sa.lpSockaddr.is_null() && (*sa.lpSockaddr).sa_family == AF_INET {
                        let sin = &*(sa.lpSockaddr as *const SOCKADDR_IN);
                        let b = &sin.sin_addr.S_un.S_un_b;
                        let ip = u32::from_be_bytes([b.s_b1, b.s_b2, b.s_b3, b.s_b4]);
                        let plen = u.OnLinkPrefixLength as u32;
                        let mask: u32 = if plen == 0 {
                            0
                        } else {
                            u32::MAX << (32 - plen.min(32))
                        };
                        let bcast = ip | !mask;
                        if ip != 0 && ip != 0x7f000001 {
                            println!(
                                "iface={name:<28} ip={}.{}.{}.{}/{plen} broadcast={}.{}.{}.{}",
                                ip >> 24,
                                (ip >> 16) & 0xff,
                                (ip >> 8) & 0xff,
                                ip & 0xff,
                                bcast >> 24,
                                (bcast >> 16) & 0xff,
                                (bcast >> 8) & 0xff,
                                bcast & 0xff
                            );
                        }
                    }
                    ua = u.Next;
                }
                cur = a.Next;
            }
        }
        Ok(())
    }
}

#[cfg(windows)]
mod serve {
    use std::collections::HashMap;
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::net::SocketAddr;
    use std::path::PathBuf;
    use std::sync::Arc;

    use ring::rand::SecureRandom;
    use russh::server::{Auth, ChannelOpenHandle, Msg, Server as _, Session, VerifiedPublicKey};
    use russh::{Channel, ChannelId};
    use russh_sftp::protocol::{
        Attrs, Data, File, FileAttributes, Handle, Name, OpenFlags, Status, StatusCode, Version,
    };
    use tokio::sync::Mutex;

    // ---------- SFTP：真实文件系统（root 内） ----------
    pub struct FsHandler {
        root: Arc<PathBuf>,
        dirs: HashMap<String, Vec<File>>,
        files: HashMap<String, std::fs::File>,
        next: u64,
    }

    impl FsHandler {
        fn new(root: Arc<PathBuf>) -> Self {
            Self {
                root,
                dirs: HashMap::new(),
                files: HashMap::new(),
                next: 1,
            }
        }

        fn resolve(&self, p: &str) -> PathBuf {
            // 虚拟根："/" = 共享根目录；拒绝 ".." 逃逸
            let p = p.replace('\\', "/");
            let rel = p.trim_start_matches('/');
            let mut out = self.root.as_ref().clone();
            for seg in rel.split('/') {
                if seg.is_empty() || seg == "." {
                    continue;
                }
                if seg == ".." {
                    out.pop();
                    continue;
                }
                out.push(seg);
            }
            out
        }

        /// 把真实路径映射回虚拟根形式（"/..."）
        fn virtual_path(&self, real: &std::path::Path) -> String {
            let rel = real
                .strip_prefix(self.root.as_ref())
                .unwrap_or(std::path::Path::new(""));
            let s = rel.to_string_lossy().replace('\\', "/");
            if s.is_empty() {
                "/".to_string()
            } else {
                format!("/{s}")
            }
        }
    }

    impl russh_sftp::server::Handler for FsHandler {
        type Error = StatusCode;

        fn unimplemented(&self) -> Self::Error {
            StatusCode::OpUnsupported
        }

        async fn init(
            &mut self,
            _version: u32,
            _extensions: HashMap<String, String>,
        ) -> Result<Version, Self::Error> {
            Ok(Version::new())
        }

        async fn realpath(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
            let p = self.resolve(&path);
            let virt = self.virtual_path(&p);
            Ok(Name {
                id,
                files: vec![File::dummy(virt)],
            })
        }

        async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, Self::Error> {
            let dir = self.resolve(&path);
            let rd = std::fs::read_dir(&dir).map_err(|_| StatusCode::NoSuchFile)?;
            let mut files = Vec::new();
            for ent in rd.flatten() {
                let md = ent.metadata().ok();
                let is_dir = md.as_ref().map(|m| m.is_dir()).unwrap_or(false);
                let size = md.as_ref().map(|m| m.len()).unwrap_or(0);
                files.push(File {
                    filename: ent.file_name().to_string_lossy().to_string(),
                    longname: String::new(),
                    attrs: FileAttributes {
                        size: Some(size),
                        permissions: Some(if is_dir { 0o040755 } else { 0o100644 }), // S_IFDIR / S_IFREG 类型位
                        ..Default::default()
                    },
                });
            }
            let handle = format!("d:{}:{}", self.next, self.virtual_path(&dir));
            self.next += 1;
            self.dirs.insert(handle.clone(), files);
            Ok(Handle { id, handle })
        }

        async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, Self::Error> {
            match self.dirs.get_mut(&handle) {
                Some(v) if !v.is_empty() => Ok(Name {
                    id,
                    files: std::mem::take(v),
                }),
                _ => Err(StatusCode::Eof),
            }
        }

        async fn open(
            &mut self,
            id: u32,
            filename: String,
            pflags: OpenFlags,
            _attrs: FileAttributes,
        ) -> Result<Handle, Self::Error> {
            let path = self.resolve(&filename);
            let opts = std::fs::OpenOptions::from(pflags);
            let file = opts.open(&path).map_err(|e| {
                println!("open 失败: {} ({e}) pflags={:?}", path.display(), pflags);
                StatusCode::NoSuchFile
            })?;
            let handle = format!("f:{}", self.next);
            self.next += 1;
            self.files.insert(handle.clone(), file);
            Ok(Handle { id, handle })
        }

        async fn read(
            &mut self,
            id: u32,
            handle: String,
            offset: u64,
            len: u32,
        ) -> Result<Data, Self::Error> {
            let f = self.files.get_mut(&handle).ok_or(StatusCode::NoSuchFile)?;
            f.seek(SeekFrom::Start(offset))
                .map_err(|_| StatusCode::Failure)?;
            let mut buf = vec![0u8; len as usize];
            let n = f.read(&mut buf).map_err(|_| StatusCode::Failure)?;
            buf.truncate(n);
            if n == 0 {
                return Err(StatusCode::Eof);
            }
            Ok(Data { id, data: buf })
        }

        async fn write(
            &mut self,
            id: u32,
            handle: String,
            offset: u64,
            data: Vec<u8>,
        ) -> Result<Status, Self::Error> {
            let f = self.files.get_mut(&handle).ok_or(StatusCode::NoSuchFile)?;
            f.seek(SeekFrom::Start(offset))
                .map_err(|_| StatusCode::Failure)?;
            f.write_all(&data).map_err(|_| StatusCode::Failure)?;
            Ok(Status {
                id,
                status_code: StatusCode::Ok,
                error_message: String::new(),
                language_tag: String::new(),
            })
        }

        async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
            self.files.remove(&handle);
            self.dirs.remove(&handle);
            Ok(Status {
                id,
                status_code: StatusCode::Ok,
                error_message: String::new(),
                language_tag: String::new(),
            })
        }

        async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
            let p = self.resolve(&path);
            let md = std::fs::metadata(&p).map_err(|e| {
                println!("stat 失败: {} ({e})", p.display());
                StatusCode::NoSuchFile
            })?;
            let mtime = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as u32);
            Ok(Attrs {
                id,
                attrs: FileAttributes {
                    size: Some(md.len()),
                    permissions: Some(if md.is_dir() { 0o040755 } else { 0o100644 }), // S_IFDIR / S_IFREG 类型位
                    mtime,
                    ..Default::default()
                },
            })
        }

        async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
            self.stat(id, path).await
        }
    }

    // ---------- SSH 服务端 ----------
    /// rand_core 0.10 的 CryptoRng 适配：ssh-key 的 `PrivateKey::random` 需要它。
    struct RingRng(ring::rand::SystemRandom);

    impl RingRng {
        fn new() -> Self {
            Self(ring::rand::SystemRandom::new())
        }
    }

    impl rand_core::TryRng for RingRng {
        type Error = core::convert::Infallible;
        fn try_next_u32(&mut self) -> Result<u32, Self::Error> {
            let mut b = [0u8; 4];
            self.0.fill(&mut b).expect("系统 RNG 不可用");
            Ok(u32::from_ne_bytes(b))
        }
        fn try_next_u64(&mut self) -> Result<u64, Self::Error> {
            let mut b = [0u8; 8];
            self.0.fill(&mut b).expect("系统 RNG 不可用");
            Ok(u64::from_ne_bytes(b))
        }
        fn try_fill_bytes(&mut self, dst: &mut [u8]) -> Result<(), Self::Error> {
            self.0.fill(dst).expect("系统 RNG 不可用");
            Ok(())
        }
    }

    // rand_core 0.10 的 Blanket impl：TryCryptoRng<Error=Infallible> 自动成为 CryptoRng
    impl rand_core::TryCryptoRng for RingRng {}

    pub struct SpikeServer {
        root: Arc<PathBuf>,
    }

    impl russh::server::Server for SpikeServer {
        type Handler = SpikeHandler;
        fn new_client(&mut self, peer: Option<SocketAddr>) -> SpikeHandler {
            println!("INFO  新连接: {peer:?}");
            SpikeHandler {
                root: self.root.clone(),
                channels: Arc::new(Mutex::new(HashMap::new())),
                pty_sizes: HashMap::new(),
                pty_in: HashMap::new(),
                masters: HashMap::new(),
                children: HashMap::new(),
            }
        }
    }

    pub struct SpikeHandler {
        root: Arc<PathBuf>,
        channels: Arc<Mutex<HashMap<ChannelId, Channel<Msg>>>>,
        pty_sizes: HashMap<ChannelId, (u32, u32)>,
        pty_in: HashMap<ChannelId, tokio::sync::mpsc::Sender<Vec<u8>>>,
        masters: HashMap<ChannelId, Arc<Mutex<Box<dyn portable_pty::MasterPty + Send>>>>,
        /// 子进程句柄必须由 handler 持有：drop 会结束 ConPTY 会话
        children: HashMap<ChannelId, Box<dyn portable_pty::Child + Send + Sync>>,
    }

    impl russh::server::Handler for SpikeHandler {
        type Error = anyhow::Error;

        async fn auth_publickey(
            &mut self,
            _user: &str,
            key: VerifiedPublicKey<'_>,
        ) -> Result<Auth, Self::Error> {
            println!("INFO  auth publickey: {:?}", key.algorithm);
            Ok(Auth::Accept)
        }

        async fn channel_open_session(
            &mut self,
            channel: Channel<Msg>,
            reply: ChannelOpenHandle,
            _session: &mut Session,
        ) -> Result<(), Self::Error> {
            self.channels.lock().await.insert(channel.id(), channel);
            reply.accept().await;
            Ok(())
        }

        async fn subsystem_request(
            &mut self,
            id: ChannelId,
            name: &str,
            session: &mut Session,
        ) -> Result<(), Self::Error> {
            println!("INFO  subsystem: {name}");
            if name == "sftp" {
                let channel = self
                    .channels
                    .lock()
                    .await
                    .remove(&id)
                    .expect("channel 已接管");
                session.channel_success(id)?;
                russh_sftp::server::run(channel.into_stream(), FsHandler::new(self.root.clone()))
                    .await;
            } else {
                session.channel_failure(id)?;
            }
            Ok(())
        }

        async fn pty_request(
            &mut self,
            id: ChannelId,
            _term: &str,
            cols: u32,
            rows: u32,
            _pw: u32,
            _ph: u32,
            _modes: &[(russh::Pty, u32)],
            session: &mut Session,
        ) -> Result<(), Self::Error> {
            // 管道 stdin 时客户端可能报 0x0，ConPTY 会 E_INVALIDARG
            self.pty_sizes.insert(id, (cols.max(20), rows.max(5)));
            session.channel_success(id)?;
            Ok(())
        }

        async fn shell_request(
            &mut self,
            id: ChannelId,
            session: &mut Session,
        ) -> Result<(), Self::Error> {
            println!("INFO  shell_request {id:?}");
            let (cols, rows) = self.pty_sizes.get(&id).copied().unwrap_or((80, 24));
            let cols = cols.max(20);
            let rows = rows.max(5);
            // 按 PATH 查存在性（不能用 `pwsh -?` 探测：它会挂住并阻塞请求）
            let shell = if let Ok(s) = std::env::var("SPIKE_SHELL") {
                s
            } else {
                let path = std::env::var_os("PATH").unwrap_or_default();
                let mut found = std::env::var("ComSpec")
                    .unwrap_or_else(|_| "C:\\Windows\\System32\\cmd.exe".to_string());
                for cand in ["pwsh.exe", "powershell.exe"] {
                    if let Some(p) = std::env::split_paths(&path)
                        .map(|p| p.join(cand))
                        .find(|p| p.is_file())
                    {
                        found = p.to_string_lossy().to_string();
                        break;
                    }
                }
                found
            };
            let pty = portable_pty::native_pty_system();
            let pair = pty
                .openpty(portable_pty::PtySize {
                    rows: rows as u16,
                    cols: cols as u16,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .map_err(|e| {
                    println!("openpty 失败: {e}");
                    e
                })?;
            let mut cmd = portable_pty::CommandBuilder::new(&shell);
            cmd.arg("-NoLogo");
            let child = pair.slave.spawn_command(cmd).map_err(|e| {
                println!("spawn {shell} 失败: {e}");
                e
            })?;
            drop(pair.slave);

            let mut reader = pair.master.try_clone_reader()?;
            let mut writer = pair.master.take_writer()?;

            // PTY 输出 → SSH channel（Handle::data 是 async，经 mpsc 转交）
            let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
            std::thread::spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            println!("PTY out {n} B");
                            if out_tx.blocking_send(buf[..n].to_vec()).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            });
            let handle = session.handle();
            tokio::spawn(async move {
                while let Some(chunk) = out_rx.recv().await {
                    let n = chunk.len();
                    match handle.data(id, chunk).await {
                        Ok(()) => println!("SSH out {n} B"),
                        Err(_) => {
                            println!("SSH out 失败");
                            break;
                        }
                    }
                }
                let _ = handle.eof(id).await;
            });

            // SSH 输入 → PTY writer
            let (in_tx, mut in_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
            std::thread::spawn(move || {
                while let Some(chunk) = in_rx.blocking_recv() {
                    if writer.write_all(&chunk).is_err() {
                        break;
                    }
                }
            });
            self.pty_in.insert(id, in_tx);
            self.masters.insert(id, Arc::new(Mutex::new(pair.master)));
            self.children.insert(id, child);
            println!("INFO  shell: channel {id:?} shell={shell} {cols}x{rows}");
            Ok(())
        }

        async fn data(
            &mut self,
            id: ChannelId,
            data: &[u8],
            _session: &mut Session,
        ) -> Result<(), Self::Error> {
            println!("PTY in {} B", data.len());
            if let Some(tx) = self.pty_in.get(&id) {
                let _ = tx.send(data.to_vec()).await;
            }
            Ok(())
        }

        async fn window_change_request(
            &mut self,
            id: ChannelId,
            cols: u32,
            rows: u32,
            _pw: u32,
            _ph: u32,
            _session: &mut Session,
        ) -> Result<(), Self::Error> {
            if let Some(m) = self.masters.get(&id) {
                let _ = m.lock().await.resize(portable_pty::PtySize {
                    rows: rows.max(5) as u16,
                    cols: cols.max(20) as u16,
                    pixel_width: 0,
                    pixel_height: 0,
                });
            }
            Ok(())
        }

        async fn channel_close(
            &mut self,
            id: ChannelId,
            _session: &mut Session,
        ) -> Result<(), Self::Error> {
            self.pty_in.remove(&id);
            self.masters.remove(&id);
            self.pty_sizes.remove(&id);
            self.children.remove(&id);
            println!("INFO  channel {id:?} 关闭");
            Ok(())
        }
    }

    pub fn run(port: u16, root: String) -> anyhow::Result<()> {
        let root = std::fs::canonicalize(&root)?;
        println!("INFO  SFTP root = {}", root.display());
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(async move {
            let key_path = std::env::temp_dir().join("spike-hostkey");
            let host_key = if key_path.is_file() {
                russh::keys::PrivateKey::from_openssh(std::fs::read_to_string(&key_path)?)?
            } else {
                let k = russh::keys::PrivateKey::random(
                    &mut RingRng::new(),
                    russh::keys::Algorithm::Ed25519,
                )?;
                std::fs::write(
                    &key_path,
                    k.to_openssh(russh::keys::ssh_key::LineEnding::LF)?
                        .as_bytes(),
                )?;
                k
            };
            let config = Arc::new(russh::server::Config {
                keys: vec![host_key],
                auth_rejection_time: std::time::Duration::from_millis(50),
                auth_rejection_time_initial: Some(std::time::Duration::ZERO),
                ..Default::default()
            });
            let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
            println!("INFO  listening on 0.0.0.0:{port}");
            let server = SpikeServer {
                root: Arc::new(root),
            };
            let mut sh = server;
            sh.run_on_socket(config, &listener).await?;
            Ok(())
        })
    }
}
