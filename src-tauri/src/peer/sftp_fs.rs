//! 对端 SFTP 的本地文件系统后端（`russh_sftp::server::Handler`）。
//!
//! 虚拟根语义（`realpath` 必须返回 POSIX 路径，客户端会按 `/` 逐段 glob）：
//! - `peer_share_root` 非空 → `/` 映射到该目录，`..` 不能越过共享根（越界 `EPeerSftpRoot`）；
//! - `peer_share_root` 为空 → 全盘模式：`/` 列出各盘符，`/C/Users/...` 映射 `C:\Users\...`；
//! - **保留路径 `/peers/inbox`** → 本机数据目录的配置收件箱。
//!
//! 授权（[`Grants`]，每次连接按 `peers.toml` 当前记录实时读取）：
//! - `files`：共享根可自由读写；
//! - `config`：允许写入 `/peers/inbox`（**与 files 独立**——授予了文件互传也不等于允许推送配置，
//!   撤销 config 后即使 files 仍在，写收件箱也会被拒绝）；
//! - 两者都无 → 连 SFTP 子系统都不允许（服务端直接拒绝）。
//!
//! 注意：`/peers`、`/peers/inbox` 是**保留路径**，共享根里恰好叫 `peers` 的目录会被遮蔽
//! （收件箱优先）；这样配置推送不会把文件混进用户共享目录。
//!
//! 权限位必须带 `S_IFREG`/`S_IFDIR` 类型位（0o100644 / 0o040755）：OpenSSH sftp 客户端
//! 依赖它区分文件/目录，缺了会把文件当"不存在"。

use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use russh_sftp::protocol::{
    Attrs, Data, File, FileAttributes, Handle, Name, OpenFlags, Status, StatusCode, Version,
};

const S_IFREG: u32 = 0o100000;
const S_IFDIR: u32 = 0o040000;
const PERM_FILE: u32 = S_IFREG | 0o644;
const PERM_DIR: u32 = S_IFDIR | 0o755;
/// 收件箱单文件上限：与备份导入上限一致（8 MiB）。
const MAX_INBOX_BYTES: u64 = 8 * 1024 * 1024;
/// 单次 `SSH_FXP_READ` 的最大返回字节数：客户端可请求 4 GiB，服务端不能照单分配。
const MAX_READ_BYTES: usize = 256 * 1024;

/// 句柄归属：共享根还是配置收件箱。授权撤销后必须按句柄类型逐请求复核，
/// 不能只看 `files || config` 任一仍为真。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HandleKind {
    Shared,
    Inbox,
}

/// 对端授权（服务端在 `subsystem_request` 时从 `peers.toml` 当前记录读取）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grants {
    /// 文件互传：共享根可自由读写。
    pub files: bool,
    /// 配置推送：允许写 `/peers/inbox`。
    pub config: bool,
}

impl Grants {
    pub fn any(self) -> bool {
        self.files || self.config
    }
}

/// 收件箱写入完成通知（在 `close` 时 emit `peer://inbox`）。
#[derive(Clone)]
pub struct InboxNotify {
    pub app: tauri::AppHandle,
    pub peer_name: String,
}

/// 共享根：目录或全盘模式。
#[derive(Debug, Clone)]
pub enum Root {
    Dir(PathBuf),
    Drives,
}

impl Root {
    pub fn from_share_root(share_root: &str) -> Result<Self, String> {
        let s = share_root.trim();
        if s.is_empty() {
            return Ok(Root::Drives);
        }
        let p = std::fs::canonicalize(s).map_err(|e| format!("共享目录不可用（{s}）: {e}"))?;
        if !p.is_dir() {
            return Err(format!("共享目录不是文件夹: {}", p.display()));
        }
        // 与 confine()/virtual_of() 使用同一 plain 基准；否则 Windows 上
        // `realpath` 会因 `\\?\` 前缀比较失败而永远回退 `/`。
        Ok(Root::Dir(plain(&p)))
    }
}

/* ---------------- symlink/junction 逃逸防护 ---------------- */

/// 把 `\\?\C:\...` 规范化前缀剥掉，得到可与用户路径比较的普通形式。
fn plain(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        p.to_path_buf()
    }
}

/// 路径边界校验：词法归一化挡不住符号链接/junction——共享目录里一个
/// 指向 `C:\Users` 的 junction 就能让已配对设备越出共享根读写全盘。
///
/// 做法：对**最近存在的祖先**做 `canonicalize`（解析沿途所有链接），再确认
/// 结果（拼回不存在的尾部后）仍然落在 `base` 之内。不存在的尾部不可能是
/// 链接，因此只需解析存在部分。全盘模式（`base=None`）不做限制，但盘符
/// 之外的 UNC/设备路径仍拒绝。
fn confine(path: &Path, base: Option<&Path>) -> Result<PathBuf, StatusCode> {
    // 拒绝盘符路径之外的奇怪根（UNC / 设备命名空间）：全盘模式也只放行 `X:\...`。
    // 注意 `\\?\C:\`（canonicalize 产物）的 prefix 是 VerbatimDisk，同样放行；
    // `\\?\UNC\...` 与 `\\server\share`（UNC）则拒绝。
    let looks_local = path.components().next().is_some_and(|c| {
        matches!(
            c,
            std::path::Component::Prefix(ref pref)
                if matches!(
                    pref.kind(),
                    std::path::Prefix::Disk(_) | std::path::Prefix::VerbatimDisk(_)
                )
        )
    });
    if !looks_local && !path.is_relative() {
        return Err(StatusCode::PermissionDenied);
    }
    let Some(base) = base else {
        return Ok(path.to_path_buf()); // 全盘模式：只做了上面的根类型过滤
    };
    let base_canon = std::fs::canonicalize(base)
        .map(|p| plain(&p))
        .map_err(|_| StatusCode::Failure)?;
    // 找最近存在的祖先：路径尾段可能是待创建文件（尚不存在），不能整条 canonicalize
    let mut existing = path.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    while !existing.exists() {
        let Some(name) = existing.file_name().map(|n| n.to_os_string()) else {
            break; // 到根仍不存在：无法解析，按原样比较（必不匹配，下面会拒绝）
        };
        tail.push(name);
        if !existing.pop() {
            break;
        }
    }
    let canon = match std::fs::canonicalize(&existing) {
        Ok(c) => plain(&c),
        Err(_) => return Err(StatusCode::NoSuchFile),
    };
    // 拼回不存在的尾部（尾段不是链接，词法拼接安全）
    let mut resolved = canon;
    for seg in tail.iter().rev() {
        resolved.push(seg);
    }
    // 前缀校验：必须恰好等于 base 或在其内部（按组件比较，避免字符串前缀的
    // `C:\foo` vs `C:\foobar` 误判）。
    let inside = resolved == base_canon
        || resolved
            .strip_prefix(&base_canon)
            .is_ok_and(|rest| rest.components().next().is_some());
    if !inside {
        applog!(
            "[peer] SFTP 路径越界已拒绝: {} 解析为 {}（共享根 {}）",
            path.display(),
            resolved.display(),
            base_canon.display()
        );
        return Err(StatusCode::PermissionDenied);
    }
    Ok(resolved)
}

/// 保留路径 `/peers/inbox` 的解析结果。
enum InboxPath {
    /// 虚拟根（只在没有文件授权时用于合成 `peers` 列表）
    Root,
    Peers,
    Inbox,
    File(PathBuf),
}

pub struct FsHandler {
    root: Arc<Root>,
    grants: Grants,
    /// 收件箱真实目录（`/peers/inbox` 的映射目标）
    inbox: PathBuf,
    notify: Option<InboxNotify>,
    /// handle → 已列出但未发完的目录条目
    dirs: HashMap<String, Vec<File>>,
    /// handle → 目录所属授权域
    dir_kinds: HashMap<String, HandleKind>,
    /// handle → 打开的文件
    files: HashMap<String, std::fs::File>,
    /// handle → 文件所属授权域
    file_kinds: HashMap<String, HandleKind>,
    /// 收件箱写 handle → 累计写入字节
    inbox_writes: HashMap<String, u64>,
    /// 收件箱文件 handle → 文件名（close 时用于 emit 事件）
    inbox_names: HashMap<String, String>,
    /// handle → 收件箱文件（open 时判定）
    inbox_handles: HashSet<String>,
    /// 实时授权复核（peers.toml）；None 表示不复核（单测）
    checker: Option<crate::peer::server::GrantCheck>,
    next: u64,
}

impl FsHandler {
    pub fn new(
        root: Arc<Root>,
        grants: Grants,
        inbox: PathBuf,
        notify: Option<InboxNotify>,
        checker: Option<crate::peer::server::GrantCheck>,
    ) -> Self {
        Self {
            root,
            grants,
            inbox,
            notify,
            dirs: HashMap::new(),
            dir_kinds: HashMap::new(),
            files: HashMap::new(),
            file_kinds: HashMap::new(),
            inbox_writes: HashMap::new(),
            inbox_names: HashMap::new(),
            inbox_handles: HashSet::new(),
            checker,
            next: 1,
        }
    }

    /// 每个 SFTP 请求都复核 peers.toml：设备被删除或 files/config 全撤销即拒绝。
    /// 这样“复用旧 SFTP 会话”也无法绕过撤销，传输中途撤销同样立刻生效。
    async fn refresh_grants(&mut self) -> Result<(), StatusCode> {
        let Some(checker) = &self.checker else {
            return Ok(());
        };
        let Some(p) = checker.current().await else {
            return Err(StatusCode::PermissionDenied);
        };
        self.grants = Grants {
            files: p.grant_files,
            config: p.grant_config,
        };
        if !self.grants.any() {
            return Err(StatusCode::PermissionDenied);
        }
        Ok(())
    }

    /// 句柄级授权：共享根句柄要求 `grant_files`，收件箱句柄要求 `grant_config`。
    fn ensure_handle_kind(&self, kind: Option<HandleKind>) -> Result<(), StatusCode> {
        match kind {
            Some(HandleKind::Shared) if self.grants.files => Ok(()),
            Some(HandleKind::Inbox) if self.grants.config => Ok(()),
            _ => Err(StatusCode::PermissionDenied),
        }
    }

    fn next_handle(&mut self, prefix: &str) -> String {
        let h = format!("{prefix}:{}", self.next);
        self.next += 1;
        h
    }

    /// 路径归一化：去掉空段 / `.`，`..` 回退一层（不会越过虚拟根）。
    fn segments(virtual_path: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for seg in virtual_path.replace('\\', "/").split('/') {
            match seg {
                "" | "." => {}
                ".." => {
                    out.pop();
                }
                s => out.push(s.to_string()),
            }
        }
        out
    }

    /// 保留命名空间：`/`（仅无文件授权时）、`/peers`、`/peers/inbox[/...]`。
    fn inbox_target(&self, virtual_path: &str) -> Option<InboxPath> {
        let segs = Self::segments(virtual_path);
        match segs.as_slice() {
            [] if !self.grants.files => Some(InboxPath::Root),
            [a] if a == "peers" => Some(InboxPath::Peers),
            [a, b] if a == "peers" && b == "inbox" => Some(InboxPath::Inbox),
            [a, b, rest @ ..] if a == "peers" && b == "inbox" => {
                let mut p = self.inbox.clone();
                for s in rest {
                    p.push(s);
                }
                Some(InboxPath::File(p))
            }
            _ => None,
        }
    }

    /// 保留路径是否因「未授予配置推送」而必须拒绝。
    fn inbox_denied(&self, target: &InboxPath) -> bool {
        !self.grants.config && !matches!(target, InboxPath::Root)
    }

    fn virtual_inbox_path(&self, virtual_path: &str) -> String {
        let segs = Self::segments(virtual_path);
        format!("/{}", segs.join("/"))
    }

    /// 虚拟路径 → 本机路径（拒绝越界/未授权；链接解析后仍必须在边界内）。
    fn resolve(&self, virtual_path: &str) -> Result<PathBuf, StatusCode> {
        if let Some(target) = self.inbox_target(virtual_path) {
            if self.inbox_denied(&target) {
                return Err(StatusCode::PermissionDenied);
            }
            return match target {
                InboxPath::File(p) => {
                    let base = std::fs::canonicalize(&self.inbox).map(|b| plain(&b)).ok();
                    confine(&p, base.as_deref()).map_err(|_| StatusCode::PermissionDenied)
                }
                InboxPath::Inbox | InboxPath::Peers | InboxPath::Root => Err(StatusCode::Failure),
            };
        }
        if !self.grants.files {
            return Err(StatusCode::PermissionDenied);
        }
        let norm = virtual_path.replace('\\', "/");
        let segs: Vec<&str> = norm
            .split('/')
            .filter(|s| !s.is_empty() && *s != ".")
            .collect();
        match self.root.as_ref() {
            Root::Dir(base) => {
                let mut out = base.clone();
                for seg in segs {
                    if seg == ".." {
                        if out == *base {
                            return Err(StatusCode::PermissionDenied);
                        }
                        out.pop();
                    } else {
                        out.push(seg);
                    }
                }
                // 共享目录内的 symlink/junction 可把词法上合法的子路径
                // 解析到边界之外——canonicalize 最近存在祖先后复核前缀。
                confine(&out, Some(base))
            }
            Root::Drives => {
                if segs.is_empty() {
                    return Ok(PathBuf::from(r"\")); // 虚拟根占位（不会真正访问）
                }
                let drive = segs[0].trim_end_matches(':').to_ascii_uppercase();
                let mut chars = drive.chars();
                let letter = chars.next().ok_or(StatusCode::NoSuchFile)?;
                if !letter.is_ascii_alphabetic() || chars.next().is_some() {
                    return Err(StatusCode::NoSuchFile);
                }
                let mut out = PathBuf::from(format!("{letter}:\\"));
                for seg in &segs[1..] {
                    if *seg == ".." {
                        if out.parent().is_none() {
                            return Err(StatusCode::PermissionDenied);
                        }
                        out.pop();
                    } else {
                        out.push(seg);
                    }
                }
                // 全盘模式本就允许所有盘符，confine 只过滤 UNC/设备路径
                confine(&out, None)
            }
        }
    }

    /// 本机路径 → 虚拟路径（用于 realpath / handle 命名）。
    fn virtual_of(&self, real: &Path) -> String {
        if let Ok(rel) = real.strip_prefix(&self.inbox) {
            let s = rel.to_string_lossy().replace('\\', "/");
            return if s.is_empty() {
                "/peers/inbox".to_string()
            } else {
                format!("/peers/inbox/{s}")
            };
        }
        match self.root.as_ref() {
            Root::Dir(base) => {
                let rel = real.strip_prefix(base).unwrap_or(Path::new(""));
                let s = rel.to_string_lossy().replace('\\', "/");
                if s.is_empty() {
                    "/".to_string()
                } else {
                    format!("/{s}")
                }
            }
            Root::Drives => {
                let s = real.to_string_lossy().replace('\\', "/");
                // "C:/Users/x" → "/C/Users/x"
                match s.split_once(':') {
                    Some((drive, rest)) => format!("/{drive}{rest}"),
                    None => format!("/{s}"),
                }
            }
        }
    }

    fn is_virtual_root(&self, virtual_path: &str) -> bool {
        self.grants.files
            && matches!(self.root.as_ref(), Root::Drives)
            && virtual_path.trim_matches('/').is_empty()
    }

    fn drive_entries(&self) -> Vec<File> {
        ('A'..='Z')
            .filter(|c| Path::new(&format!("{c}:\\")).exists())
            .map(|c| File {
                filename: c.to_string(),
                longname: String::new(),
                attrs: FileAttributes {
                    permissions: Some(PERM_DIR),
                    ..Default::default()
                },
            })
            .collect()
    }

    fn file_attrs(&self, md: &std::fs::Metadata) -> FileAttributes {
        FileAttributes {
            size: Some(md.len()),
            permissions: Some(if md.is_dir() { PERM_DIR } else { PERM_FILE }),
            mtime: md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as u32),
            ..Default::default()
        }
    }

    fn ok_status(id: u32) -> Status {
        Status {
            id,
            status_code: StatusCode::Ok,
            error_message: String::new(),
            language_tag: String::new(),
        }
    }

    /// 列出真实目录（共享给 opendir 的收件箱/共享根分支）。
    fn list_real_dir(
        &mut self,
        id: u32,
        dir: &Path,
        kind: HandleKind,
    ) -> Result<Handle, StatusCode> {
        let rd = std::fs::read_dir(dir).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => StatusCode::NoSuchFile,
            std::io::ErrorKind::PermissionDenied => StatusCode::PermissionDenied,
            _ => StatusCode::Failure,
        })?;
        let mut files = Vec::new();
        for ent in rd.flatten() {
            let md = ent.metadata().ok();
            let attrs = md.as_ref().map(|m| self.file_attrs(m)).unwrap_or_default();
            files.push(File {
                filename: ent.file_name().to_string_lossy().to_string(),
                longname: String::new(),
                attrs,
            });
        }
        let h = self.next_handle("d");
        self.dirs.insert(h.clone(), files);
        self.dir_kinds.insert(h.clone(), kind);
        Ok(Handle { id, handle: h })
    }

    fn synthetic_dir(&mut self, id: u32, name: &str) -> Result<Handle, StatusCode> {
        let h = self.next_handle("d");
        self.dirs.insert(
            h.clone(),
            vec![File {
                filename: name.to_string(),
                longname: String::new(),
                attrs: FileAttributes {
                    permissions: Some(PERM_DIR),
                    ..Default::default()
                },
            }],
        );
        self.dir_kinds.insert(h.clone(), HandleKind::Inbox);
        Ok(Handle { id, handle: h })
    }

    fn stat_target(&self, id: u32, path: &str) -> Option<Result<Attrs, StatusCode>> {
        let target = self.inbox_target(path)?;
        if self.inbox_denied(&target) {
            return Some(Err(StatusCode::PermissionDenied));
        }
        Some(match target {
            InboxPath::Root | InboxPath::Peers => Ok(Attrs {
                id,
                attrs: FileAttributes {
                    permissions: Some(PERM_DIR),
                    ..Default::default()
                },
            }),
            InboxPath::Inbox => {
                let md = std::fs::metadata(&self.inbox).ok();
                Ok(Attrs {
                    id,
                    attrs: md
                        .as_ref()
                        .map(|m| self.file_attrs(m))
                        .unwrap_or(FileAttributes {
                            permissions: Some(PERM_DIR),
                            ..Default::default()
                        }),
                })
            }
            InboxPath::File(p) => {
                // stat 也过边界校验（探测 symlink 目标是否存在同样算越界信息泄露）
                let base = std::fs::canonicalize(&self.inbox).map(|b| plain(&b)).ok();
                let confined = match confine(&p, base.as_deref()) {
                    Ok(c) => c,
                    Err(_) => return Some(Err(StatusCode::PermissionDenied)),
                };
                std::fs::metadata(&confined)
                    .map(|m| Attrs {
                        id,
                        attrs: self.file_attrs(&m),
                    })
                    .map_err(|e| match e.kind() {
                        std::io::ErrorKind::NotFound => StatusCode::NoSuchFile,
                        std::io::ErrorKind::PermissionDenied => StatusCode::PermissionDenied,
                        _ => StatusCode::Failure,
                    })
            }
        })
    }

    /// 收件箱落盘完成：推 `peer://inbox`（payload 含摘要，前端直接展示 + 可导入）。
    fn notify_inbox(&self, name: &str) {
        let Some(n) = &self.notify else { return };
        match crate::peer::inbox::item(name) {
            Ok(item) => {
                applog!(
                    "[peer] 收到来自 {} 的配置（{} 台主机）: {}",
                    n.peer_name,
                    item.hosts,
                    name
                );
                crate::events::emit(
                    &n.app,
                    crate::events::PEER_INBOX,
                    crate::peer::inbox::InboxNotice { item },
                );
            }
            Err(e) => applog!("[peer] 收件箱条目解析失败（{name}）: {e}"),
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
        self.refresh_grants().await?;
        if let Some(target) = self.inbox_target(&path) {
            if self.inbox_denied(&target) {
                return Err(StatusCode::PermissionDenied);
            }
            let v = match target {
                InboxPath::Root => "/".to_string(),
                InboxPath::Peers => "/peers".to_string(),
                InboxPath::Inbox => "/peers/inbox".to_string(),
                InboxPath::File(_) => self.virtual_inbox_path(&path),
            };
            return Ok(Name {
                id,
                files: vec![File::dummy(v)],
            });
        }
        if !self.grants.files {
            return Err(StatusCode::PermissionDenied);
        }
        if self.is_virtual_root(&path) {
            return Ok(Name {
                id,
                files: vec![File::dummy("/")],
            });
        }
        let real = self.resolve(&path)?;
        Ok(Name {
            id,
            files: vec![File::dummy(self.virtual_of(&real))],
        })
    }

    async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, Self::Error> {
        self.refresh_grants().await?;
        if let Some(target) = self.inbox_target(&path) {
            if self.inbox_denied(&target) {
                return Err(StatusCode::PermissionDenied);
            }
            return match target {
                InboxPath::Root => self.synthetic_dir(id, "peers"),
                InboxPath::Peers => self.synthetic_dir(id, "inbox"),
                InboxPath::Inbox => self.list_real_dir(id, &self.inbox.clone(), HandleKind::Inbox),
                // 收件箱子目录同样过链接解析 + 前缀校验
                InboxPath::File(p) => {
                    let base = std::fs::canonicalize(&self.inbox).map(|b| plain(&b)).ok();
                    let confined =
                        confine(&p, base.as_deref()).map_err(|_| StatusCode::PermissionDenied)?;
                    self.list_real_dir(id, &confined, HandleKind::Inbox)
                }
            };
        }
        if !self.grants.files {
            return Err(StatusCode::PermissionDenied);
        }
        if self.is_virtual_root(&path) {
            let entries = self.drive_entries();
            let h = self.next_handle("droot");
            self.dirs.insert(h.clone(), entries);
            self.dir_kinds.insert(h.clone(), HandleKind::Shared);
            return Ok(Handle { id, handle: h });
        }
        let dir = self.resolve(&path)?;
        self.list_real_dir(id, &dir, HandleKind::Shared)
    }

    async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, Self::Error> {
        self.refresh_grants().await?;
        self.ensure_handle_kind(self.dir_kinds.get(&handle).copied())?;
        match self.dirs.get_mut(&handle) {
            Some(v) if !v.is_empty() => Ok(Name {
                id,
                files: std::mem::take(v),
            }),
            Some(_) => Err(StatusCode::Eof),
            None => Err(StatusCode::NoSuchFile),
        }
    }

    async fn open(
        &mut self,
        id: u32,
        filename: String,
        pflags: OpenFlags,
        _attrs: FileAttributes,
    ) -> Result<Handle, Self::Error> {
        self.refresh_grants().await?;
        let target = self.inbox_target(&filename);
        let is_inbox = matches!(target, Some(InboxPath::File(_)));
        let real = match target {
            Some(t) if self.inbox_denied(&t) => return Err(StatusCode::PermissionDenied),
            // 收件箱写入同样过链接解析 + 前缀校验（写 symlink 即写边界外文件）
            Some(InboxPath::File(p)) => {
                let base = std::fs::canonicalize(&self.inbox).map(|b| plain(&b)).ok();
                confine(&p, base.as_deref()).map_err(|_| StatusCode::PermissionDenied)?
            }
            Some(_) => return Err(StatusCode::Failure), // 目录不能 open
            None => {
                if !self.grants.files {
                    return Err(StatusCode::PermissionDenied);
                }
                self.resolve(&filename)?
            }
        };
        if is_inbox {
            if let Some(parent) = real.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
        }
        let opts = std::fs::OpenOptions::from(pflags);
        let file = opts.open(&real).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => StatusCode::NoSuchFile,
            std::io::ErrorKind::PermissionDenied => StatusCode::PermissionDenied,
            _ => StatusCode::Failure,
        })?;
        let h = self.next_handle("f");
        self.files.insert(h.clone(), file);
        self.file_kinds.insert(
            h.clone(),
            if is_inbox {
                HandleKind::Inbox
            } else {
                HandleKind::Shared
            },
        );
        if is_inbox {
            self.inbox_handles.insert(h.clone());
            if let Some(name) = real.file_name() {
                self.inbox_names
                    .insert(h.clone(), name.to_string_lossy().to_string());
            }
        }
        Ok(Handle { id, handle: h })
    }

    async fn read(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        len: u32,
    ) -> Result<Data, Self::Error> {
        self.refresh_grants().await?;
        self.ensure_handle_kind(self.file_kinds.get(&handle).copied())?;
        let f = self.files.get_mut(&handle).ok_or(StatusCode::NoSuchFile)?;
        f.seek(SeekFrom::Start(offset))
            .map_err(|_| StatusCode::Failure)?;
        let want = (len as usize).min(MAX_READ_BYTES);
        let mut buf = vec![0u8; want];
        let n = f.read(&mut buf).map_err(|_| StatusCode::Failure)?;
        if n == 0 {
            return Err(StatusCode::Eof);
        }
        buf.truncate(n);
        Ok(Data { id, data: buf })
    }

    async fn write(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<Status, Self::Error> {
        self.refresh_grants().await?;
        self.ensure_handle_kind(self.file_kinds.get(&handle).copied())?;
        let write_len = data.len() as u64;
        let end = offset
            .checked_add(write_len)
            .ok_or(StatusCode::PermissionDenied)?;
        if self.inbox_handles.contains(&handle) && end > MAX_INBOX_BYTES {
            return Err(StatusCode::PermissionDenied);
        }
        let f = self.files.get_mut(&handle).ok_or(StatusCode::NoSuchFile)?;
        f.seek(SeekFrom::Start(offset))
            .map_err(|_| StatusCode::Failure)?;
        f.write_all(&data).map_err(|_| StatusCode::Failure)?;
        if self.inbox_handles.contains(&handle) {
            if f.metadata().map(|m| m.len()).unwrap_or(0) > MAX_INBOX_BYTES {
                let _ = f.set_len(MAX_INBOX_BYTES);
                return Err(StatusCode::PermissionDenied);
            }
            let written = self.inbox_writes.entry(handle.clone()).or_insert(0);
            *written = (*written).max(end);
        }
        Ok(Self::ok_status(id))
    }

    async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
        self.file_kinds.remove(&handle);
        self.dir_kinds.remove(&handle);
        let existed = self.files.remove(&handle).is_some() || self.dirs.remove(&handle).is_some();
        if self.inbox_handles.remove(&handle) {
            let wrote = self.inbox_writes.remove(&handle).is_some();
            if wrote {
                if let Some(name) = self.inbox_names.remove(&handle) {
                    self.notify_inbox(&name);
                }
            }
        }
        if existed {
            Ok(Self::ok_status(id))
        } else {
            Err(StatusCode::NoSuchFile)
        }
    }

    async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, Self::Error> {
        self.refresh_grants().await?;
        self.ensure_handle_kind(self.file_kinds.get(&handle).copied())?;
        let f = self.files.get(&handle).ok_or(StatusCode::NoSuchFile)?;
        let md = f.metadata().map_err(|_| StatusCode::Failure)?;
        Ok(Attrs {
            id,
            attrs: self.file_attrs(&md),
        })
    }

    async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        self.refresh_grants().await?;
        if let Some(r) = self.stat_target(id, &path) {
            return r;
        }
        if !self.grants.files {
            return Err(StatusCode::PermissionDenied);
        }
        if self.is_virtual_root(&path) {
            return Ok(Attrs {
                id,
                attrs: FileAttributes {
                    permissions: Some(PERM_DIR),
                    ..Default::default()
                },
            });
        }
        let real = self.resolve(&path)?;
        let md = std::fs::metadata(&real).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => StatusCode::NoSuchFile,
            std::io::ErrorKind::PermissionDenied => StatusCode::PermissionDenied,
            _ => StatusCode::Failure,
        })?;
        Ok(Attrs {
            id,
            attrs: self.file_attrs(&md),
        })
    }

    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        // 保留路径（inbox）由服务端合成，仍走 stat；普通路径不能跟随 symlink。
        if self.inbox_target(&path).is_some() {
            return self.stat(id, path).await;
        }
        self.refresh_grants().await?;
        if !self.grants.files {
            return Err(StatusCode::PermissionDenied);
        }
        if self.is_virtual_root(&path) {
            return self.stat(id, path).await;
        }
        let real = self.resolve(&path)?;
        let md = std::fs::symlink_metadata(&real).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => StatusCode::NoSuchFile,
            std::io::ErrorKind::PermissionDenied => StatusCode::PermissionDenied,
            _ => StatusCode::Failure,
        })?;
        Ok(Attrs {
            id,
            attrs: self.file_attrs(&md),
        })
    }

    async fn setstat(
        &mut self,
        id: u32,
        path: String,
        attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        self.refresh_grants().await?;
        let real = self.resolve(&path)?;
        if let Some(size) = attrs.size {
            let f = std::fs::OpenOptions::new()
                .write(true)
                .open(&real)
                .map_err(|_| StatusCode::Failure)?;
            f.set_len(size).map_err(|_| StatusCode::Failure)?;
        }
        #[cfg(unix)]
        if let Some(mode) = attrs.permissions {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&real, std::fs::Permissions::from_mode(mode & 0o777))
                .map_err(|_| StatusCode::Failure)?;
        }
        // mtime/atime 与 chown 不做（Windows 共享根没有对应语义）。
        Ok(Self::ok_status(id))
    }

    async fn mkdir(
        &mut self,
        id: u32,
        path: String,
        _attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        self.refresh_grants().await?;
        // 保留命名空间：`/peers` / `/peers/inbox` 由服务端维护，客户端 mkdir 视为成功
        match self.inbox_target(&path) {
            Some(t) if self.inbox_denied(&t) => return Err(StatusCode::PermissionDenied),
            Some(InboxPath::Root) | Some(InboxPath::Peers) | Some(InboxPath::Inbox) => {
                return Ok(Self::ok_status(id));
            }
            Some(InboxPath::File(p)) => {
                let base = std::fs::canonicalize(&self.inbox).map(|b| plain(&b)).ok();
                let confined =
                    confine(&p, base.as_deref()).map_err(|_| StatusCode::PermissionDenied)?;
                return match std::fs::create_dir_all(&confined) {
                    Ok(()) => Ok(Self::ok_status(id)),
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                        Ok(Self::ok_status(id))
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                        Err(StatusCode::PermissionDenied)
                    }
                    Err(_) => Err(StatusCode::Failure),
                };
            }
            None => {}
        }
        if !self.grants.files {
            return Err(StatusCode::PermissionDenied);
        }
        let real = self.resolve(&path)?;
        std::fs::create_dir(&real).map_err(|e| match e.kind() {
            std::io::ErrorKind::AlreadyExists => StatusCode::Failure,
            std::io::ErrorKind::PermissionDenied => StatusCode::PermissionDenied,
            _ => StatusCode::Failure,
        })?;
        Ok(Self::ok_status(id))
    }

    async fn remove(&mut self, id: u32, filename: String) -> Result<Status, Self::Error> {
        self.refresh_grants().await?;
        let real = self.resolve(&filename)?;
        let md = std::fs::metadata(&real).map_err(|_| StatusCode::NoSuchFile)?;
        let res = if md.is_dir() {
            std::fs::remove_dir(&real)
        } else {
            std::fs::remove_file(&real)
        };
        res.map_err(|_| StatusCode::Failure)?;
        Ok(Self::ok_status(id))
    }

    async fn rmdir(&mut self, id: u32, path: String) -> Result<Status, Self::Error> {
        self.refresh_grants().await?;
        let real = self.resolve(&path)?;
        std::fs::remove_dir(&real).map_err(|_| StatusCode::Failure)?;
        Ok(Self::ok_status(id))
    }

    async fn rename(
        &mut self,
        id: u32,
        oldpath: String,
        newpath: String,
    ) -> Result<Status, Self::Error> {
        self.refresh_grants().await?;
        let from = self.resolve(&oldpath)?;
        let to = self.resolve(&newpath)?;
        // SFTP v3：newpath 已存在时必须报错，不能像 Windows MoveFileEx 那样覆盖。
        if std::fs::symlink_metadata(&to).is_ok() {
            return Err(StatusCode::Failure);
        }
        std::fs::rename(&from, &to).map_err(|_| StatusCode::Failure)?;
        Ok(Self::ok_status(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: Grants = Grants {
        files: true,
        config: true,
    };
    const FILES_ONLY: Grants = Grants {
        files: true,
        config: false,
    };
    const CONFIG_ONLY: Grants = Grants {
        files: false,
        config: true,
    };

    fn tmp_dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "ruthenium-sftproot-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn handler(root: Root, grants: Grants, inbox: PathBuf) -> FsHandler {
        FsHandler::new(Arc::new(root), grants, inbox, None, None)
    }

    fn peer_with(files: bool, config: bool) -> crate::peer::peers::PeerRecord {
        crate::peer::peers::PeerRecord {
            id: "dev-test".to_string(),
            name: "peer".to_string(),
            pubkey: "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAITEST".to_string(),
            last_addr: None,
            paired_at: String::new(),
            last_seen: None,
            grant_files: files,
            grant_control: false,
            grant_config: config,
            grant_screen: false,
            grant_clipboard: false,
            grant_audio: false,
            peer_allows_files: false,
            peer_allows_control: false,
            peer_allows_config: false,
            peer_allows_screen: false,
            peer_allows_clipboard: false,
            peer_allows_audio: false,
        }
    }

    /// 撤销文件/配置授权后，已建立的 SFTP 会话必须在下一次请求就失效
    /// （只在打开子系统时读一次授权不够，旧会话否则可继续读写）。
    #[tokio::test]
    async fn revoke_is_enforced_on_existing_sftp_session() {
        use crate::peer::server::GrantCheck;
        use russh_sftp::server::Handler as _;
        use tokio::sync::RwLock;

        let d = tmp_dir();
        let root = Root::from_share_root(d.to_str().unwrap()).unwrap();
        let peers = Arc::new(RwLock::new(crate::peer::peers::PeersFile {
            version: 1,
            peers: vec![peer_with(true, true)],
        }));
        let checker = GrantCheck::new(peers.clone(), "dev-test".to_string());
        let mut h = FsHandler::new(
            Arc::new(root),
            Grants {
                files: true,
                config: true,
            },
            tmp_dir(),
            None,
            Some(checker),
        );

        // 已授权：列目录成功
        assert!(h.opendir(1, "/".to_string()).await.is_ok());

        // 撤销全部授权：同一会话的下一次请求立刻被拒
        {
            let mut f = peers.write().await;
            f.peers[0].grant_files = false;
            f.peers[0].grant_config = false;
        }
        assert!(matches!(
            h.opendir(2, "/".to_string()).await,
            Err(StatusCode::PermissionDenied)
        ));

        // 设备被删除：同样拒绝
        {
            let mut f = peers.write().await;
            f.peers.clear();
        }
        assert!(matches!(
            h.stat(3, "/".to_string()).await,
            Err(StatusCode::PermissionDenied)
        ));
    }

    /// 句柄级授权：撤销 files 但保留 config 时，已打开的共享根句柄不能继续读；
    /// 撤销 config 但保留 files 时，已打开的收件箱句柄同样不能继续读。
    #[tokio::test]
    async fn handle_capability_survives_other_grant_only() {
        use crate::peer::server::GrantCheck;
        use tokio::sync::RwLock;

        let share = tmp_dir();
        std::fs::write(share.join("a.txt"), b"secret").unwrap();
        let inbox = tmp_dir();
        let peers = Arc::new(RwLock::new(crate::peer::peers::PeersFile {
            version: 1,
            peers: vec![peer_with(true, true)],
        }));
        let checker = GrantCheck::new(peers.clone(), "dev-test".to_string());
        let mut h = FsHandler::new(
            Arc::new(Root::from_share_root(share.to_str().unwrap()).unwrap()),
            Grants {
                files: true,
                config: true,
            },
            inbox.clone(),
            None,
            Some(checker),
        );

        let shared = russh_sftp::server::Handler::open(
            &mut h,
            1,
            "/a.txt".to_string(),
            OpenFlags::READ,
            FileAttributes::default(),
        )
        .await
        .unwrap();
        let inbox_file = russh_sftp::server::Handler::open(
            &mut h,
            2,
            "/peers/inbox/msg.json".to_string(),
            OpenFlags::CREATE | OpenFlags::WRITE,
            FileAttributes::default(),
        )
        .await
        .unwrap();

        {
            let mut f = peers.write().await;
            f.peers[0].grant_files = false;
            f.peers[0].grant_config = true;
        }
        assert!(matches!(
            russh_sftp::server::Handler::read(&mut h, 3, shared.handle.clone(), 0, 16).await,
            Err(StatusCode::PermissionDenied)
        ));

        {
            let mut f = peers.write().await;
            f.peers[0].grant_files = true;
            f.peers[0].grant_config = false;
        }
        assert!(matches!(
            russh_sftp::server::Handler::read(&mut h, 4, inbox_file.handle.clone(), 0, 16).await,
            Err(StatusCode::PermissionDenied)
        ));
        let _ = std::fs::remove_dir_all(&share);
        let _ = std::fs::remove_dir_all(&inbox);
    }

    #[test]
    fn dir_root_maps_and_blocks_escape() {
        // Root::from_share_root 会 canonicalize（Windows 下带 \\?\ 前缀），测试基准也规范化；
        // confine 返回 plain 形式（已剥 \\?\ 前缀），基准同步 plain 化。
        let d = std::fs::canonicalize(tmp_dir()).unwrap();
        let base = plain(&d);
        let root = Root::from_share_root(d.to_str().unwrap()).unwrap();
        let h = handler(root, ALL, tmp_dir());
        assert_eq!(h.resolve("/").unwrap(), base);
        assert_eq!(h.resolve("/a/b").unwrap(), base.join("a").join("b"));
        assert_eq!(h.resolve("/a/../b").unwrap(), base.join("b"));
        // 越过共享根 → 拒绝
        assert!(matches!(
            h.resolve("/.."),
            Err(StatusCode::PermissionDenied)
        ));
        assert!(matches!(
            h.resolve("/a/../../x"),
            Err(StatusCode::PermissionDenied)
        ));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn drives_root_maps_letters() {
        let h = handler(Root::Drives, ALL, tmp_dir());
        assert!(h.is_virtual_root("/"));
        let c = h.resolve("/C/Users").unwrap();
        assert!(
            c.to_string_lossy().starts_with("C:\\Users")
                || c.to_string_lossy().starts_with("c:\\Users")
        );
        assert!(matches!(h.resolve("/.."), Err(StatusCode::NoSuchFile)));
        assert!(matches!(h.resolve("/ab/c"), Err(StatusCode::NoSuchFile)));
        // realpath 形式
        let v = h.virtual_of(Path::new(r"C:\Users\x"));
        assert_eq!(v, "/C/Users/x");
    }

    #[test]
    fn share_root_must_exist() {
        assert!(Root::from_share_root("Z:\\definitely-not-exist-xyz").is_err());
        assert!(matches!(Root::from_share_root(""), Ok(Root::Drives)));
    }

    /// 保留路径：`/peers/inbox/<文件>` 映射到数据目录，且不能借 `..` 逃出。
    #[test]
    fn inbox_namespace_is_reserved_and_bounded() {
        let inbox = tmp_dir();
        let h = handler(Root::Drives, ALL, inbox.clone());
        assert_eq!(
            h.resolve("/peers/inbox/a.json").unwrap(),
            inbox.join("a.json")
        );
        // `..` 归一化后落回收件箱根（不会逃到 peers 之外）
        assert_eq!(
            h.resolve("/peers/inbox/../inbox/b.json").unwrap(),
            inbox.join("b.json")
        );
        // 虚拟路径还原
        assert_eq!(h.virtual_of(&inbox.join("a.json")), "/peers/inbox/a.json");
        let _ = std::fs::remove_dir_all(&inbox);
    }

    /// **配置授权撤销后必须立即拒绝收件箱写入**（即使仍授予文件互传）。
    #[test]
    fn inbox_is_denied_when_config_revoked() {
        let inbox = tmp_dir();
        let h = handler(Root::Drives, FILES_ONLY, inbox.clone());
        assert!(matches!(
            h.resolve("/peers/inbox/x.json"),
            Err(StatusCode::PermissionDenied)
        ));
        // 普通文件路径仍可用（文件互传未撤销）
        assert!(h.resolve("/C/Users").is_ok());
        let _ = std::fs::remove_dir_all(&inbox);
    }

    /// 只授予配置推送：看不到共享根，但收件箱可用。
    #[test]
    fn config_only_hides_share_root() {
        let inbox = tmp_dir();
        let h = handler(Root::Drives, CONFIG_ONLY, inbox.clone());
        assert!(h.resolve("/C/Windows").is_err());
        assert_eq!(
            h.resolve("/peers/inbox/a.json").unwrap(),
            inbox.join("a.json")
        );
        let _ = std::fs::remove_dir_all(&inbox);
    }

    /// ConfigOnly：`/` 下只有合成的 `peers`（看不到共享根）。
    #[tokio::test]
    async fn config_only_root_lists_only_inbox_namespace() {
        let inbox = tmp_dir();
        std::fs::write(inbox.join("x.json"), b"{}").unwrap();
        let mut h = handler(Root::Drives, CONFIG_ONLY, inbox.clone());
        let handle = russh_sftp::server::Handler::opendir(&mut h, 1, "/".into())
            .await
            .unwrap();
        let name = russh_sftp::server::Handler::readdir(&mut h, 2, handle.handle)
            .await
            .unwrap();
        let names: Vec<String> = name.files.iter().map(|f| f.filename.clone()).collect();
        assert_eq!(names, vec!["peers".to_string()]);

        let handle = russh_sftp::server::Handler::opendir(&mut h, 3, "/peers/inbox".into())
            .await
            .unwrap();
        let name = russh_sftp::server::Handler::readdir(&mut h, 4, handle.handle)
            .await
            .unwrap();
        assert!(name.files.iter().any(|f| f.filename == "x.json"));
        let _ = std::fs::remove_dir_all(&inbox);
    }

    /// 收件箱写超限直接拒绝（8 MiB），避免对端把数据目录写满。
    #[tokio::test]
    async fn inbox_write_is_capped() {
        let inbox = tmp_dir();
        let mut h = handler(Root::Drives, CONFIG_ONLY, inbox.clone());
        let flags = OpenFlags::CREATE | OpenFlags::WRITE | OpenFlags::TRUNCATE;
        let handle = russh_sftp::server::Handler::open(
            &mut h,
            1,
            "/peers/inbox/big.json".into(),
            flags,
            FileAttributes::default(),
        )
        .await
        .unwrap();
        let tiny = vec![0u8; 1024];
        russh_sftp::server::Handler::write(&mut h, 2, handle.handle.clone(), 0, tiny.clone())
            .await
            .unwrap();
        let err = russh_sftp::server::Handler::write(
            &mut h,
            3,
            handle.handle.clone(),
            MAX_INBOX_BYTES,
            tiny,
        )
        .await;
        assert!(err.is_err(), "超过上限必须拒绝");
        let _ = std::fs::remove_dir_all(&inbox);
    }

    /* ---------------- symlink/junction 逃逸 ---------------- */

    /// 共享目录内的 junction 指向外部目录时，经它访问必须被拒绝。
    /// junction 无需特权即可创建（`mklink /J`），是 Windows 上最现实的逃逸载体。
    #[cfg(windows)]
    #[test]
    fn junction_escape_is_blocked() {
        let share = tmp_dir();
        let outside = tmp_dir();
        std::fs::write(outside.join("secret.txt"), b"top secret").unwrap();
        // share\jump → outside（junction）
        let out = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(share.join("jump"))
            .arg(&outside)
            .output()
            .expect("创建 junction 失败");
        assert!(
            out.status.success(),
            "mklink /J 应成功: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        let share_canon = std::fs::canonicalize(&share).unwrap();
        let h = handler(Root::Dir(share_canon), ALL, tmp_dir());
        // 词法上 /jump/secret.txt 在共享根内；解析后指向 outside —— 必须拒绝
        let err = h.resolve("/jump/secret.txt");
        assert!(
            matches!(err, Err(StatusCode::PermissionDenied)),
            "经 junction 访问外部文件必须被拒绝，实际: {:?}",
            err.map(|p| p.display().to_string())
        );
        // 共享根内的正常文件不受影响
        std::fs::write(share.join("ok.txt"), b"fine").unwrap();
        assert!(h.resolve("/ok.txt").is_ok(), "共享根内正常文件应可访问");
        // 二级 junction（share\sub\jump → outside）同样拒绝
        std::fs::create_dir(share.join("sub")).unwrap();
        let out2 = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(share.join("sub").join("jump"))
            .arg(&outside)
            .output()
            .expect("创建二级 junction 失败");
        assert!(out2.status.success());
        assert!(matches!(
            h.resolve("/sub/jump/secret.txt"),
            Err(StatusCode::PermissionDenied)
        ));
        let _ = std::fs::remove_dir_all(&share);
        let _ = std::fs::remove_dir_all(&outside);
    }

    /// 收件箱内的链接逃逸：inbox 分支同样必须拒绝（写入即写边界外文件）。
    #[cfg(windows)]
    #[test]
    fn inbox_junction_escape_is_blocked() {
        let inbox = tmp_dir();
        let outside = tmp_dir();
        std::fs::write(outside.join("evil.json"), b"{}").unwrap();
        let out = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(inbox.join("jumpdir"))
            .arg(&outside)
            .output()
            .expect("创建 junction 失败");
        assert!(out.status.success());

        let h = handler(Root::Drives, ALL, inbox.clone());
        assert!(
            matches!(
                h.resolve("/peers/inbox/jumpdir/evil.json"),
                Err(StatusCode::PermissionDenied)
            ),
            "收件箱内经 junction 写外部文件必须被拒绝"
        );
        // 正常收件箱文件不受影响
        assert!(h.resolve("/peers/inbox/normal.json").is_ok());
        let _ = std::fs::remove_dir_all(&inbox);
        let _ = std::fs::remove_dir_all(&outside);
    }

    /// 路径“不存在尾部”场景：待创建文件（open CREATE）不应因不存在而被误拒。
    #[test]
    fn confine_allows_creating_new_files_in_share() {
        let share = tmp_dir();
        std::fs::write(share.join("exist.txt"), b"x").unwrap();
        let h = handler(
            Root::Dir(std::fs::canonicalize(&share).unwrap()),
            ALL,
            tmp_dir(),
        );
        // 已存在文件与同目录新文件都应放行
        assert!(h.resolve("/exist.txt").is_ok());
        assert!(
            h.resolve("/new-file.txt").is_ok(),
            "尚不存在的新文件应允许（confine 只解析存在祖先）"
        );
        let _ = std::fs::remove_dir_all(&share);
    }

    /// 全盘模式：UNC/设备命名空间路径必须拒绝（盘符以外的根不开放）。
    /// 注意 `\\?\C:\`（VerbatimDisk）是 canonicalize 的正常产物，必须放行。
    #[test]
    fn confine_drives_mode_rejects_non_local_roots() {
        // UNC（\\server\share）与 VerbatimUNC（\\?\UNC\server\share）都拒绝
        assert!(matches!(
            confine(Path::new(r"\\server\share\file"), None),
            Err(StatusCode::PermissionDenied)
        ));
        assert!(matches!(
            confine(Path::new(r"\\?\UNC\server\share\file"), None),
            Err(StatusCode::PermissionDenied)
        ));
        // VerbatimDisk（\\?\C:\）是合法本地路径形式
        assert!(confine(Path::new(r"\\?\C:\Windows"), None).is_ok());
        // 普通盘符路径
        assert!(confine(Path::new(r"C:\Windows"), None).is_ok());
    }
}
