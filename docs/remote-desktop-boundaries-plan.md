# 远程桌面已知边界整改方案（Windows 采集 / 输入 / 编解码 / 剪贴板 / 音频）

> 适用代码：Ruthenium `0.1.0-rc.1`（`src-tauri/src/peer`）
> 编写日期：2026-11-27
> 状态：工程方案，部分落地。P0 中的采集健康提示、异步硬件 H.264 与编码器诊断、富剪贴板（文本/HTML/RTF/PNG/DIB，写入前清洗）、音频设备跟随与麦克风混音（被控端逐次确认、横幅可停止）已实现；UIAccess / LocalSystem 服务、厂商硬编 SDK、文件剪贴板、进程 loopback、AEC 等仍未实施。
> 来源标注：`[官方文档]` = 已核对 Microsoft Learn / 厂商官方文档原文；`[源码/实测]` = 已核对开源实现或社区实测记录；`[待验证]` = 需要本项目 PoC 确认；`[二手]` = 引用他人调研、未逐条复核。
> 估时口径：1 名熟悉 Windows 原生 API 的 Rust 工程师，含 PoC、测试与回归，不含签名/合规审批时间。

---

## 0. 结论速览

| # | README 中的"已知边界" | 问题性质 | 能否根治 | 推荐路线 | 优先级 | 估时 |
|---|---|---|---|---|---|---|
| 1 | UAC / 安全桌面内无法捕获或注入 | **平台安全边界**（桌面隔离 + SYSTEM IL） | 装可选系统服务后可根治画面与注入；不装则只能诚实降级 | P0 检测+降级 → P1 可选 UIAccess 助手 → P2 LocalSystem 服务 + SYSTEM helper | P0 立即，P2 中期 | P0 3–5 人日；P2 30–45 人日 |
| 2 | 独占全屏、受保护内容与 DRM 窗口可能黑屏或跳帧 | **平台/内容保护边界**（真 FSE 不进合成、WDA 与 PMP 设计如此） | FSE 不可根治；WDA 不可绕过；DRM 不可绕过 | 采集健康探针 + 后端自动切换 + 受保护窗口检测 + 明确 UX；不做绕过 | P0 检测与 UX，P1 HDR/后端优化 | 5–10 人日 |
| 3 | H.264 硬编因厂商驱动而异 | **实现缺陷为主 + 硬件生态差异** | 可解决大部分：MF 硬编路径本身就是错的；再叠加厂商 SDK 与选择器 | 先修 MF（异步 MFT + LUID + D3D11 纹理），再引入 NVENC/AMF/oneVPL 主路径 + "对端可解"能力协商 | P0 修 MF，P1 厂商 SDK | MF 5–8 人日；SDK 10–20 人日；前端 3–5 人日 |
| 4 | 剪贴板只同步纯文本 | **功能缺口** | 可解决 | 协议 v2（RDPECLIP 骨架）+ P0 文本/HTML/RTF/图片 + P1 文件 + 循环抑制与清洗 | P0 8–10 人日 | P0 8–10 人日；P1 8–10 人日 |
| 5 | 声音只采默认播放设备的 loopback（不含麦克风） | **功能缺口 + API 版本限制** | 可解决（指定应用采集需 Win10 2004+，官方文档口径 20348+，运行时探测降级） | 设备跟随 + 事件驱动 + 进程 loopback + 麦克风混音 + Opus/时间戳修正 | P0 3–5 人日，P1 10–15 人日 | 合计 13–20 人日 |

一句话总览：

- **第 1 条是"要不要装服务"的产品决策**，不是纯技术问题；技术路线已被 Parsec、MeshAgent、RustDesk、Sunshine 分支验证。
- **第 2 条里 FSE/DRM 只能"解释 + 降级"**，任何声称能采集 DRM 的方案都应视为违规或不可信。
- **第 3 条存在明确的代码级根因**：现有实现用同步方式驱动硬件 MFT，而硬件 MFT 按规范必须是异步的（见 §3.2），这解释了"换台机器就不好使"。
- **第 4、5 条是正常的功能补齐**，没有不可逾越的平台障碍。

### 0.1 分诊原则

| 类型 | 判定 | 处理策略 |
|---|---|---|
| 实现缺口 | 平台有公开 API，只是没做或做错了 | 排期实现，P0/P1 |
| 平台安全边界 | 系统设计上禁止（安全桌面、WDA、PMP/HDCP） | 检测 + 明确文案 + 可选增强组件；**不做绕过** |
| 硬件/驱动生态差异 | 能力随 GPU、驱动、SKU 变化 | 运行时能力探测 + 选择器 + 回退链 + 诊断日志 |
| 产品/合规成本 | 需要签名、服务安装、管理员授权 | 作为可选"增强模式"，默认关闭，显式同意 |

### 0.2 平台基线（2026-11）

| 版本 | Build | 状态 |
|---|---|---|
| Windows 11 26H2 | 26300 | 当前 GA（2026-09-29） |
| Windows 11 26H1 | 28000 | 仅新设备，不是既有设备的功能更新 |
| Windows 11 25H2 | 26200 | GA，支持中 |
| Windows 11 24H2 | 26100 | Home/Pro 已结束更新，Enterprise 支持到 2027-10 |
| Windows 11 23H2 | 22631 | 已结束更新 |
| Windows 10 1809–22H2 | — | **2025-10-14 已结束支持** |

依据：[Windows 11 release information](https://learn.microsoft.com/en-us/windows/release-health/windows11-release-information) `[官方文档]`

建议的产品口径（需产品确认）：

- **一级支持**：Windows 11 24H2+（SDK 26100），完整功能与测试覆盖。
- **二级支持**：Windows 11 22H2/23H2、Windows 10 22H2，功能降级（无进程 loopback、无 WGC 新特性、无 Borderless 能力等）。
- **不再承诺**：Windows 10 1809–21H2。README 的 `Windows 10 1809+` 建议在下一版收敛，否则每条新能力都要写一堆降级分支。

### 0.3 现状代码定位

| 边界 | 现状实现 | 位置 |
|---|---|---|
| 安全桌面检测 | `GetForegroundWindow().is_invalid()`，只用于决定"DXGI 失败时是否回退 WGC" | `src-tauri/src/peer/rd.rs:574`（`secure_desktop()`）、`:560`（`try_wgc()`） |
| 采集后端 | 显示器：DXGI Desktop Duplication 优先，特定错误时回退 WGC；窗口：WGC | `rd.rs:1424`（`Source::desktop`）、`:1157`（`WgcSurface`） |
| 输入注入 | `SendInput`，返回值被丢弃；无 UIPI/安全桌面判断 | `src-tauri/src/peer/input.rs:557`（`send()`） |
| H.264 | `MFTEnumEx(HARDWARE\|SYNCMFT)` → 同步 `ProcessInput/ProcessOutput`；CPU BGRA→NV12 | `src-tauri/src/peer/h264.rs:129`（`activate_encoder`）、`:272`（`encode`） |
| 剪贴板 | 仅 `CF_UNICODETEXT`，按需 get/set，前端触发 | `src-tauri/src/peer/clipboard.rs:449` 起 |
| 音频 | 默认渲染设备 WASAPI loopback；5ms 轮询；手写线性重采样；只取 L/R；Opus 64kbps + DTX | `src-tauri/src/peer/audio.rs:481`（`Loopback::new`）、`:383`（轮询）、`:389`（Opus 参数） |
| README 边界声明 | 一行文字 | `README.md:156` |

---

## 1. UAC / 安全桌面：采集与注入

### 1.1 现象

- 触发 UAC 提权弹窗、锁屏、登录界面、Ctrl+Alt+Del 后：查看端黑屏/冻结，键鼠全部失效。
- 当前代码没有"安全桌面模式"这一状态：`secure_desktop()` 只影响是否回退 WGC，且用"前台窗口是否无效"猜测，锁屏与安全桌面、普通无前台窗口的场景会混淆。
- `SendInput` 失败（UIPI/桌面不匹配）时静默忽略，用户侧表现为"点了没反应"，也没有可诊断日志。

### 1.2 根因（层层递进，均有官方依据）

1. **一个 window station 里只有一个"输入桌面"**。交互式 station 固定是 `WinSta0`，内含 `Default`、`ScreenSaver`、`Winlogon` 三个桌面；UAC 弹窗、Ctrl+Alt+Del、登录/锁屏时系统切到 **Winlogon 桌面**。
   [Desktops](https://learn.microsoft.com/en-us/windows/win32/winstation/desktops) `[官方文档]`
2. **Winlogon 桌面的安全描述符只放行极少数账户（含 LocalSystem）**；普通进程（哪怕已提权到管理员）一般不具备这些 SID，因此既不能 `OpenInputDesktop` 进入，也不能 `SwitchDesktop`。
   同上文档 + [SwitchDesktop](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-switchdesktop) `[官方文档]`
3. **采集侧同一堵墙**。`IDXGIOutput1::DuplicateOutput` 的错误码表明确写：
   > `E_ACCESSDENIED` if the application does not have access privilege to the current desktop image. For example, **only an application that runs at LOCAL_SYSTEM can access the secure desktop.**
   [IDXGIOutput1::DuplicateOutput](https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_2/nf-dxgi1_2-idxgioutput1-duplicateoutput) `[官方文档]`
   这直接说明：用户态进程无论怎么改代码都拿不到安全桌面画面；必须是 SYSTEM。
4. **UIAccess 救不了这件事**。UIAccess 的作用是绕过 UIPI、向更高完整性窗口注入输入、置顶，但微软文档明确：
   > None of the previously listed scenarios provide access to UI running under the system IL. This is only possible if the process is launched in the user account control (UAC) desktop under SYSTEM (and system IL). In this case, setting UIAccess has no effect.
   [Security Considerations for Assistive Technologies](https://learn.microsoft.com/en-us/windows/win32/winauto/uiauto-securityoverview) `[官方文档]`
   再说，UIAccess 本身门槛不低：Authenticode 签名 + 安装在 `Program Files`/`System32` + manifest `uiAccess="true"` + 管理员用户启动，且"不应用于非辅助技术"。
   [Application manifests](https://learn.microsoft.com/en-us/windows/win32/sbscs/application-manifests) `[官方文档]`
5. **服务不能直接交互**。Vista 起服务只能跑在 Session 0，不能直接弹 UI/采集用户会话；正确做法是服务通过 `CreateProcessAsUser` 把 helper 放进用户会话（IPC 走命名管道）。
   [Interactive Services](https://learn.microsoft.com/en-us/windows/win32/services/interactive-services) `[官方文档]`
6. **helper 需要 SYSTEM 令牌进入用户会话**。标准序列：服务（LocalSystem）复制自身令牌 → `SetTokenInformation(TokenSessionId)` 指向 console session → `CreateProcessAsUser` 启动 helper；`WTSQueryUserToken` 只适用于"以登录用户身份启动进程"，不是本方案必需（但可用于取当前登录用户做提示/水印）。
   [CreateProcessAsUserW](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-createprocessasuserw)、[WTSQueryUserToken](https://learn.microsoft.com/en-us/windows/win32/api/wtsapi32/nf-wtsapi32-wtsqueryusertoken) `[官方文档]`
7. **锁屏/登录界面的键盘输入另有 2026 年新限制**。KB5080542（2026-02 发布）：安装 2026-01-13 之后更新，Windows 身份验证对话框**只接受受信本地来源输入**——物理键盘、受信任的 UIAccess 辅助技术、以管理员完整性运行的进程；远程键盘/虚拟键盘/自动填凭据会被限制，官方建议改用受支持的认证接口，临时缓解是让执行远程凭据提交的应用以管理员权限运行。
   [KB5080542](https://support.microsoft.com/en-us/servicing/os/windows/docs/2026/01/new-behavior-restricting-certain-applications-to-autofill-credentials-introduced-by-the-windows-janu) `[官方文档]`
   → SYSTEM helper 属于高完整性，按该口径应被接受，但**必须实测**；长期方向是凭证提供者（§1.3 路线 D）。
8. **SAS（Ctrl+Alt+Del）不能靠 SendInput**。`SendSAS` 要求调用者是服务或带 `uiAccess` 的进程，且组策略"Disable or enable software Secure Attention Sequence"必须允许。
   [SendSAS](https://learn.microsoft.com/en-us/windows/win32/api/sas/nf-sas-sendsas) `[官方文档]`

### 1.3 路线对比

| 路线 | 能解决什么 | 前置条件 | 结论 |
|---|---|---|---|
| A. 纯用户态（现状） | 普通桌面；**安全桌面完全不行** | — | 只能做检测与降级 |
| B. UIAccess 助手（单进程，签名 + Program Files） | 向高完整性窗口注入、置顶、`SendSAS`；**安全桌面画面/输入不行** | 代码签名、受信位置、管理员用户启动、策略允许 | 可选 P1，用于"提权后的普通窗口"，不要宣传能看 UAC |
| C. LocalSystem 服务 + SYSTEM helper | 安全桌面/锁屏画面采集 + 键鼠 + `SendSAS` | 管理员安装签名服务；本机确认；SAS 策略 | **唯一工程上被验证的完整方案**（Parsec/MeshAgent/RustDesk/Sunshine 分支） |
| D. 自定义 V2 凭证提供者 | 远程提交凭据完成登录/解锁（不传输画面） | 签名 CP + 安装注册；微软未把"远程技师登录"列为受支持场景 | 备选，P3，仅登录；风险高 |
| E. 官方策略"UAC 不切安全桌面" | 提权弹窗回到普通桌面，现有采集/注入直接可用 | 管理员改策略 | 企业选项，需显著风险提示 |

**业界证据**（用于说明可行性，不代表微软为远程协助背书）：

- Parsec 官方文档：系统服务用于 "pass inputs and capture anything with elevated permissions, such as the task manager, Windows' lock screen, User Access Control (UAC) and any programs running as admin"；进程模型为 `pservice.exe`(SYSTEM) + 一个 SYSTEM `parsecd.exe` + 一个用户 `parsecd.exe`。
  [Parsec App for Windows](https://support.parsec.app/hc/en-us/articles/32381199341716-Parsec-App-for-Windows) `[官方文档]`
- MeshAgent 开源 PR：服务模式下 DXGI 采集在 Windows 11 UAC 安全桌面实测通过，失败帧回退 BitBlt。
  [MeshAgent#496](https://github.com/Ylianst/MeshAgent/pull/496) `[二手/未复核]`
- Sunshine/Apollo 分支：SYSTEM 进程的 DXGI 能跟随输入桌面，per-user WGC helper 不能。
  [Vibepollo#743](https://github.com/Nonary/Vibepollo/pull/743) `[二手/未复核]`
- RustDesk：已合入"Windows 服务/SYSTEM 配置、SAS 策略、portable-service、logon helper"，并有"SYSTEM 子进程采集安全桌面"的原型 PR。
  [#16562](https://github.com/rustdesk/rustdesk/pull/16562)、[#14671](https://github.com/rustdesk/rustdesk/pull/14671)、[#16969](https://github.com/rustdesk/rustdesk/pull/16969) `[源码/PR]`
- 微软 Q&A 答复（非官方文档）：给出了"复制 SYSTEM 令牌 → 设 session → CreateProcessAsUser → helper 内 OpenInputDesktop/SetThreadDesktop → SendInput/SendSAS"的完整序列，同时明确微软**没有**专门给第三方远控的锁屏注入 API，建议自定义凭证提供者。
  [MS Q&A](https://learn.microsoft.com/en-us/answers/questions/6079966/supported-windows-11-integration-for-remote-assist) `[二手/非官方]`

### 1.4 推荐方案

#### P0：检测与诚实降级（3–5 人日，必须做）

**目标**：不再把"安全桌面"表现为黑屏卡死/点击无反应，而是明确状态 + 恢复。

1. 用桌面名判断当前输入桌面，而不是猜前台窗口：
   - `OpenInputDesktop(0, FALSE, DESKTOP_READOBJECTS | DESKTOP_SWITCHDESKTOP)` → `GetUserObjectInformation(UOI_NAME)`；
   - 名字为 `Default` 才是普通桌面；`Winlogon` 等即安全桌面（不要写死只判断 `Winlogon`，锁屏/登录/凭据 UI 可能不同）。
   - 用 200–500ms 轮询 + `DXGI_ERROR_ACCESS_LOST` 作为快速触发信号（`WTSRegisterSessionNotification` 只覆盖会话变化，不覆盖 UAC 桌面切换）。
2. 采集层新增 `CaptureState::SecureDesktop`：
   - 停止发帧（或发"冻结末帧 + 状态提示"），停止重建循环的空转；
   - 恢复到 `Default` 后自动重建 duplication/WGC，并补一个关键帧。
3. 注入层：`input.rs` 在安全桌面/锁屏状态下拒绝注入，向查看端返回结构化错误（现有授权复核逻辑保留）；`SendInput` 返回值与 `GetLastError` 必须检查并写日志。
4. UI：查看端顶部显示"对端处于 UAC/登录界面，画面与操作暂不可用"，被控端横幅同步提示；审计记录进入/离开安全桌面的时间。
5. 顺手修掉 `rd.rs:574` 的 `GetForegroundWindow().is_invalid()` 启发式：它会把"普通桌面但无前台窗口"误判为安全桌面，导致 WGC 回退被无谓禁用。

**验收**：VM 中分别触发 UAC、锁屏、普通桌面切窗口；断言状态机切换、日志、恢复自动出帧；`cargo test` 覆盖桌面名解析与状态迁移。

#### P1：可选 UIAccess 助手（5–8 人日，可选）

**目标**：解决"高完整性窗口"（任务管理器、以管理员运行的程序）的键鼠/读屏，以及 `SendSAS` 能力。**不承诺安全桌面画面。**

- 新增产物 `ruthenium-uia.exe`：manifest `uiAccess="true"`，签名 + 安装到 `Program Files\Ruthenium`，仅由用户会话启动，不联网；通过本地管道与主进程通信。
- 用途：主进程检测到目标窗口完整性级别高于自身时，把注入请求转发给 UIA 助手；`SendSAS` 也由它承担（仍需软件 SAS 策略允许）。
- 约束：微软明确"UIAccess 只应用于辅助技术"；产品文案不要把它包装成"远程控制增强"，否则 Smart App Control/EDR 与应用商店审核都可能出问题。
- 注意：UIA 助手的输入对 `Default` 桌面有效，对安全桌面无效（system IL 隔离，见 §1.2 第 4 条）。

#### P2：LocalSystem 服务 + SYSTEM helper（完整方案，30–45 人日）

**目标**：安全桌面（UAC/锁屏/登录）的画面采集 + 键鼠注入 + SAS，作为**可选"增强模式"**（默认关闭，需管理员安装并逐会话确认）。

**进程与模块**

```
Ruthenium 主进程（用户态，Tauri）
  │  ① 本机确认/授权（沿用现有 ScreenHub 每会话确认）
  │  ② 通过本地管道请求"增强会话"
  ▼
rt-secure-svc.exe（Windows 服务，LocalSystem，Session 0，按需启动）
  │  ③ 校验调用方：PID + 镜像路径 + 签名 + 会话 ID + 用户 SID
  │  ④ 复制自身 SYSTEM 令牌 → SetTokenInformation(TokenSessionId=console) → CreateProcessAsUser
  ▼
rt-secure-helper.exe（SYSTEM，运行在用户 console session）
  │  ⑤ 采集线程：DXGI DuplicateOutput（失败回退 GDI BitBlt），桌面切换时重建
  │  ⑥ 注入线程：OpenInputDesktop + SetThreadDesktop，SendInput；SAS 由服务侧 SendSAS
  │  ⑦ 帧 → 共享内存/D3D11 共享纹理；输入 ← 命名管道
  ▼
Ruthenium 主进程读取帧、编码、走现有 SSH 传输（不改网络协议，只加一路采集源）
```

**关键调用序列**（Rust `windows` crate 命名空间）

1. 服务安装：SCM API（`CreateServiceW`/`ChangeServiceConfigW`），**不要用 `sc.exe`/`cmd.exe`**（RustDesk 遇到过 `sc create` 链被 Bitdefender 拦），启动类型 `demand`。
2. 服务启动：`WTSGetActiveConsoleSessionId` / `WTSEnumerateSessionsW` 找当前活动会话；`OpenProcessToken(GetCurrentProcess())` → `DuplicateTokenEx(TokenPrimary)` → `SetTokenInformation(TokenSessionId)`。
3. 启动 helper：`CreateProcessAsUserW`，`STARTUPINFO.lpDesktop = L"winsta0\\default"`（先落在普通桌面，再动态挂输入桌面），`CREATE_UNICODE_ENVIRONMENT`，环境用 `CreateEnvironmentBlock`。
4. helper 采集：每 200ms 或 `ACCESS_LOST` 时 `OpenInputDesktop` + `SetThreadDesktop`；DXGI duplication 在 SYSTEM 下可访问安全桌面（§1.2 第 3 条），失败回退 `GetDC(NULL)` + `BitBlt`（需要 window station 的 `WINSTA_READSCREEN`，SYSTEM 具备）。
5. helper 注入：线程已挂在输入桌面后 `SendInput`；绝对坐标沿用现有 `MOUSEEVENTF_ABSOLUTE|VIRTUALDESK` 算法；**注入线程不能有窗口/钩子**（`SetThreadDesktop` 的要求）。
6. SAS：由服务进程调用 `SendSAS(FALSE)`（服务身份满足"running as a service"）；同时检查/引导策略。
7. IPC：命名管道 `\\.\pipe\ruthenium-secure-<nonce>`，`PIPE_REJECT_REMOTE_CLIENTS` + 显式 DACL（SYSTEM 全权、指定用户读写）；服务用 `GetNamedPipeClientProcessId` + `QueryFullProcessImageNameW` + `WinVerifyTrust` 校验客户端。
8. 帧回传：MVP 用 `Global\` 共享内存双缓冲 + 事件（1080p BGRA 约 8 MB/帧，30fps 需 ~250 MB/s 内存带宽，可接受）；进阶用 `ID3D11Texture2D` + `IDXGIResource1::CreateSharedHandle`（NT handle + 受限安全描述符）把采集纹理直接交给主进程的编码器，做到 GPU→GPU 零拷贝。
9. 生命周期：一次增强会话 = 一个 helper；会话结束/授权撤销/主进程退出/服务停止时立即终止；helper 不发网络包、不接受任意命令。

**安全设计（这是本方案的核心边界，不是可选项）**

- 默认关闭，安装时单独勾选；安装器用 SCM API，不落地 `sc.exe` 命令行。
- 每次远程会话仍需本机确认（沿用 `ScreenHub` 流程），"安全桌面支持"作为独立授权项，不能随 `grant_screen` 自动获得。
- 管道 DACL 按用户 SID/logon SID 限制，拒绝远程客户端；校验 PID→镜像路径→签名→会话；nonce 一次性。
- helper 消息 schema 固定（帧就绪、输入事件、SAS、心跳），**无文件路径、无 shell、无任意进程操作**。
- 审计：helper 启动/退出、桌面切换、SAS、注入启停写入 Event Log + 本地 JSONL（不含内容）。
- 安全桌面画面烧录"远程会话中"水印（普通窗口的横幅在安全桌面不可见，这一点 RustDesk 也承认覆盖层无法保证盖住安全桌面，[#17145](https://github.com/rustdesk/rustdesk/pull/17145)）。
- 全局急停热键 + 托盘"立即断开"；企业提供策略开关（禁用服务/禁用注入）。

**明确不做**：不隐藏安装、不静默无人值守控制、不注入受保护进程、不读写 LSASS/SAM/凭据、不 hook Winlogon、不装内核驱动（XDDM 镜像驱动自 Win8 起已不可用，官方替代就是 Desktop Duplication，[Desktop Duplication API](https://learn.microsoft.com/en-us/windows/win32/direct3ddxgi/desktop-dup-api)）。

**风险**

- 被 EDR/杀软当作远控木马：必须代码签名（EV 或 Azure Artifact Signing）、提交 Defender/SmartScreen 误报申诉、开源可审计、文档写清楚。
- 2026 年微软正在收紧"远程输入注入"（KB5080542）；SYSTEM 完整性当前应被接受，但要在 24H2/25H2/26H2 回归；长期看凭证提供者是官方导向。
- Administrator protection 会改变提权模型（隐藏管理员账户、即时令牌），但对"服务 + SYSTEM helper"路线影响较小；官方开发者博客反而建议"需要提权能力时采用系统账户/服务化设计"（[Administrator protection](https://learn.microsoft.com/en-us/windows/security/application-security/application-control/administrator-protection/) `[官方文档]`；博客为二手引用）。
- 维护成本高：服务生命周期、安装/卸载/升级、跨版本回归，需要独立 CI 夹具。

#### P3：备选与降级

- **企业选项**：引导管理员配置"UAC 不切安全桌面"策略（`User Account Control: Allow UIAccess applications to prompt for elevation without using the secure desktop` / 关闭 secure desktop 提示），之后提权弹窗出现在普通桌面，现有采集与注入直接可用；必须显著提示"这会降低本机 UAC 防护"。
  [UAC settings](https://learn.microsoft.com/en-us/windows/security/application-security/application-control/user-account-control/settings-and-configuration) `[官方文档]`
- **凭证提供者（V2）**：只解决"远程完成登录/解锁"，不传输安全桌面画面；可作为 P3 长期项（工作量大，微软未把远程技师场景列为受支持）。
- **UIA 读 UAC 内容**：不可行（UIA 客户端在 `Default` 桌面无法跨到 system IL），不要尝试。

### 1.5 工作量与验收

| 项 | 人日 | 验收 |
|---|---|---|
| P0 检测/状态机/UX/日志 | 3–5 | VM 自动触发 UAC/锁屏，状态正确、自动恢复、无黑屏假死 |
| P1 UIAccess 助手 | 5–8 | 能控制任务管理器等提权窗口；不能看安全桌面（负向断言） |
| P2 服务 + helper | 30–45 | 安全桌面 3 帧非黑、注入 Tab/方向键画面变化、SAS 生效、断开即杀、管道越权测试全部拒绝 |
| 安装器/签名/审计/文档 | 含在 P2 | 安装/卸载/升级/回滚；Event Log 与审计导出 |

测试夹具：带 `requireAdministrator` manifest 的触发程序 + `ShellExecuteEx("runas")`；锁屏用 `rundll32 user32.dll,LockWorkStation`；CI 需要 self-hosted Win11 VM（托管 runner 无交互桌面），用快照复位。

---

## 2. 独占全屏、受保护内容与 DRM

### 2.1 现象拆分（三类不同问题，不能混为一谈）

| 现象 | 典型场景 | 根因 | 能否解决 |
|---|---|---|---|
| 黑屏/冻结/AccessLost | 真独占全屏游戏（FSE）、部分视频全屏 | FSE 绕过 DWM 合成，复制接口拿不到帧；Win11 24H2+ 更严格 | **不能从采集侧修复**，只能检测与提示 |
| 黑块/窗口区域透明 | 应用设置了 `WDA_EXCLUDEFROMCAPTURE`/`WDA_MONITOR` | 系统按窗口内容保护策略从公共采集表面排除该窗口 | **不能绕过**；可检测并提示 |
| 黑屏但界面正常 | Netflix/Prime/Disney+ 等 DRM 内容 | 受保护媒体路径（PMP）+ OPM/HDCP 链路保护，解码在保护进程内 | **不能绕过**；明确不支持 |
| 发灰/偏色 | HDR/宽色域显示器 | 旧复制接口固定 BGRA8；需要 `DuplicateOutput1`/WGC 指定 FP16 | 可解决 |

### 2.2 根因与依据

1. **DDA 复制的是 DWM 合成后的桌面**。`DuplicateOutput` 在"切换到全屏应用、DWM on/off、模式切换"时会 `ACCESS_LOST`，必须释放并重建；非 DWM 桌面模式直接 `DXGI_ERROR_UNSUPPORTED`。同一输出最多 4 个进程同时复制。
   [IDXGIOutput1::DuplicateOutput](https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_2/nf-dxgi1_2-idxgioutput1-duplicateoutput) `[官方文档]`
2. **Win11 24H2+ 上真 FSE 的帧不进入合成桌面**。OBS 2026-10 的实测 issue 记录：FSE 下 DXGI 与 WGC **两种采集都冻结**；`AcquireNextFrame` 返回 `ACCESS_LOST`，重建后 `LastPresentTime == 0`；关闭 FSO、禁用 MPO、换旧驱动均无效，结论是"游戏的帧没有进入被复制的桌面"。
   [OBS #14021](https://github.com/obsproject/obs-studio/issues/14021) `[源码/实测]`
   → 因此不要把"支持独占全屏"写进宣传；正确做法是提示用户切换"无边框窗口"。
3. **WGC 也不是 FSE 的银弹**：它走 DWM 合成，在 24H2+ 真 FSE 下同样冻结（上条证据）；它的价值在窗口共享、覆盖层、非 FSE 场景。
4. **受保护窗口是设计如此**：`WDA_EXCLUDEFROMCAPTURE`（0x11，Win10 2004+）让窗口"只显示在显示器上，其它地方完全不出现"；`WDA_MONITOR`（0x1）显示但无内容。官方同时声明它**不是安全/DRM 机制**，不保证防拍照；但只要 DWM 在合成，公共采集 API 就拿不到。
   [SetWindowDisplayAffinity](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-setwindowdisplayaffinity) `[官方文档]`
   可用 `GetWindowDisplayAffinity` 从任意进程查询（要求窗口 layered 且 DWM 在合成），**不存在** `IsWindowDisplayAffinity`。
   [GetWindowDisplayAffinity](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-getwindowdisplayaffinity) `[官方文档]`
5. **DRM 不是"窗口保护"，而是整条链路保护**：PMP 在独立受保护进程中解密/解码，应用只能与代理 Media Session 交换控制消息；OTA 通过 OPM 向显卡驱动下发 HDCP/DPCP 链路保护要求，不满足就阻断。
   [Protected Media Path](https://learn.microsoft.com/en-us/windows/win32/medfound/protected-media-path)、[GPU-Based Content Protection](https://learn.microsoft.com/en-us/windows/win32/medfound/gpu-based-content-protection) `[官方文档]`
   采集 API 从架构上就取不到受保护路径的画面，实际表现多为黑块或壁纸。
6. **HDR**：WGC 文档明确 HD Color 开启时采集内容像素格式不一定还是 `B8G8R8A8_UNORM`，建议用 `R16G16B16A16_FLOAT` 避免发灰；`DuplicateOutput1` 可以指定格式列表，直接拿到全色域（如 `R10G10B10A2`）而不做转换。
   [Screen capture](https://learn.microsoft.com/en-us/windows/apps/develop/media-authoring-processing/screen-capture)、[IDXGIOutput5::DuplicateOutput1](https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_5/nf-dxgi1_5-idxgioutput5-duplicateoutput1) `[官方文档]`

### 2.3 方案：采集层重构

**目标**：把"黑屏/冻结"变成"可解释、可恢复、可诊断"的状态，而不是让用户面对无止境的静止画面。

#### 2.3.1 采集健康探针（P0，3–5 人日）

新增 `capture/health.rs`（或 `rd.rs` 内模块）：

- 输入：每帧的抓取结果、`ACCESS_LOST` 计数、帧间隔、帧哈希（网格采样）、全黑/单色像素占比、`GetForegroundWindow` 进程名、`SHQueryUserNotificationState`。
- 输出状态：`Live` / `Static` / `AccessLostFlapping` / `SuspectedExclusiveFullscreen` / `CaptureBlockedProtected` / `SecureDesktop` / `EncoderStalled`。
- 策略：
  - 连续 N 帧全黑/同哈希 + `ACCESS_LOST` 高频 → 标记 `SuspectedExclusiveFullscreen`，UI 提示"检测到独占全屏，请切换为无边框窗口"，同时冻结末帧继续音频；
  - `ACCESS_LOST` 重建采用指数退避（1s→2s→4s，上限 10s），避免每秒 15 次空转重建（OBS 实测每秒重建 15 次仍不恢复，只烧 CPU）；
  - 恢复 `Live` 时强制补关键帧，前端 `VideoDecoder` 重新配置。
- 指标：`access_lost/min`、`frames_dropped`、`rebuild_ms`、`black_ratio`、`static_ms`，写入现有 `applog` 与诊断导出。

#### 2.3.2 后端策略调整（P0，2–3 人日）

| 场景 | 现状 | 建议 |
|---|---|---|
| 显示器共享 | DXGI 优先，WGC 仅在特定错误时回退 | 保持 DXGI 性能优势，但把"健康探针判定异常"也纳入回退条件；回退后再失败则进入 `SuspectedExclusiveFullscreen` 状态而不是无限重建 |
| 窗口共享 | WGC | 保持；Win11 24H2+ 打开 `IncludeSecondaryWindows`（抓对话框/菜单，26100+）；用 `IDisplayGraphicsCaptureSession.SetWindowExclusionList` 把自家悬浮通知窗排除（26100+，需实测） |
| 光标 | DXGI 自带光标形状；WGC 可开关 | 统一自绘，避免两后端行为差异 |
| HDR | 固定 BGRA8 | `IDXGIOutput6::GetDesc1` 判色域；DDA 用 `DuplicateOutput1` 的格式列表（`B8G8R8A8_UNORM` + `R16G16B16A16_FLOAT`）；WGC 帧池用 FP16；编码前 tone-map 到 SDR 或保留 HDR（前端 `VideoDecoder` 需支持 10bit，工作量大，放 P2） |
| 边框 | WGC 尝试 `SetIsBorderRequired(false)` | 该属性要求"包标识 + `graphicsCaptureWithoutBorder` 能力 + `GraphicsCaptureAccess.RequestAccessAsync(Borderless)` 用户同意"，未打包的 Tauri 桌面应用大概率关不掉黄框（[IsBorderRequired](https://learn.microsoft.com/en-us/uwp/api/windows.graphics.capture.graphicscapturesession.isborderrequired)）；产品文档要说明"窗口共享时可能显示系统采集边框"，不要当 bug |

#### 2.3.3 受保护窗口检测与 UX（P0，2–3 人日）

- 窗口共享列表阶段：对候选窗口 `GetWindowDisplayAffinity`，命中 `0x11`/`0x1` 直接标注"该窗口启用了防截屏"；`DwmGetWindowAttribute(DWMWA_CLOAKED)` 过滤不可见窗口。
- 会话中检测到目标窗口受保护：返回结构化 `CaptureBlocked { reason }`，前端显示明确文案，**不要持续发送黑帧**（黑帧会让用户误以为网络卡）。
- 不提供"强制采集/绕过"开关。

#### 2.3.4 FSE 与 DRM：明确的产品边界（P0，1 人日）

- FSE：检测 + 提示"请切换为无边框/窗口模式"；文档写明"独占全屏不保证可见"。
- DRM：明确"受版权保护内容不支持远程播放"；可选降级："仅音频"或"静态帧 + 音频"（不触碰受保护视频面）。
- 不提供任何 HDCP 剥离、DLL 注入、驱动 hook 方案；这些既不可靠也有法律风险（HDCP 实现需与 DCP LLC 授权协议，规避访问控制可能触及 DMCA §1201，[17 U.S.C. §1201](https://www.law.cornell.edu/uscode/text/17/1201)、[HDCP 规范](https://www.digital-cp.com/hdcp-specifications)）。

#### 2.3.5 虚拟显示器（P3，可选）

IddCx 间接显示驱动可解决：无头/无显示器、固定分辨率、隐私模式（关闭物理显示器）、多屏布局。**不能**解决 DRM（需要虚拟显示实现 OPM/HDCP 受保护输出，通常没有）和 WDA 窗口。
参考：[IddCx overview](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/indirect-display-driver-model-overview) `[官方文档]`
代价：驱动工程 + EV 签名 + 管理员安装 + 蓝屏/升级兼容测试，按 1–2 人月评估，放 P3。

### 2.4 工作量与验收

| 项 | 人日 | 验收 |
|---|---|---|
| 健康探针 + 状态机 + 退避重建 | 3–5 | 构造黑屏/静止/AccessLost 场景，状态与 UI 正确；恢复后自动出帧 |
| 后端策略 + WGC 24H2 特性 | 2–3 | 窗口共享能抓到对话框；黄框行为符合文案 |
| 受保护窗口检测 + UX | 2–3 | 自建 `WDA_EXCLUDEFROMCAPTURE` 测试窗，列表标注、会话提示、无黑帧 |
| FSE/DRM 文案与降级 | 1 | Minecraft/osu! FSE 用例：提示正确；Netflix：黑块 + 正确文案 |
| HDR/P2（可选） | 5–10 | HDR 显示器不发灰；10bit 链路前端可解 |

---

## 3. H.264 硬编：从"枚举碰运气"到"能力矩阵 + 回退链"

### 3.1 现状

`h264.rs` 的编码器选择逻辑（`activate_encoder`，`h264.rs:129`）：

1. `MFTEnumEx(MFT_CATEGORY_VIDEO_ENCODER, HARDWARE | SYNCMFT | SORTANDFILTER, NV12→H264)`；
2. 激活后设置 `CODECAPI_AVLowLatencyMode`、`AVEncCommonRealTime`、CBR、GOP=2s、无 B 帧、1 参考帧；
3. 用**同步** `IMFTransform::ProcessInput/ProcessOutput` 驱动；输入是 CPU 转好的 NV12；
4. 失败回退软件 MFT，再失败回退 JPEG；前端只支持 `avc1.*`。

### 3.2 根因（代码级，按可能性排序）

1. **硬件 MFT 本质是异步的，当前代码按同步方式驱动**（最可能的根因）。
   - `MFTEnumEx` 文档：`MFT_ENUM_FLAG_HARDWARE` 的 MFT **always process data asynchronously**；`Hardware MFTs` 文档：硬件 MFT 必须使用异步模型，设置 `MF_TRANSFORM_ASYNC=TRUE`；
   - `Asynchronous MFTs` 文档：未解锁前所有 `IMFTransform` 方法返回 `MF_E_TRANSFORM_ASYNC_LOCKED`，客户端需设 `MF_TRANSFORM_ASYNC_UNLOCK=TRUE`，并通过 `IMFMediaEventGenerator` 等 `METransformNeedInput`/`METransformHaveOutput` 再调用 `ProcessInput`/`ProcessOutput`。
   [MFTEnumEx](https://learn.microsoft.com/en-us/windows/win32/api/mfapi/nf-mfapi-mftenumex)、[Hardware MFTs](https://learn.microsoft.com/en-us/windows/win32/medfound/hardware-mfts)、[Asynchronous MFTs](https://learn.microsoft.com/en-us/windows/win32/medfound/asynchronous-mfts) `[官方文档]`
   - 当前代码既没解锁、也没有事件循环，硬件 MFT 要么激活/配置失败，要么 `ProcessInput` 返回错误被吞，最终表现为"有的机器能用、有的机器直接回退"。
2. **没有按 GPU 适配器 LUID 枚举**。核显 + 独显机器上，`MFTEnumEx` 可能拿到另一块 GPU 的 MFT，激活后与当前 D3D 设备不匹配。正确做法是 `MFTEnum2` + `MFT_ENUM_ADAPTER_LUID`（且必须同时带 `MFT_ENUM_FLAG_HARDWARE`，否则 `E_INVALIDARG`）。
   [MFTEnum2](https://learn.microsoft.com/en-us/windows/win32/api/mfapi/nf-mfapi-mftenum2) `[官方文档]`
3. **没有 D3D11 纹理输入**。硬件 MFT 常要求 `MF_SA_D3D11_AWARE` + `MFT_MESSAGE_SET_D3D_MANAGER` 挂 `IMFDXGIDeviceManager`，只喂系统内存 NV12 时协商失败或触发隐式拷贝；BGRA→NV12 的 CPU 转换在 1080p60 下本身也是瓶颈。
   [MF_SA_D3D11_AWARE](https://learn.microsoft.com/en-us/windows/win32/medfound/mf-sa-d3d11-aware) `[官方文档]`
4. **没有区分"真硬件"**。`IMFActivate` 上 `MFT_ENUM_HARDWARE_URL_Attribute` 缺失的是软件 MFT；当前只看枚举 flag，容易把软件 MFT 当硬件打日志。
   [MFT_ENUM_HARDWARE_URL_Attribute](https://learn.microsoft.com/en-us/windows/win32/medfound/mft-enum-hardware-url-attribute) `[官方文档]`
5. **环境因素**：N 版 Windows 未装媒体功能包；OEM 把 `HKLM\...\HardwareMFT\EnableEncoders` 置 0；混合显卡/虚拟机 GPU-P/远程桌面会话下 MFT 注册不全。
   [MFTEnumEx 备注](https://learn.microsoft.com/en-us/windows/win32/api/mfapi/nf-mfapi-mftenumex) `[官方文档]`
6. **前端只有 H.264 一种解码器**：即使以后编出 HEVC/AV1，前端也接不住；另外 `avc1` 走 Annex B 可行，但换 HEVC 需要正确的 `hvc1/hvcC` 配置（见 §3.5）。

### 3.3 方案 A：先修 MF 硬编路径（P0，5–8 人日）

不改架构，先让现有 MF 路径真正可用，同时产出可诊断的能力信息。

1. **枚举**：
   - 硬件：`MFTEnum2(MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER, pAttributes = {MFT_ENUM_ADAPTER_LUID: 当前采集适配器 LUID})`；
   - 软件：`MFTEnumEx(..., MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER)`；
   - 对每个 `IMFActivate` 记录 `MFT_ENUM_HARDWARE_URL_Attribute` 是否存在、`FriendlyName`、LUID、是否 `MF_TRANSFORM_ASYNC`。
2. **异步处理模型**（只对 `MF_TRANSFORM_ASYNC=TRUE` 的 MFT）：
   - `GetAttributes().SetUINT32(MF_TRANSFORM_ASYNC_UNLOCK, 1)`；
   - 在专用线程上 `BeginGetEvent/EndGetEvent` 等 `METransformNeedInput` → `ProcessInput`；等 `METransformHaveOutput` → `ProcessOutput`；
   - 处理 `METransformDrainComplete`、`MFT_MESSAGE_NOTIFY_START_OF_STREAM`；
   - 编码线程与采集线程解耦，队列长度 1（满则丢最旧），与现有 `run_stream` 的帧队列策略一致；
   - 加 watchdog：初始化/首帧/连续 N 帧超时 → 换下一个候选。
3. **D3D11 输入**：`MFCreateDXGIDeviceManager` + `ResetDevice` → `MFT_MESSAGE_SET_D3D_MANAGER`；采集纹理（BGRA）先经 shader 或 `ID3D11VideoProcessor` 转 NV12，用 `MFCreateDXGISurfaceBuffer` 包成 sample；如果 MFT 支持 BGRA 输入则直接注册纹理（NVENC/AMF 可行，MF MFT 少见）。
4. **低延迟参数**（保持现有思路，补齐语义）：
   - `CODECAPI_AVLowLatencyMode=TRUE`（要求不引入重排序，1 入 1 出）；
   - `CODECAPI_AVEncMPVDefaultBPictureCount=0`（必须在 `SetOutputType` 前设置）；
   - `CODECAPI_AVEncCommonRateControlMode=CBR`、`CODECAPI_AVEncCommonMeanBitRate`；
   - `CODECAPI_AVEncVideoForceKeyFrame` 只对下一帧生效且自动清零，每次 IDR 都要重设；
   - 可选 `CODECAPI_AVScenarioInfo=DisplayRemoting`（若驱动支持）。
   [CODECAPI_AVLowLatencyMode](https://learn.microsoft.com/en-us/windows/win32/medfound/codecapi-avlowlatencymode) `[官方文档]`
5. **诊断**：启动时输出一张"编码器能力表"（候选 MFT、是否硬件、异步、支持分辨率/格式、失败原因），写 `applog` 并可在设置页导出；这能直接回答"为什么这台机器没有硬编"。

### 3.4 方案 B：厂商 SDK 主路径（P1，10–20 人日）

MF 路径修好后，覆盖率和性能仍受驱动注册影响；长期建议改为"厂商 SDK 优先、MF 兜底"。

| 编码器 | 最低条件 | 优点 | 缺点 | 建议 |
|---|---|---|---|---|
| NVENC | Maxwell+，`nvEncodeAPI64.dll` 动态加载；SDK 13.x 要求较新驱动，**编译用较低 SDK 以兼容存量驱动** | BGRA/D3D11 纹理直喂、ULL 模式、AV1（Ada+）、GeForce 12 并发会话 | NVIDIA 独显；SDK 版本/驱动门槛 | P1 首选 |
| AMD AMF | `amf.dll`；AMF 1.5.x | D3D11/D3D12、AV1、屏幕内容 palette/整数 MV 默认开 | 驱动差异；Rust 无成熟绑定 | P1 |
| Intel oneVPL | `libvpl` 2.x；Arc/Xe | H.264/HEVC/AV1、`display remoting` 场景 | Windows 上需 D3D11 device；绑定也不成熟 | P1（有 Intel 机器时） |
| FFmpeg 8.x | 静态/动态库 | 一次封装三家 + 软件编码；`h264_mf` 也顺带可用 | LGPL 合规、构建体积、额外 IPC（sidecar） | P2/实验 |

Rust 生态现状（供决策）：`ffmpeg-next`/`ffmpeg-sidecar` 可用；`nvenc` crate 很薄（低下载量）、`moq-nvenc` 运行时 dlopen 更适合分发；AMD/Intel 没有成熟 crate（`vpl`/`amf` 不存在），实际只有"FFmpeg 静态链"或"`windows`/`libloading` 自己 FFI"两条路。
`[源码/API]`（来源：crates.io / FFmpeg 源码调研）

**低延迟参数模板**（关键：无 B 帧、CBR、CBR 填充、短 GOP、强制 IDR）：

| SDK | 关键参数 |
|---|---|
| NVENC | `tuningInfo=ULTRA_LOW_LATENCY`、preset P1/P3、CBR、`frameIntervalP=1`、`gopLength=2*fps`、`lowDelayKeyFrameScale=1`、`enableFillerDataInsertion=1`、可选 intra-refresh |
| AMF | `usage=ULTRA_LOW_LATENCY`、`LowLatencyInternal=true`、`BPicturesPattern=0`、`IDRPeriod=2*fps`、`QueryTimeout=0` |
| oneVPL | `LowDelayBRC=ON`、`ScenarioInfo=displayremoting`、`GopRefDist=1`、CBR |
| MF（兜底） | 见 §3.3 第 4 条 |

### 3.5 编码器选择器与前端适配

**选择器（P1，3–5 人日）**

```
候选 = 本机可编码器（探针缓存）
     ∩ 对端可解码器（查看端 VideoDecoder.isConfigSupported 上报）
优先级 = AV1 硬 > HEVC 硬 > H.264 硬 > H.264 软 > JPEG
```

- 对端能力必须参与决策：本机能编 HEVC/AV1 不代表对端能解；Windows 上 HEVC 解码需要硬件解码 + HEVC Video Extensions，AV1 覆盖较好但不保证。
- 结果写入握手消息（`stream` 控制消息已有 `codec` 字段，扩展 `profile/level` 即可）。
- 分辨率/帧率变化、GPU 掉卡（`DXGI_ERROR_DEVICE_REMOVED`）、驱动更新后重新探测；升级有冷却，避免抖动。

**前端（3–5 人日）**

- 保持 H.264 默认（兼容性最好）。
- AV1：`av01.*` + 序列头（OBU）解析；HEVC：`hvc1.*`/`hev1.*` + `hvcC`（需要从 Annex B 的 VPS/SPS/PPS 构造 hvcC，或让编码侧输出带 description 的格式）；`VideoDecoder.configure({ codec, description, optimizeForLatency: true, hardwareAcceleration: "prefer-hardware" })`。
- 解码失败/黑帧：`isConfigSupported` 预检 + 首帧超时 → 通过控制消息要求服务端降回 H.264，并 `close()`/`configure()` 重建（现有 `RemoteView` 已有 codec 降级入口，扩展即可）。
- 注意 Annex B 与 AVCC 的差异：现有 H.264 管线依赖"无 description 时按 Annex B 解"的浏览器行为；新增 HEVC/AV1 时要把配置格式显式化，避免靠隐式行为。

### 3.6 工作量与验收

| 项 | 人日 | 验收 |
|---|---|---|
| MF 异步 MFT + LUID + D3D11 输入 + 诊断表 | 5–8 | 同一台混合显卡机器：能力表列出 HW MFT；有硬编时日志确认异步路径；FFmpeg `mfenc` 同款行为对齐 |
| 厂商 SDK 抽象层 + 至少 NVENC | 10–20 | NVIDIA 机器硬编成功率 ≥95%，编码线程 CPU < 10%（1080p30） |
| 选择器 + 对端能力协商 | 3–5 | 人为禁用对端 HEVC/AV1，服务端自动回 H.264 |
| 前端多 codec 支持 | 3–5 | H.264 回归全绿；AV1/HEVC 在支持的机器上可解；不支持时 2s 内降级 |
| 测试矩阵 | 2–3 | NV/AMD/Intel × H.264/HEVC/AV1 × 1080p/1440p/4K/竖屏，记录首次出帧、延迟、CPU、码率 |

---

## 4. 剪贴板：从纯文本到多格式 + 文件

### 4.1 现状

- 协议：`ruthenium-clipboard` 子系统，前端按需 get/set，消息为 JSON + 文本，16 MiB 上限，仅 `CF_UNICODETEXT`（`clipboard.rs:449` 起）。
- 安全模型：`grant_clipboard` + 屏幕会话令牌，服务端每 5s 复核授权（`clipboard.rs:341` 附近）。
- 根因不是"Windows 不支持"，而是**只实现了最小格式**，且没有协议层的格式协商/按需拉取。

### 4.2 格式矩阵

| 格式 | 传输 | 阶段 | 风险/坑 |
|---|---|---|---|
| `CF_UNICODETEXT` | UTF-8 单帧 | P0 | CRLF/NUL 终止、代理对、长度上限 |
| `"HTML Format"`（注册名） | UTF-8 + 重算头偏移 | P0 | `<script>`/`<iframe>`/外链/`base` → 投毒；必须清洗 |
| `"Rich Text Format"` | 单帧，可降级纯文本 | P0 | `\objdata` 可嵌 OLE 对象；默认只提取文本或整段禁用 |
| `"PNG"`（注册名）+ `CF_DIBV5` | 原始 PNG 字节；必要时 DIB→PNG | P0 | 解码炸弹、超大尺寸；DIB 无 alpha、自底向上 |
| `CF_HDROP` | 只传文件元数据，内容走现有 SFTP/文件通道 | P1 | 路径泄露、UNC、覆盖/重解析点；落地到随机临时目录且不可执行 |
| `"FileGroupDescriptorW"` + `"FileContents"` | 虚拟文件流式分块 | P2 | 需要 OLE/延迟渲染语义，主要用于 Outlook 附件 |
| `"Shell IDList Array"` | 拒绝 | — | PIDL 无法跨机重建 |
| 自定义注册格式 | 白名单 + 原始字节 | P3 | 解析器攻击面 |

依据：[Standard Clipboard Formats](https://learn.microsoft.com/en-us/windows/win32/dataxchg/standard-clipboard-formats)、[HTML Clipboard Format](https://learn.microsoft.com/en-us/windows/win32/dataxchg/html-clipboard-format) `[官方文档]`

### 4.3 协议 v2（复用 MS-RDPECLIP 骨架）

不照抄 RDP 传输层，但复用它的"先能力协商、再格式列表、按需拉取、文件按 range 读"骨架（[MS-RDPECLIP](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpeclip/) `[官方文档]`）：

```
HELLO   { ver, caps: { formats:[{id,name,mime,max}], features:[inline,stream,lock], limits } }
ANNOUNCE{ seq, origin, hash, items:[{i, fmtId, name, mime, size, sha256}] }   // 只发元数据
PULL    { reqId, hash, i, off, len }                                          // 用户粘贴/点击时才拉
DATA    { reqId, off, eof, payload }                                          // 64 KiB 分块
LOCK/UNLOCK { clipId, ttl }                                                   // 多粘贴期间钉住内容
ERROR   { reqId, code, detail }
```

- **通道拆分**：`ruthenium-clipboard` 保留为控制面（授权/小对象），同 SSH 会话内新增第二条 channel（同 subsystem 名，HELLO 里 `role=data`）承载大对象，避免 1 GiB 文件阻塞文本。
- **大小/配额**：单项 ≤ 2 GiB、会话配额 1 GiB、chunk 64 KiB、请求超时 15s、重试 3 次；`sha256` 前 16 字节做去重（LRU 32 项）。
- **循环抑制**（三重）：写入后记录 `GetClipboardSequenceNumber`（变了才读）+ 内容哈希回声过滤 + 私有 owner 标记格式 "Ruthenium.Ignore"（读到即跳过）。`GetClipboardSequenceNumber` 在内容变化/清空时递增，但延迟渲染要到实际渲染才递增，不能只靠它。
  [GetClipboardSequenceNumber](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-getclipboardsequencenumber) `[官方文档]`
- **授权**：沿用 `grant_clipboard` + 会话令牌；文件格式单独二次确认（文件比文本危险得多）。
- **审计**：JSONL 只记 `{ts, peer, dir, formats, sizes, sha256}`，**不落内容**。

### 4.4 实现要点

- **监听**：消息专用窗口 `HWND_MESSAGE` + `AddClipboardFormatListener` → `WM_CLIPBOARDUPDATE`（Vista+，优于旧 `SetClipboardViewer`）；`OpenClipboard` 全系统单写者，失败需退避重试；所有剪贴板调用固定在一条 STA 线程（`OleInitialize`），经 mpsc 串行化。
  [AddClipboardFormatListener](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-addclipboardformatlistener) `[官方文档]`
- **内存所有权**：写入 `GlobalAlloc(GMEM_MOVEABLE)` → `GlobalLock` → 拷贝 → `GlobalUnlock` → `SetClipboardData`（成功即所有权转移，失败 `GlobalFree`）；读取只拷贝，不长期锁、不 `GlobalFree`。
- **OLE/延迟渲染**：`OleSetClipboard(IDataObject*)` 放入的数据对象默认延迟渲染；Excel/Word/Outlook 富格式、虚拟文件、拖放必须走 OLE。**"按需拉取"应实现在协议层**，不要把远端请求挂到本机 `WM_RENDERFORMAT`（同步回调会卡 UI，且语义不对）。
  [HTML Clipboard Format](https://learn.microsoft.com/en-us/windows/win32/dataxchg/html-clipboard-format)、[WinRT Clipboard](https://learn.microsoft.com/en-us/uwp/api/windows.applicationmodel.datatransfer.clipboard) `[官方文档]`
- **WinRT 取舍**：`Windows.ApplicationModel.DataTransfer.Clipboard` 只能在**前台/UI 线程**访问，适合查看端 UI 内的轻量操作，不适合被控端常驻监听/自定义 HGLOBAL/延迟渲染；服务端继续用 Win32。
- **Rust 依赖**：前期可用 `arboard`/`clipboard-win` 快速覆盖文本/图片；文件、延迟渲染、自定义格式再下探 `windows` crate（`Win32_System_DataExchange`、`Win32_System_Memory`、`Win32_System_Ole`、`Win32_System_Com_StructuredStorage`）。
- **前端 IPC**：用 `tauri::ipc::Response` 返回 `ArrayBuffer` 或 `Channel<&[u8]>` 分块，**不要 base64**；预览用 Blob URL 并及时 revoke。

### 4.5 安全

- 按"格式名 + 内容校验"双白名单；HTML 用 `ammonia` 清洗（去 `script/iframe/object/embed/link/style/@import`、删 `base`、重写远程 URL、≤1 MiB）；RTF 默认只提取文本。
- 图片限制 ≤ 16384² 像素、解码 ≤ 64 MiB；DIB 校验 `biSize`/`biHeight`。
- 文件拒绝 `..`、UNC、ADS、重解析点；只下载到随机临时目录且不可执行；下载完成前不写本地剪贴板。
- 绝不自动渲染/自动拉取 HTML 外链（防剪贴板投毒）。
- 密码管理器内容：优先靠 `CanIncludeInClipboardHistory=0`/`ExcludeClipboardContentFromMonitorProcessing` 或 owner 进程名排除；做不到时在 UI 明示"剪贴板内容会同步"。
- 渲染/粘贴路径与"是否自动拉取"分离：默认只在用户主动粘贴时拉取，避免后台把大对象/敏感内容搬来搬去。

### 4.6 Linux / SSH 主机

抽象 `trait ClipboardBackend { list(); read(fmt); write(items) }`：

- SSH 纯文本走 OSC 52（`ESC ] 52 ; c ; base64 BEL`），需终端支持，tmux 可能拦截；
- 图片检测 `wl-paste -t image/png` / `xclip -t image/png -o`，可用则转 PNG 走同一 PULL；
- 无图形会话只开文本。

### 4.7 工作量与验收

| 项 | 人日 | 验收 |
|---|---|---|
| 协议 v2 + 通道拆分 + 循环抑制 | 4–5 | 双端同时写不打架；同一内容只 ANNOUNCE 一次；断线重连可恢复 |
| P0 格式（文本/HTML/RTF/PNG/DIB） | 4–5 | Office/浏览器/截图工具互拷；HTML 被清洗；1 MiB 文本/8K 图正常 |
| P1 文件（CF_HDROP + SFTP） | 8–10 | 资源管理器复制 1 GiB 文件，进度/取消/校验；路径攻击被拒 |
| P2 Outlook 附件/自定义白名单 | 5–8 | 附件可粘贴；未知格式被忽略 |
| 测试脚本/夹具 | 2–3 | PowerShell 构造多格式剪贴板，CI 断言 |

---

## 5. 音频：设备跟随、指定应用、麦克风混音

### 5.1 现状与根因

| 现状 | 问题 |
|---|---|
| `GetDefaultAudioEndpoint(eRender, eConsole)` 一次性取默认渲染设备 | 用户切默认设备后不跟随；蓝牙/显示器音频切换会失效 |
| `AUDCLNT_STREAMFLAGS_LOOPBACK` + 5ms `sleep` 轮询 | 非事件驱动，空转唤醒；应改 `AUDCLNT_STREAMFLAGS_EVENTCALLBACK`（Win10 1703+ 支持事件驱动 loopback） |
| 手写线性插值重采样到 48k | 无抗混叠，44.1k→48k 有可闻损失；应交给 WASAPI `AUTOCONVERTPCM\|SRC_DEFAULT_QUALITY` |
| 只取前两个声道 | 5.1/7.1 时中置对白/环绕丢失；需按 `dwChannelMask` 下混 |
| 不采麦克风、不跟随设备、不支持指定应用 | 功能缺口 |
| Opus 64kbps + `Signal::Music` + DTX | 64kbps 偏低（RFC 7587 建议音乐 64–128kbps）；**DTX 只作用于 LPC 层**，在 `Audio/Music` 模式下基本无效 |
| 丢包只插静音 | WebCodecs `AudioDecoder` 无 PLC API；要么接受，要么 Rust 侧解码 |

依据：[Loopback Recording](https://learn.microsoft.com/en-us/windows/win32/coreaudio/loopback-recording)、[AUDIOCLIENT_ACTIVATION_TYPE](https://learn.microsoft.com/en-us/windows/win32/api/audioclientactivationparams/ne-audioclientactivationparams-audioclient_activation_type)、[IMMNotificationClient](https://learn.microsoft.com/en-us/windows/win32/api/mmdeviceapi/nn-mmdeviceapi-immnotificationclient)、[Application loopback audio capture](https://learn.microsoft.com/en-us/samples/microsoft/windows-classic-samples/applicationloopbackaudio-sample/)、[Opus Encoder CTLs](https://opus-codec.org/docs/opus_api-1.3.1/group__opus__encoderctls.html) `[官方文档]`

### 5.2 目标模型

```
音频源（可多选）：
  ├─ 系统声：默认渲染设备 loopback（跟随切换，P0）
  ├─ 指定应用：进程级 loopback（Win10 2004+，官方口径 20348+，运行时探测，P1）
  └─ 麦克风：默认录音端点（P1）
        ↓ 统一 48 kHz / 2ch / f32
   混音（软限幅、AGC 仅麦克风、漂移补偿）
        ↓
   Opus 96kbps VBR / 20ms（P0 调参；需要更低延迟可 10ms）
        ↓
   每帧带 host QPC 时间戳（48k 时钟）+ seq
```

默认"系统声（跟随默认设备）"；"指定应用"和"麦克风"作为设置项，会话内只保留总开关/静音；被控端需要授权提示与"正在采集"指示。

### 5.3 实现方案

1. **设备跟随（P0，2–3 人日）**
   - `IMMDeviceEnumerator::RegisterEndpointNotificationCallback` 实现 `IMMNotificationClient`，关注 `OnDefaultDeviceChanged(eRender, eConsole, id)`、`OnDeviceStateChanged/Removed`；
   - 回调只置位/发信号（非阻塞、不释放最后一个 MMDevice 引用），由采集线程重建：旧 client Stop/Release → 重新取默认设备 → `GetMixFormat` → `Initialize` → `SetEventHandle` → `Start`；用 generation 计数丢弃旧线程残留数据；交叉淡入避免爆音。
2. **事件驱动 + 自动重采样（P0，1–2 人日）**
   - `Initialize(AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK, 0, 0, fmt, None)`（共享模式 event-driven 要求 `hnsBufferDuration=hnsPeriodicity=0`）；
   - 在格式里加 `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY`，让引擎直接给 48k stereo，删掉手写线性重采样；
   - `WaitForSingleObject(event, timeout)` 替代 5ms sleep；`IAudioClient3` 仅在需要更低周期时使用（20ms 帧收益有限）。
3. **进程级 loopback（P1，5–8 人日）**
   - `ActivateAudioInterfaceAsync(VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, __uuidof(IAudioClient), params, handler)`；
   - `AUDIOCLIENT_ACTIVATION_PARAMS { ActivationType = AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK; ProcessLoopbackParams { TargetProcessId, ProcessLoopbackMode = INCLUDE_TARGET_PROCESS_TREE/EXCLUDE_... } }`，`PROPVARIANT vt=VT_BLOB`；
   - 端点无关，默认设备切换不影响；目标无渲染流则静音；子进程树强制包含（OBS 的已知行为）；
   - **版本探测**：官方结构体文档写 Windows 10 Build 20348，但 OBS 28 起在 Windows 10 2004+ 提供该能力（社区实践）；本项目应 `build >= 20348` 才暴露，低版本运行时探测失败则降级为系统 loopback，不要写死版本号。
4. **麦克风 + 混音（P1，5–8 人日）**
   - 默认录音端点 + 共享模式 + `AUTOCONVERTPCM`；隐私设置（`ms-settings:privacy-microphone`）失败时给引导；
   - 系统声与麦克风各自环形缓冲，按 QPC/`GetBuffer` 的 device position 对齐；麦克风向系统声时钟靠拢（微调重采样比例或偶尔插/删帧）；
   - 软限幅（tanh/软膝，-1 dBFS）；AGC 只加麦克风；
   - 5.1/7.1：按 `dwChannelMask` 下混（L + 0.707C + 0.707Ls 等，预留 -3dB 防削波）。
5. **AEC/NS/AGC（P2，5–10 人日，可选）**
   - 只有"主机外放 + 开麦克风"才需要；系统 loopback 天然是 far-end 参考，主机侧做 AEC3 再混音；
   - 方案：`webrtc-audio-processing`（封装 WebRTC APM，含 AEC3/NS/AGC/VAD）或更轻的 SpeexDSP；Windows APO 只作用于端点管线，拿不到"任意应用 mix"，不适用；
   - 戴耳机时应关闭 AEC，避免损伤语音。
6. **Opus 与传输（P0，1–2 人日 + P1 调优）**
   - `OPUS_APPLICATION_AUDIO`；96 kbps VBR、complexity 6–8（现状 64k/5）；
   - **去掉 DTX**（`Audio` 模式基本不生效），改为混音端 VAD 门控；FEC 只在能接受音质偏向时开启（且仅 LPC 层有效）；
   - 每帧带 host QPC 的 48k 时间戳；查看端按时间戳排序 + 自适应抖动缓冲（80–120ms，按到达抖动 p95 调整），而不是仅按 seq；
   - 播放建议 `AudioWorklet` + 环形缓冲（现状 `AudioBufferSourceNode` 每包建 buffer 有调度抖动/GC 风险）；`new AudioContext({ sampleRate: 48000, latencyHint: 'interactive' })`；断线淡出 200ms 防爆音。
7. **稳定性**
   - `AUDCLNT_E_DEVICE_INVALIDATED`/`RESOURCES_INVALIDATED` 统一走重建；
   - 休眠唤醒重新枚举；独占模式应用导致系统声缺失时提示；DRM 音频静音属预期（WASAPI 文档明确受保护流不会被 loopback 捕获）。

### 5.4 权限、隐私与 EDR

- 麦克风受"设置 → 隐私 → 麦克风/允许桌面应用"控制，失败时深链引导，不要静默失败。
- 被控端横幅/托盘必须显示"正在采集系统声/麦克风"，可一键停止。
- 读取他进程音频对 EDR 敏感：代码签名 + 说明；首次开启麦克风/进程音频时给出本机确认。
- 进程 loopback 可能被企业策略/杀软拦截，失败时降级为系统 loopback 并提示。

### 5.5 工作量与验收

| 项 | 人日 | 验收 |
|---|---|---|
| 设备跟随 + 事件驱动 + 自动重采样 | 3–5 | 播放中切换默认设备不断音/不爆音；44.1k/5.1 设备输出正确 48k stereo |
| 进程 loopback | 5–8 | 选中播放器只出该应用声音；目标静音时为静音；跟随子进程；不支持版本降级 |
| 麦克风 + 混音 | 5–8 | 双源混音无削波；麦克风延迟 < 150ms；隐私关闭时有引导 |
| Opus/时间戳/抖动缓冲 | 2–3 | 丢包 5% 可听损伤可控；音画偏差 < 100ms |
| AEC（可选） | 5–10 | 外放双讲可懂、无明显回声；耳机场景自动关闭 |
| 测试脚本 | 2–3 | `audio-tone.ps1` 扩展 44.1k/5.1/长时；进程 loopback include/exclude；设备切换断言 |

---

## 6. 横切设计

### 6.1 能力探测与协商

启动时一次性探测并缓存（设置页可"重新检测"）：

- OS：build、SKU（是否 N 版）、媒体功能包存在性、Administrator protection 状态；
- GPU：`IDXGIFactory1` 枚举适配器（VendorId/DeviceId/LUID/驱动版本）、`IDXGIOutput6::GetDesc1` 色域/HDR；
- 编码器：MF（MFTEnum2+LUID）+ 厂商 SDK（dlopen 探测）→ `EncoderCaps` 列表；
- 采集：DDA/WGC 可用性、HDR、显示器拓扑；
- 音频：默认设备、进程 loopback 可用性、麦克风隐私状态；
- 剪贴板：窗口站/桌面是否允许（服务/会话）；
- 对端：`VideoDecoder.isConfigSupported`、支持的剪贴板格式、音频编解码能力。

### 6.2 会话状态机与错误模型

统一结构化错误（前端按 code 显示文案，不靠字符串匹配）：

```
CaptureBlocked { reason: SecureDesktop | ProtectedWindow | ExclusiveFullscreen | DrmContent | SessionDisconnected }
CaptureDegraded { from, to, reason }          // DDA→WGC、硬编→软编、系统声→静音
AudioSourceLost { kind }                       // 默认设备切换/进程退出/麦克风权限
ClipboardRejected { reason }                   // 大小/格式/授权/清洗失败
EncoderUnavailable { candidates, detail }      // 诊断导出
```

### 6.3 授权模型扩展

现有六项独立授权（文件/终端/推送/屏幕/剪贴板/声音）需要新增：

| 新授权项 | 说明 |
|---|---|
| `grant_secure_desktop` | 安全桌面采集+注入（仅增强模式可用），默认关，逐会话确认 |
| `grant_clipboard_files` | 文件剪贴板（比文本危险），二次确认 |
| `grant_audio_mic` | 麦克风采集，默认关，被控端可见指示 |
| `grant_audio_app` | 指定应用音频（可选） |

复核周期沿用现有 5s 授权轮询；安全桌面会话断开立即失效。

### 6.4 可观测性

- 日志字段：`session_id`、`peer_id`、`capture_backend`、`access_lost/min`、`rebuild_ms`、`black_ratio`、`encoder`、`codec`、`bitrate`、`fps`、`rtt`、`audio_src`、`clip_fmt/size`。
- 设置页"诊断"：导出能力表 + 最近 N 条状态迁移 + 错误码，方便用户反馈。
- 指标埋点默认只写本地日志（项目原则：不上报遥测）。

### 6.5 依赖与版本策略

- `windows` crate：现有 `0.62` 已覆盖大部分；新增需要 `Win32_Media_Audio_Endpoints`、`Win32_Devices_FunctionDiscovery`、`Win32_UI_Shell_PropertiesSystem`、`Win32_System_RemoteDesktop`、`Win32_System_Services`、`Win32_System_Security`（令牌）、`Win32_Security_WinTrust`（签名校验）、`Win32_Graphics_Dwm`、`Win32_Media_Audio` 已有。
- 新增 crate：`windows-service`（服务）、`libloading`（厂商 SDK 动态加载）、`rubato`（可选重采样）、`ammonia`（HTML 清洗）、`webrtc-audio-processing`（可选 AEC，C++ 依赖，需评估构建）。
- 新特性按 OS build 探测启用；SDK 用 26100（24H2）为基线，26H1/26H2 新成员（如 `IncludeSecondaryWindows`、`SetWindowExclusionList`）用 `IsSupported`/接口 query 做能力探测，不要静态依赖。

---

## 7. 里程碑与排期

假设 1 名主力 + 1 名兼职测试/签名支持。

| 里程碑 | 内容 | 人日 | 产出 |
|---|---|---|---|
| **M1 不再骗人**（1–2 周） | 安全桌面检测/暂停/恢复；FSE/WDA/DRM 检测与文案；音频设备跟随+自动重采样+去 DTX；编码器诊断表；`SECURE_DESKTOP` 授权项占位 | 12–18 | 用户不再面对黑屏假死；日志能回答"为什么没画面/没声音/没硬编" |
| **M2 可用性补齐**（3–5 周） | MF 硬编修正（异步/LUID/D3D11）；剪贴板 P0（文本/HTML/RTF/PNG/DIB）；音频事件驱动+麦克风混音基础；前端 codec 降级加固 | 25–35 | 硬编成功率显著提升；富文本/图片可同步；系统声+麦克风可用 |
| **M3 增强模式**（6–10 周） | SYSTEM 服务 + helper（安全桌面完整支持）；进程 loopback 指定应用；剪贴板文件传输；NVENC/AMF SDK 实验；HDR 实验 | 40–60 | "UAC/锁屏可看可控"（安装增强组件后）；文件剪贴板；更稳的硬编 |
| **M4 长期**（季度级） | IddCx 虚拟显示器；AEC；Outlook 附件/自定义格式；AV1/HEVC 正式支持；HDR 全链路 | 按需 | 差异化能力 |

发布建议：

- M1 进 `0.1.x` 补丁（纯检测/文案/音频参数，无架构风险）；
- M2 进 `0.2.0`（编码器/剪贴板协议变更，需要前后端协议兼容处理）；
- M3 进 `0.3.0`，安装器增加"增强模式（可选）"勾选，默认不装服务。

---

## 8. 测试计划

### 8.1 测试夹具

| 夹具 | 用途 |
|---|---|
| `fixtures/uac-trigger.exe`（`requireAdministrator`） | 触发 UAC 安全桌面；断言 helper 采集/注入 |
| `fixtures/fse-test`（D3D11 全屏 swapchain） | 真 FSE 黑屏/冻结用例 |
| `fixtures/wda-window`（`SetWindowDisplayAffinity(0x11)`） | 受保护窗口检测/UX 用例 |
| `fixtures/clip-multi.ps1` | 构造文本/HTML/RTF/PNG/DIB/HDROP，验证双端同步与清洗 |
| `fixtures/audio-multi.ps1` + `scripts/audio-tone.ps1` 扩展 | 44.1k/48k/5.1、进程 loopback、设备切换、漂移 |
| `fixtures/encoder-matrix`（可选） | 各 GPU/驱动下探测与编码冒烟 |

### 8.2 自动化

- 单元：桌面名解析、状态机迁移、协议编解码、HTML 清洗、DIB 校验、能力矩阵选择。
- 集成：本机双实例（被控/查看）跑真实 SSH 子系统；断言帧率、错误码、降级路径。
- CI：GitHub Actions 托管 runner 无交互桌面，安全桌面/FSE/多 GPU 用例需要 **self-hosted Win11 VM**（自动登录 + 快照复位）；其余用例可托管。
- 回归矩阵：Win11 24H2/25H2/26H2 × NVIDIA/AMD/Intel 核显/独显 × 单/多显示器 × 1080p/4K/竖屏 × HDR on/off。

### 8.3 验收总则

- 任一降级/失败路径必须有结构化错误码与用户文案，不允许静默黑屏。
- 新增后台线程/服务必须有 CPU/内存基线（空闲 < 1%、1080p30 编码 < 10% CPU）。
- 安全相关（服务、管道、剪贴板文件、麦克风）必须通过越权/路径/大小/签名负向测试。

---

## 9. README / 产品文案更新建议

当前（`README.md:156`）：

> 已知边界：UAC / 安全桌面内无法捕获或注入；独占全屏、受保护内容与 DRM 窗口可能黑屏或跳帧；H.264 硬编因厂商驱动而异；剪贴板只同步纯文本；声音只采默认播放设备的 loopback（不含麦克风）。

建议按里程碑分阶段替换：

**M1 后**

> 已知边界：UAC / 安全桌面与锁屏画面需要安装"增强模式"组件后才可查看与操作，未安装时会明确提示并暂停；真独占全屏与 DRM 内容无法采集；受保护窗口（防截屏）不可采集；剪贴板当前仅同步文本；声音仅采集默认播放设备的系统声。

**M2 后**

> 已知边界：UAC / 安全桌面与锁屏需要安装"增强模式"组件（可选，默认不装）；真独占全屏、受保护窗口与 DRM 内容无法采集；剪贴板支持文本 / HTML / RTF / 图片，文件传输在下一版本；声音支持系统声（跟随设备切换）与麦克风混音；AV1/HEVC 编码在实验阶段。

**M3 后**

> 平台限制：Windows 对安全桌面（UAC/锁屏）的访问只允许以 SYSTEM 运行的可选增强组件，安装后支持查看与操作；真独占全屏（Win11 24H2+ 不进入合成桌面）、应用主动开启的防截屏窗口、DRM 受保护内容仍无法采集，这是系统安全设计，不做绕过；剪贴板支持文本 / HTML / RTF / 图片 / 文件；音频支持系统声、指定应用与麦克风，可选回声消除。

另外建议在 README 增加：

- **系统要求**：Windows 11 24H2+ 完整体验；Windows 10 22H2 降级支持（进程音频、部分采集特性不可用）；Windows 10 已结束支持。
- **安全说明**：增强模式需要管理员安装签名服务；每次会话本机确认；审计与一键停止；不上传遥测。
- **不支持清单**：DRM、防截屏窗口、真独占全屏——写清楚比让用户猜更好。

---

## 10. 风险登记

| 风险 | 影响 | 缓解 |
|---|---|---|
| 增强模式被 EDR/杀软判定为远控木马 | 安装失败/用户信任受损 | 代码签名（EV/Artifact Signing）、Defender/SmartScreen 申诉、开源、无隐藏安装、可见确认与审计 |
| 微软继续收紧远程输入注入（KB5080542 方向） | 锁屏/登录注入可能失效 | 优先走凭证提供者/官方认证接口；把"安全桌面支持"做成可降级能力；及时跟进 Windows 更新 |
| Administrator protection 改变提权模型 | 老方案/测试用例失效 | 服务化设计（官方建议方向）；24H2/25H2/26H2 回归 |
| FSE/DRM 用户预期管理失败 | 差评/工单 | 文档 + UI 明确边界；检测到场景主动提示 |
| 编码器碎片化维护成本 | 长期人力 | 能力探测 + 回退链 + 诊断表；优先 FFmpeg 统一封装或只做 NVENC |
| 剪贴板文件安全（路径/执行） | 安全问题 | 临时目录、不可执行、二次确认、路径校验、大小限制 |
| 麦克风隐私/合规 | 用户投诉 | 显式授权、可见指示、默认关闭 |
| 服务安装/升级/卸载复杂度 | 稳定性事故 | 独立 CI、SCM API、幂等安装、失败回滚、审计 |

---

## 11. 参考资料

**Microsoft Learn（已核对原文）**

- [Desktops](https://learn.microsoft.com/en-us/windows/win32/winstation/desktops) — WinSta0 三个桌面、UAC 切 Winlogon、DACL 只放行 LocalSystem
- [How User Account Control works](https://learn.microsoft.com/en-us/windows/security/application-security/application-control/user-account-control/how-it-works) — 安全桌面定义、只有 Windows 进程可访问
- [Security Considerations for Assistive Technologies](https://learn.microsoft.com/en-us/windows/win32/winauto/uiauto-securityoverview) — UIAccess 不能进入 system IL；签名/受信位置要求
- [Application manifests](https://learn.microsoft.com/en-us/windows/win32/sbscs/application-manifests) — `uiAccess` 语义
- [SendSAS](https://learn.microsoft.com/en-us/windows/win32/api/sas/nf-sas-sendsas) — 服务或 uiAccess + 策略
- [Interactive Services](https://learn.microsoft.com/en-us/windows/win32/services/interactive-services) — 服务不能直接交互，CreateProcessAsUser + IPC
- [CreateProcessAsUserW](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-createprocessasuserw)、[WTSQueryUserToken](https://learn.microsoft.com/en-us/windows/win32/api/wtsapi32/nf-wtsapi32-wtsqueryusertoken) — 令牌/会话/权限
- [IDXGIOutput1::DuplicateOutput](https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_2/nf-dxgi1_2-idxgioutput1-duplicateoutput)、[IDXGIOutput5::DuplicateOutput1](https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_5/nf-dxgi1_5-idxgioutput5-duplicateoutput1) — 安全桌面 E_ACCESSDENIED、非 DWM 不支持、HDR 格式
- [SetWindowDisplayAffinity](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-setwindowdisplayaffinity)、[GetWindowDisplayAffinity](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-getwindowdisplayaffinity) — WDA 语义、仅 DWM 合成时生效
- [Screen capture（WGC）](https://learn.microsoft.com/en-us/windows/apps/develop/media-authoring-processing/screen-capture)、[GraphicsCaptureSession](https://learn.microsoft.com/en-us/uwp/api/windows.graphics.capture.graphicscapturesession)、[IsBorderRequired](https://learn.microsoft.com/en-us/uwp/api/windows.graphics.capture.graphicscapturesession.isborderrequired) — 黄框、HDR FP16、能力要求
- [Protected Media Path](https://learn.microsoft.com/en-us/windows/win32/medfound/protected-media-path)、[GPU-Based Content Protection](https://learn.microsoft.com/en-us/windows/win32/medfound/gpu-based-content-protection) — PMP/OPM/HDCP
- [MFTEnumEx](https://learn.microsoft.com/en-us/windows/win32/api/mfapi/nf-mfapi-mftenumex)、[MFTEnum2](https://learn.microsoft.com/en-us/windows/win32/api/mfapi/nf-mfapi-mftenum2)、[Hardware MFTs](https://learn.microsoft.com/en-us/windows/win32/medfound/hardware-mfts)、[Asynchronous MFTs](https://learn.microsoft.com/en-us/windows/win32/medfound/asynchronous-mfts) — 硬件 MFT 异步、LUID、解锁/事件
- [Loopback Recording](https://learn.microsoft.com/en-us/windows/win32/coreaudio/loopback-recording)、[IAudioClient3](https://learn.microsoft.com/en-us/windows/win32/api/audioclient/nn-audioclient-iaudioclient3)、[AUDIOCLIENT_ACTIVATION_TYPE](https://learn.microsoft.com/en-us/windows/win32/api/audioclientactivationparams/ne-audioclientactivationparams-audioclient_activation_type)、[Application loopback audio capture](https://learn.microsoft.com/en-us/samples/microsoft/windows-classic-samples/applicationloopbackaudio-sample/)、[IMMNotificationClient](https://learn.microsoft.com/en-us/windows/win32/api/mmdeviceapi/nn-mmdeviceapi-immnotificationclient) — loopback/进程 loopback/设备通知
- [Standard Clipboard Formats](https://learn.microsoft.com/en-us/windows/win32/dataxchg/standard-clipboard-formats)、[HTML Clipboard Format](https://learn.microsoft.com/en-us/windows/win32/dataxchg/html-clipboard-format)、[AddClipboardFormatListener](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-addclipboardformatlistener)、[WinRT Clipboard](https://learn.microsoft.com/en-us/uwp/api/windows.applicationmodel.datatransfer.clipboard) — 剪贴板格式/监听/延迟渲染
- [MS-RDPECLIP](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpeclip/) — 远端剪贴板协议骨架
- [Administrator protection](https://learn.microsoft.com/en-us/windows/security/application-security/application-control/administrator-protection/) — 2026 提权模型变化
- [KB5080542](https://support.microsoft.com/en-us/servicing/os/windows/docs/2026/01/new-behavior-restricting-certain-applications-to-autofill-credentials-introduced-by-the-windows-janu) — 2026-01 认证对话框输入限制
- [Windows 11 release information](https://learn.microsoft.com/en-us/windows/release-health/windows11-release-information) — 版本/Build 基线

**社区 / 开源实现（二手证据，建议 PoC 复核）**

- [OBS #14021](https://github.com/obsproject/obs-studio/issues/14021) — Win11 24H2+ 真 FSE 下 DXGI/WGC 均冻结
- [OBS Application Audio Capture Guide](https://obsproject.com/kb/application-audio-capture-guide) — 进程 loopback 的 Windows 10 2004+ 实践口径
- [Parsec App for Windows](https://support.parsec.app/hc/en-us/articles/32381199341716-Parsec-App-for-Windows) — SYSTEM 服务 + SYSTEM/用户双进程模型
- [RustDesk #16562](https://github.com/rustdesk/rustdesk/pull/16562)、[#14671](https://github.com/rustdesk/rustdesk/pull/14671)、[#16969](https://github.com/rustdesk/rustdesk/pull/16969)、[#17145](https://github.com/rustdesk/rustdesk/pull/17145) — 服务/安全桌面/水印/覆盖层边界
- [MeshAgent #496](https://github.com/Ylianst/MeshAgent/pull/496)、[Vibepollo #743](https://github.com/Nonary/Vibepollo/pull/743) — SYSTEM 采集安全桌面的实测记录
- [NVIDIA Video Encode and Decode Support Matrix](https://developer.nvidia.com/video-encode-decode-support-matrix) — NVENC 代际/并发/AV1/4:4:4
- [Opus Encoder CTLs](https://opus-codec.org/docs/opus_api-1.3.1/group__opus__encoderctls.html) — DTX/FEC 仅 LPC 层
- [17 U.S.C. §1201](https://www.law.cornell.edu/uscode/text/17/1201)、[HDCP 规范](https://www.digital-cp.com/hdcp-specifications) — 绕过 DRM/HDCP 的法律风险

---

## 附录 A 关键 API 速查

| 目的 | API / 接口 | 最低版本 |
|---|---|---|
| 判断当前输入桌面 | `OpenInputDesktop` + `GetUserObjectInformation(UOI_NAME)` | Vista |
| 从服务启动交互进程 | `CreateProcessAsUserW` + `SetTokenInformation(TokenSessionId)` | XP |
| 安全桌面采集 | `IDXGIOutput1::DuplicateOutput`（SYSTEM）或 `BitBlt`/`GetDC` | Win8 / Win7 |
| SAS | `SendSAS`（服务或 uiAccess + 策略） | Win7 |
| 桌面复制 | `IDXGIOutputDuplication::AcquireNextFrame` | Win8 |
| 窗口/显示器采集 | `IGraphicsCaptureItemInterop` + `Direct3D11CaptureFramePool` | Win10 1803/1903 |
| 采集边框控制 | `GraphicsCaptureSession.IsBorderRequired` + `GraphicsCaptureRequestAccessAsync` | 10.0.20348 |
| HDR 复制格式 | `IDXGIOutput5::DuplicateOutput1` + `IDXGIOutput6::GetDesc1` | Win10 |
| 受保护窗口检测 | `GetWindowDisplayAffinity`（`WDA_EXCLUDEFROMCAPTURE=0x11`） | Win7/2004 |
| 硬编枚举 | `MFTEnum2` + `MFT_ENUM_ADAPTER_LUID` | Win10 |
| 硬编异步 | `MF_TRANSFORM_ASYNC_UNLOCK` + `IMFMediaEventGenerator` | Win7 |
| 硬编 D3D11 | `MF_SA_D3D11_AWARE` + `MFT_MESSAGE_SET_D3D_MANAGER` | Win8 |
| 系统 loopback | `IAudioClient::Initialize(LOOPBACK\|EVENTCALLBACK\|AUTOCONVERTPCM)` | Win10 1703（事件驱动） |
| 进程 loopback | `ActivateAudioInterfaceAsync(AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK)` | 文档 20348，实践 19041+ |
| 设备变化 | `IMMDeviceEnumerator::RegisterEndpointNotificationCallback` | Vista |
| 剪贴板监听 | `AddClipboardFormatListener` → `WM_CLIPBOARDUPDATE` | Vista |
| 剪贴板延迟渲染 | `SetClipboardData(fmt, NULL)` + `WM_RENDERFORMAT` / `OleSetClipboard` | Win2000+ |
| 剪贴板文件 | `CF_HDROP` + `DROPFILES`；`FileGroupDescriptorW` + `FileContents` | Win2000+ |

## 附录 B 与业界方案对照

| 产品 | 安全桌面 | FSE | 受保护内容/DRM | 剪贴板 | 音频 |
|---|---|---|---|---|---|
| Parsec | ✅ 系统服务（文档明说 lock screen/UAC） | 仅窗口化/无边框 | 不采集 | 文本为主 | 系统声 + 麦克风（虚拟麦克风驱动） |
| RustDesk | ✅ 服务路线（合入中/原型） | 不支持 | 不采集 | 文本/图片/文件（cliprdr） | 系统声 |
| OBS | ❌ 纯用户态 | 24H2+ 双后端均冻结 | 不采集 | — | 系统声/应用音频 |
| Sunshine/Apollo | ✅ SYSTEM | 有限 | 不采集 | — | 系统声 |
| mstsc（RDP） | 不支持安全桌面画面（黑屏） | 会踢出全屏 | 受保护内容不呈现 | RDPECLIP 全格式 | RDP 音频重定向 |
| Ruthenium 目标 | M3 可选增强 | 检测+提示 | 明确不支持 | P0 富文本/图片，P1 文件 | P0 系统声，P1 麦克风/应用 |
