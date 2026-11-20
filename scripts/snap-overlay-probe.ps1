# Windows 11 贴靠布局（Snap Layouts）桌面探针 —— 被测代码见 src-tauri/src/snap.rs
#
# 贴靠布局的唯一前提：窗口要对「最大化按钮那块矩形」回答 WM_NCHITTEST = HTMAXBUTTON。
# 而 WebView2 是铺满客户区的子窗口，父窗口收不到那个点，所以本项目在按钮上盖了一个
# 原生子窗口（类名 RutheniumSnapOverlay）代答。这件事**在浏览器里验不出来**：
# E2E 只跑到 mock 前端，这里量的是真实窗口、真实鼠标、真实窗口消息。
#
# 逐项断言：
#   1. 覆盖层子窗口存在且尺寸为 46×40 CSS 像素（按窗口 DPI 换算成物理像素）
#   2. 覆盖层贴住标题栏顶部；宽窗口（>860 CSS，标题栏无右侧内边距）时 left + 2×w == 客户区宽
#   3. WM_NCHITTEST 返回 9（HTMAXBUTTON）—— 贴靠浮层出现的唯一条件
#   4. WindowFromPoint 命中覆盖层（真实指针也在它上面，不是只有消息层面）
#   5. WM_GETMINMAXINFO 的最小尺寸 == 560×400（最小尺寸大于目标分区时，浮层照弹但窗口贴不进去）
#   6. 真实鼠标点击最大化 / 再点还原；程序化 WM_NCLBUTTONUP 路径同样生效
#   7. Win+← 真的贴到屏幕左半（最小尺寸不够时唯一会失败的一项）
#   8. 悬停截图：贴靠浮层是独立的 OS 窗口，PrintWindow 抓不到它，只能 CopyFromScreen，
#      所以先把窗口置顶并断言窗口自己拥有采样点，再截屏
#
# 警告：脚本会**接管真实鼠标与键盘**（移动指针、发 Win+←、点最大化按钮），并结束
# 已在运行的 Ruthenium 进程。请在没人用这台机器的时候跑。
#
# 前置：debug 版 exe 走 devUrl（cargo build 出来的是 dev 配置），先 `npm run dev`。
#   npm run dev
#   powershell -ExecutionPolicy Bypass -File scripts/snap-overlay-probe.ps1
# 也可以用 -Exe 指向其它构建产物（例如 tauri build --debug 的 exe）。
param(
  [string]$Exe = '',
  [string]$Out = '',
  [int]$DebugPort = 9444
)
$ErrorActionPreference = 'Stop'
# 输出统一 UTF-8：否则管道/重定向到文件时中文按控制台代码页编码，日志会变成乱码
try { [Console]::OutputEncoding = New-Object Text.UTF8Encoding $false } catch { }
Add-Type -AssemblyName System.Drawing | Out-Null
Add-Type -TypeDefinition @'
using System;
using System.Text;
using System.Runtime.InteropServices;

public static class W32 {
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
    [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X, Y; }
    [StructLayout(LayoutKind.Sequential)] public struct MONITORINFO { public int cbSize; public RECT rcMonitor; public RECT rcWork; public uint dwFlags; }
    [StructLayout(LayoutKind.Sequential)] public struct MINMAXINFO {
        public POINT ptReserved; public POINT ptMaxSize; public POINT ptMaxPosition; public POINT ptMinTrackSize; public POINT ptMaxTrackSize;
    }
    public delegate bool EnumProc(IntPtr hWnd, IntPtr lParam);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr p);
    [DllImport("user32.dll")] public static extern bool EnumChildWindows(IntPtr h, EnumProc cb, IntPtr p);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetClassName(IntPtr h, StringBuilder sb, int n);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
    [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")] public static extern bool ScreenToClient(IntPtr h, ref POINT p);
    [DllImport("user32.dll")] public static extern IntPtr SendMessage(IntPtr h, uint msg, IntPtr w, IntPtr l);
    [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr h, uint msg, IntPtr w, IntPtr l);
    [DllImport("user32.dll")] public static extern IntPtr WindowFromPoint(POINT p);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern bool IsZoomed(IntPtr h);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
    [DllImport("user32.dll")] public static extern uint GetDpiForWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr after, int x, int y, int cx, int cy, uint flags);
    [DllImport("user32.dll")] public static extern bool SetProcessDpiAwarenessContext(IntPtr ctx);
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern bool GetCursorPos(out POINT p);
    [DllImport("user32.dll")] public static extern void mouse_event(uint f, int dx, int dy, uint d, IntPtr e);
    [DllImport("user32.dll")] public static extern void keybd_event(byte bVk, byte bScan, uint dwFlags, UIntPtr dwExtraInfo);
    [DllImport("user32.dll")] public static extern IntPtr MonitorFromWindow(IntPtr h, uint flags);
    [DllImport("user32.dll")] public static extern bool GetMonitorInfo(IntPtr m, ref MONITORINFO mi);

    public static IntPtr FindTop(uint pid, string cls) {
        IntPtr found = IntPtr.Zero;
        W32.EnumWindows(delegate(IntPtr h, IntPtr p) {
            uint wpid; W32.GetWindowThreadProcessId(h, out wpid);
            if (wpid != pid) return true;
            var sb = new StringBuilder(256); W32.GetClassName(h, sb, 256);
            if (sb.ToString() == cls) { found = h; return false; }
            return true;
        }, IntPtr.Zero);
        return found;
    }

    public static IntPtr FindChildByClass(IntPtr parent, string cls) {
        IntPtr found = IntPtr.Zero;
        W32.EnumChildWindows(parent, delegate(IntPtr h, IntPtr p) {
            var sb = new StringBuilder(256); W32.GetClassName(h, sb, 256);
            if (sb.ToString() == cls) { found = h; return false; }
            return true;
        }, IntPtr.Zero);
        return found;
    }

    // WM_GETMINMAXINFO：窗口告诉 Windows 它最小能有多小。贴靠分区就是拿这个数比对的。
    // 注意 SetWindowPos 会绕过它（程序化改大小不受最小尺寸约束），所以不能用「缩到 400 看
    // 能不能缩」来测，必须直接读这个结构。
    public static int[] MinTrackSize(IntPtr h) {
        int size = Marshal.SizeOf(typeof(MINMAXINFO));
        IntPtr p = Marshal.AllocHGlobal(size);
        try {
            for (int i = 0; i < size; i++) Marshal.WriteByte(p, i, 0);
            W32.SendMessage(h, 0x0024, IntPtr.Zero, p);
            var mi = (MINMAXINFO)Marshal.PtrToStructure(p, typeof(MINMAXINFO));
            return new int[] { mi.ptMinTrackSize.X, mi.ptMinTrackSize.Y };
        } finally { Marshal.FreeHGlobal(p); }
    }

    public static string ClassName(IntPtr h) {
        var sb = new StringBuilder(256);
        W32.GetClassName(h, sb, 256);
        return sb.ToString();
    }
}
'@

# PER_MONITOR_AWARE_V2：不做这一步，WindowFromPoint/SetCursorPos 用虚拟化坐标、
# 而 GetWindowRect/GetDpiForWindow 给物理坐标，150% 缩放下算出来的点会偏移 1.5 倍，
# 且没有任何报错——所有合成点击都会落到窗口外面。
[void][W32]::SetProcessDpiAwarenessContext([IntPtr](-4))

if (-not $Exe) { $Exe = Join-Path (Split-Path -Parent $PSScriptRoot) 'src-tauri\target\debug\Ruthenium.exe' }
if (-not $Out) { $Out = Join-Path $env:TEMP 'ruthenium-snap-probe' }
if (-not (Test-Path $Exe)) { throw "找不到 $Exe（先 cargo build 或 npm run tauri:dev）" }
New-Item -ItemType Directory -Force -Path $Out | Out-Null

$WM_NCHITTEST = 0x0084
$HTMAXBUTTON = 9
$WM_NCLBUTTONDOWN = 0x00A1
$WM_NCLBUTTONUP = 0x00A2
$HWND_TOPMOST = [IntPtr](-1)
$HWND_NOTOPMOST = [IntPtr](-2)
$SWP_NOSIZE = 0x0001
$SWP_NOMOVE = 0x0002
$SWP_NOZORDER = 0x0004
$SWP_NOACTIVATE = 0x0010
$MOUSEEVENTF_LEFTDOWN = 0x0002
$MOUSEEVENTF_LEFTUP = 0x0004
$VK_LWIN = 0x5B
$VK_LEFT = 0x25
$KEYEVENTF_KEYUP = 0x0002

$fail = 0
function Check([string]$name, [bool]$ok, [string]$detail) {
  if ($ok) { Write-Host ("PASS  " + $name + "  | " + $detail) }
  else { Write-Host ("FAIL  " + $name + "  | " + $detail); $script:fail++ }
}

function Capture([string]$file, [int]$x, [int]$y, [int]$w, [int]$h) {
  $bmp = New-Object System.Drawing.Bitmap($w, $h)
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  $g.CopyFromScreen($x, $y, 0, 0, (New-Object System.Drawing.Size($w, $h)))
  $bmp.Save($file, [System.Drawing.Imaging.ImageFormat]::Png)
  $g.Dispose(); $bmp.Dispose()
}

Get-Process -Name Ruthenium -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 400
# 让 WebView2 带调试端口起来：下面用 -DebugPort 探针尺寸时方便，也留给人工 devtools。
$env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS = "--remote-debugging-port=$DebugPort"
$proc = Start-Process -FilePath $Exe -PassThru
$procId = $proc.Id
$main = [IntPtr]::Zero
$oldCursor = New-Object W32+POINT

try {
  for ($i = 0; $i -lt 200; $i++) {
    $main = [W32]::FindTop($procId, 'Tauri Window')
    if ($main -ne [IntPtr]::Zero) { break }
    Start-Sleep -Milliseconds 200
  }
  if ($main -eq [IntPtr]::Zero) { throw 'Tauri 窗口没出现（前端没起来？先 npm run dev）' }
  Write-Host ("INFO  main hwnd=0x{0:X}" -f $main.ToInt64())

  # 覆盖层由前端量完按钮矩形后创建，窗口 show 之后还要等一帧
  $overlay = [IntPtr]::Zero
  for ($i = 0; $i -lt 100; $i++) {
    $overlay = [W32]::FindChildByClass($main, 'RutheniumSnapOverlay')
    if ($overlay -ne [IntPtr]::Zero) { break }
    Start-Sleep -Milliseconds 200
  }
  Check '覆盖层已创建' ($overlay -ne [IntPtr]::Zero) ("hwnd=0x{0:X}" -f $overlay.ToInt64())
  if ($overlay -eq [IntPtr]::Zero) { throw '没有覆盖层子窗口' }
  Check '覆盖层可见' ([W32]::IsWindowVisible($overlay)) 'IsWindowVisible'

  $dpi = [W32]::GetDpiForWindow($main)
  $scale = $dpi / 96.0
  Write-Host ("INFO  dpi={0} scale={1}" -f $dpi, $scale)

  # 归一化起点：窗口状态插件会还原上次的尺寸（可能是 ≤860 CSS 的窄窗口），而宽档
  # 不变式（left+2w == 客户区宽）只在 >860 CSS 成立；先摆到已知宽尺寸 + 工作区左上角，
  # 保证后续断言与上次运行时的窗口状态无关。
  $mi0 = New-Object W32+MONITORINFO
  $mi0.cbSize = [System.Runtime.InteropServices.Marshal]::SizeOf([type][W32+MONITORINFO])
  [void][W32]::GetMonitorInfo([W32]::MonitorFromWindow($main, 2), [ref]$mi0)
  [void][W32]::SetWindowPos($main, [IntPtr]::Zero, $mi0.rcWork.Left + 8, $mi0.rcWork.Top + 8, [int](1280 * $scale), [int](800 * $scale), $SWP_NOZORDER -bor $SWP_NOACTIVATE)
  Start-Sleep -Milliseconds 900

  $cr = New-Object W32+RECT; [void][W32]::GetClientRect($main, [ref]$cr)
  $wr = New-Object W32+RECT; [void][W32]::GetWindowRect($overlay, [ref]$wr)
  $tl = New-Object W32+POINT; $tl.X = $wr.Left; $tl.Y = $wr.Top
  [void][W32]::ScreenToClient($main, [ref]$tl)
  $ow = $wr.Right - $wr.Left
  $oh = $wr.Bottom - $wr.Top
  Write-Host ("INFO  client={0}x{1}  overlay(client) = ({2},{3}) {4}x{5}  expect={6}x{7}" -f `
      $cr.Right, $cr.Bottom, $tl.X, $tl.Y, $ow, $oh, [math]::Round(46 * $scale), [math]::Round(40 * $scale))

  Check '覆盖层为 46x40 CSS' ([math]::Abs($ow - 46 * $scale) -le 2 -and [math]::Abs($oh - 40 * $scale) -le 2) `
    ("{0}x{1} vs {2}x{3}" -f $ow, $oh, [math]::Round(46 * $scale), [math]::Round(40 * $scale))
  Check '覆盖层贴住标题栏顶部' ([math]::Abs($tl.Y) -le 1) ("top={0}" -f $tl.Y)
  # 三键同宽（46）：最大化右边还有关闭键，所以 left + 2w == 客户区宽（>860 CSS 时标题栏无右侧内边距）
  Check '覆盖层右对齐（left+2w == 客户区宽）' ([math]::Abs(($tl.X + 2 * $ow) - $cr.Right) -le 2) `
    ("{0} + 2*{1} = {2} vs {3}" -f $tl.X, $ow, ($tl.X + 2 * $ow), $cr.Right)

  $lb = (([int64]$wr.Left + [int64][math]::Floor($ow / 2)) -band 0xFFFF) -bor ((([int64]$wr.Top + [int64][math]::Floor($oh / 2)) -band 0xFFFF) -shl 16)
  $hit = [W32]::SendMessage($overlay, $WM_NCHITTEST, [IntPtr]::Zero, [IntPtr]$lb).ToInt64()
  Check 'WM_NCHITTEST == HTMAXBUTTON' ($hit -eq $HTMAXBUTTON) ("got {0}" -f $hit)

  $cp = New-Object W32+POINT
  $cp.X = $wr.Left + [int][math]::Floor($ow / 2); $cp.Y = $wr.Top + [int][math]::Floor($oh / 2)
  Check 'WindowFromPoint 命中覆盖层' ([W32]::WindowFromPoint($cp) -eq $overlay) `
    ("0x{0:X} vs overlay 0x{1:X}" -f ([W32]::WindowFromPoint($cp)).ToInt64(), $overlay.ToInt64())

  # 最小尺寸：Windows 拿它跟贴靠分区比。最小宽度 560、最小高度 400 见 tauri.conf.json。
  $minTrack = [W32]::MinTrackSize($main)
  Check '最小尺寸为 560x400 CSS' ([math]::Abs($minTrack[0] - 560 * $scale) -le 2 -and [math]::Abs($minTrack[1] - 400 * $scale) -le 2) `
    ("ptMinTrackSize={0}x{1} vs {2}x{3}" -f $minTrack[0], $minTrack[1], [math]::Round(560 * $scale), [math]::Round(400 * $scale))

  # 宽窗口（>860 CSS，标题栏没有 8px 右侧内边距）：精确对齐不变式
  $orig = New-Object W32+RECT; [void][W32]::GetWindowRect($main, [ref]$orig)
  [void][W32]::SetWindowPos($main, [IntPtr]::Zero, $orig.Left, $orig.Top, [int](1100 * $scale), [int](700 * $scale), $SWP_NOZORDER -bor $SWP_NOACTIVATE)
  Start-Sleep -Milliseconds 900
  $wr2 = New-Object W32+RECT; [void][W32]::GetWindowRect($overlay, [ref]$wr2)
  $tl2 = New-Object W32+POINT; $tl2.X = $wr2.Left; $tl2.Y = $wr2.Top
  [void][W32]::ScreenToClient($main, [ref]$tl2)
  $cr2 = New-Object W32+RECT; [void][W32]::GetClientRect($main, [ref]$cr2)
  $ow2 = $wr2.Right - $wr2.Left
  Check '改窗口宽度后覆盖层跟随' ([math]::Abs(($tl2.X + 2 * $ow2) - $cr2.Right) -le 2) `
    ("1100 CSS 宽：left={0} w={1} clientW={2}" -f $tl2.X, $ow2, $cr2.Right)

  # 窄窗口（<=860 CSS）：标题栏多出 8px 右侧内边距，绝对位置不等于 clientW-2w，
  # 因此断言「覆盖层把按钮整块盖住」（三点采样）。
  [void][W32]::SetWindowPos($main, [IntPtr]::Zero, $orig.Left, $orig.Top, [int](700 * $scale), [int](500 * $scale), $SWP_NOZORDER -bor $SWP_NOACTIVATE)
  Start-Sleep -Milliseconds 900
  $wr2 = New-Object W32+RECT; [void][W32]::GetWindowRect($overlay, [ref]$wr2)
  $tl2 = New-Object W32+POINT; $tl2.X = $wr2.Left; $tl2.Y = $wr2.Top
  [void][W32]::ScreenToClient($main, [ref]$tl2)
  $cr2 = New-Object W32+RECT; [void][W32]::GetClientRect($main, [ref]$cr2)
  $ow2 = $wr2.Right - $wr2.Left
  $oh2 = $wr2.Bottom - $wr2.Top
  $midY = $wr2.Top + [int][math]::Floor($oh2 / 2)
  $pL = New-Object W32+POINT; $pL.X = $wr2.Left + 3; $pL.Y = $midY
  $pC = New-Object W32+POINT; $pC.X = $wr2.Left + [int][math]::Floor($ow2 / 2); $pC.Y = $midY
  $pR = New-Object W32+POINT; $pR.X = $wr2.Right - 4; $pR.Y = $midY
  Check '窄窗口：覆盖层仍是 46x40 CSS' ([math]::Abs($ow2 - 46 * $scale) -le 2 -and [math]::Abs($oh2 - 40 * $scale) -le 2) `
    ("{0}x{1}（700 CSS 宽）" -f $ow2, $oh2)
  Check '窄窗口：覆盖层盖住按钮左缘' ([W32]::WindowFromPoint($pL) -eq $overlay) 'WindowFromPoint(left+3)'
  Check '窄窗口：覆盖层盖住按钮中心' ([W32]::WindowFromPoint($pC) -eq $overlay) 'WindowFromPoint(centre)'
  Check '窄窗口：覆盖层盖住按钮右缘' ([W32]::WindowFromPoint($pR) -eq $overlay) 'WindowFromPoint(right-4)'
  Check '窄窗口：覆盖层仍在右侧' (($cr2.Right - ($tl2.X + $ow2)) -le [int](60 * $scale)) `
    ("距客户区右缘 {0} CSS（46 关闭键 + 8 内边距）" -f [math]::Round(($cr2.Right - ($tl2.X + $ow2)) / $scale))

  # 换成鼠标测试用的几何：窗口刚被改过大小，按钮位置变了
  $wr = $wr2
  $ow = $wr.Right - $wr.Left; $oh = $wr.Bottom - $wr.Top
  $cp = New-Object W32+POINT
  $cp.X = $wr.Left + [int][math]::Floor($ow / 2); $cp.Y = $wr.Top + [int][math]::Floor($oh / 2)

  $mi = New-Object W32+MONITORINFO
  $mi.cbSize = [System.Runtime.InteropServices.Marshal]::SizeOf([type][W32+MONITORINFO])
  [void][W32]::GetMonitorInfo([W32]::MonitorFromWindow($main, 2), [ref]$mi)
  $mon = $mi.rcMonitor
  Write-Host ("INFO  monitor=({0},{1})-({2},{3})" -f $mon.Left, $mon.Top, $mon.Right, $mon.Bottom)

  # 真鼠标：悬停截图。先置顶并确认自己是前台，否则 CopyFromScreen 拍到的是别的窗口。
  [void][W32]::SetWindowPos($main, $HWND_TOPMOST, 0, 0, 0, 0, $SWP_NOMOVE -bor $SWP_NOSIZE -bor $SWP_NOACTIVATE)
  Start-Sleep -Milliseconds 300
  for ($i = 0; $i -lt 3 -and [W32]::GetForegroundWindow() -ne $main; $i++) {
    [void][W32]::SetForegroundWindow($main); Start-Sleep -Milliseconds 400
  }
  Write-Host ("INFO  应用是前台窗口：{0}" -f ([W32]::GetForegroundWindow() -eq $main))

  # 遮挡守卫：真实点击只认覆盖层。桌面上别的置顶窗口（游戏覆盖层、截图/录屏工具、
  # UnrealWindow 类名的悬浮球等）盖住按钮坐标时，点击会落到外部窗口上——那不是
  # 应用缺陷，报出遮挡者类名，避免把环境问题算到应用头上。
  $occl = [W32]::WindowFromPoint($cp)
  Check '覆盖层点击点未被外部窗口遮挡' ($occl -eq $overlay) `
    ("点击点被 0x{0:X} '{1}' 挡住" -f $occl.ToInt64(), [W32]::ClassName($occl))

  [void][W32]::GetCursorPos([ref]$oldCursor)
  [void][W32]::SetCursorPos($cp.X - 500, $cp.Y + 300)
  Start-Sleep -Milliseconds 500

  $tbX = [Math]::Max($mon.Left, $wr.Left - 300); $tbY = [Math]::Max($mon.Top, $wr.Top - 8)
  $tbW = [Math]::Min($mon.Right - $tbX, 460); $tbH = 76
  $flyX = [Math]::Max($mon.Left, $wr.Right - 620); $flyY = $wr.Top
  $flyW = [Math]::Min($mon.Right - $flyX, 640); $flyH = [Math]::Min($mon.Bottom - $flyY, 620)

  Capture (Join-Path $Out '0-baseline-tb.png') $tbX $tbY $tbW $tbH
  Capture (Join-Path $Out '0-baseline-fly.png') $flyX $flyY $flyW $flyH

  [void][W32]::SetCursorPos($cp.X, $cp.Y)
  Start-Sleep -Milliseconds 1000
  Capture (Join-Path $Out '1-hover-tb.png') $tbX $tbY $tbW $tbH
  Capture (Join-Path $Out '1-hover-fly.png') $flyX $flyY $flyW $flyH
  Write-Host "INFO  贴靠浮层应出现在 $Out\1-hover-fly.png（浮层是独立 OS 窗口，只能屏幕截图）"

  $zoomedBefore = ([W32]::IsZoomed($main) -ne 0)
  [W32]::mouse_event($MOUSEEVENTF_LEFTDOWN, 0, 0, 0, [IntPtr]::Zero)
  Start-Sleep -Milliseconds 260
  Capture (Join-Path $Out '2-press-tb.png') $tbX $tbY $tbW $tbH
  [W32]::mouse_event($MOUSEEVENTF_LEFTUP, 0, 0, 0, [IntPtr]::Zero)
  Start-Sleep -Milliseconds 900
  # 断言「状态翻转且翻到最大化」：动态对比前置状态，即使前面某步意外翻转过也判得准
  $zoomedAfter = ([W32]::IsZoomed($main) -ne 0)
  Check '真实点击最大化' ($zoomedAfter -and $zoomedAfter -ne $zoomedBefore) `
    ("IsZoomed {0} → {1}" -f $zoomedBefore, $zoomedAfter)

  $wr3 = New-Object W32+RECT; [void][W32]::GetWindowRect($overlay, [ref]$wr3)
  $tl3 = New-Object W32+POINT; $tl3.X = $wr3.Left; $tl3.Y = $wr3.Top
  [void][W32]::ScreenToClient($main, [ref]$tl3)
  $cr3 = New-Object W32+RECT; [void][W32]::GetClientRect($main, [ref]$cr3)
  $ow3 = $wr3.Right - $wr3.Left
  Check '最大化后覆盖层仍右对齐' ([math]::Abs(($tl3.X + 2 * $ow3) - $cr3.Right) -le 2) `
    ("left={0} w={1} clientW={2}" -f $tl3.X, $ow3, $cr3.Right)

  $cp2 = New-Object W32+POINT
  $cp2.X = $wr3.Left + [int][math]::Floor($ow3 / 2); $cp2.Y = $wr3.Top + [int][math]::Floor(($wr3.Bottom - $wr3.Top) / 2)
  $occl2 = [W32]::WindowFromPoint($cp2)
  Check '还原点击点未被外部窗口遮挡' ($occl2 -eq $overlay) `
    ("点击点被 0x{0:X} '{1}' 挡住" -f $occl2.ToInt64(), [W32]::ClassName($occl2))
  [void][W32]::SetCursorPos($cp2.X, $cp2.Y)
  Start-Sleep -Milliseconds 400
  $zoomedBefore2 = ([W32]::IsZoomed($main) -ne 0)
  [W32]::mouse_event($MOUSEEVENTF_LEFTDOWN, 0, 0, 0, [IntPtr]::Zero)
  Start-Sleep -Milliseconds 130
  [W32]::mouse_event($MOUSEEVENTF_LEFTUP, 0, 0, 0, [IntPtr]::Zero)
  Start-Sleep -Milliseconds 1000
  $zoomedAfter2 = ([W32]::IsZoomed($main) -ne 0)
  Check '真实点击还原' ((-not $zoomedAfter2) -and $zoomedAfter2 -ne $zoomedBefore2) `
    ("IsZoomed {0} → {1}" -f $zoomedBefore2, $zoomedAfter2)

  # 程序化按钮消息路径（不依赖鼠标）。DOWN 与 UP 之间留 150ms，贴近真实点击节奏；
  # 断言同样是动态翻转对比。
  [void][W32]::SetCursorPos($cp.X - 500, $cp.Y + 300)
  Start-Sleep -Milliseconds 600
  $msgBefore = ([W32]::IsZoomed($main) -ne 0)
  [void][W32]::PostMessage($overlay, $WM_NCLBUTTONDOWN, [IntPtr]$HTMAXBUTTON, [IntPtr]::Zero)
  Start-Sleep -Milliseconds 150
  [void][W32]::PostMessage($overlay, $WM_NCLBUTTONUP, [IntPtr]$HTMAXBUTTON, [IntPtr]::Zero)
  Start-Sleep -Milliseconds 900
  $msgAfter = ([W32]::IsZoomed($main) -ne 0)
  Check 'WM_NCLBUTTONUP 切换到最大化' ($msgAfter -and $msgAfter -ne $msgBefore) `
    ("IsZoomed {0} → {1}" -f $msgBefore, $msgAfter)
  $msgBefore2 = ([W32]::IsZoomed($main) -ne 0)
  [void][W32]::PostMessage($overlay, $WM_NCLBUTTONDOWN, [IntPtr]$HTMAXBUTTON, [IntPtr]::Zero)
  Start-Sleep -Milliseconds 150
  [void][W32]::PostMessage($overlay, $WM_NCLBUTTONUP, [IntPtr]$HTMAXBUTTON, [IntPtr]::Zero)
  Start-Sleep -Milliseconds 900
  $msgAfter2 = ([W32]::IsZoomed($main) -ne 0)
  Check 'WM_NCLBUTTONUP 切换到还原' ((-not $msgAfter2) -and $msgAfter2 -ne $msgBefore2) `
    ("IsZoomed {0} → {1}" -f $msgBefore2, $msgAfter2)

  # 真贴靠：Win+← 必须落到工作区左半。这是最小尺寸不够时唯一会失败的一项。
  $mi2 = New-Object W32+MONITORINFO
  $mi2.cbSize = [System.Runtime.InteropServices.Marshal]::SizeOf([type][W32+MONITORINFO])
  [void][W32]::GetMonitorInfo([W32]::MonitorFromWindow($main, 2), [ref]$mi2)
  $work = $mi2.rcWork
  $workW = $work.Right - $work.Left; $workH = $work.Bottom - $work.Top
  Write-Host ("INFO  工作区=({0},{1})-({2},{3}) {4}x{5} 物理像素；最小尺寸 {6}x{7}" -f `
      $work.Left, $work.Top, $work.Right, $work.Bottom, $workW, $workH, $minTrack[0], $minTrack[1])
  Check '左/右半分放得进最小尺寸' ($workW / 2 -ge $minTrack[0] -and $workH -ge $minTrack[1]) `
    ("分区 {0}x{1} vs 最小 {2}x{3}" -f [math]::Floor($workW / 2), $workH, $minTrack[0], $minTrack[1])
  Check '1/3 分区放得进最小尺寸' ($workW / 3 -ge $minTrack[0]) `
    ("分区宽 {0} vs 最小 {1}" -f [math]::Floor($workW / 3), $minTrack[0])
  Check '2x2 分区放得进最小尺寸' ($workW / 2 -ge $minTrack[0] -and $workH / 2 -ge $minTrack[1]) `
    ("分区 {0}x{1} vs 最小 {2}x{3}" -f [math]::Floor($workW / 2), [math]::Floor($workH / 2), $minTrack[0], $minTrack[1])

  if ([W32]::GetForegroundWindow() -ne $main) { [void][W32]::SetForegroundWindow($main); Start-Sleep -Milliseconds 400 }
  [W32]::keybd_event($VK_LWIN, 0, 0, [UIntPtr]::Zero)
  [W32]::keybd_event($VK_LEFT, 0, 0, [UIntPtr]::Zero)
  [W32]::keybd_event($VK_LEFT, 0, $KEYEVENTF_KEYUP, [UIntPtr]::Zero)
  [W32]::keybd_event($VK_LWIN, 0, $KEYEVENTF_KEYUP, [UIntPtr]::Zero)
  Start-Sleep -Milliseconds 1200
  $snap = New-Object W32+RECT; [void][W32]::GetWindowRect($main, [ref]$snap)
  $scr = New-Object W32+RECT; [void][W32]::GetClientRect($main, [ref]$scr)
  $cw = $scr.Right - $scr.Left; $ch = $scr.Bottom - $scr.Top
  # 无边框窗口的 window rect 含不可见缩放边框（150% 下每边约 11px），所以比客户区
  Check 'Win+← 贴到左半屏' ([math]::Abs($cw - $workW / 2) -le 16 -and [math]::Abs($snap.Left - $work.Left) -le 16 -and [math]::Abs($ch - $workH) -le 20) `
    ("window ({0},{1}) -> 客户区 {2}x{3} vs 半屏 {4}x{5}" -f $snap.Left, $snap.Top, $cw, $ch, [math]::Floor($workW / 2), $workH)
  Check '贴靠后不算「最大化」' (-not [W32]::IsZoomed($main)) 'IsZoomed == false'
}
catch {
  Write-Host ("ERROR " + $_.Exception.Message + " @ line " + $_.InvocationInfo.ScriptLineNumber)
  $fail++
}
finally {
  try { [void][W32]::SetCursorPos($oldCursor.X, $oldCursor.Y) } catch { }
  try { [void][W32]::SetWindowPos($main, $HWND_NOTOPMOST, 0, 0, 0, 0, $SWP_NOMOVE -bor $SWP_NOSIZE -bor $SWP_NOACTIVATE) } catch { }
  Get-Process -Id $procId -ErrorAction SilentlyContinue | Stop-Process -Force
  Write-Host ("RESULT fail={0}" -f $fail)
  Write-Host ("INFO  截图 -> " + $Out)
}
if ($fail -gt 0) { exit 1 }
