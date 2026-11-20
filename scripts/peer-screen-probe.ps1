# Two-instance probe: remote desktop view-only.
# A views B's screen: consent -> frames on canvas -> quality/monitor list -> B kicks -> revoke denies.
# Requires: npm run dev + cargo build (debug exe), as in peer-e2e.ps1.
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = New-Object Text.UTF8Encoding($false)
$script:fail = 0
$script:total = 0
function Check([string]$name, [bool]$ok, [string]$detail) {
  $script:total++
  if ($ok) { Write-Host ("PASS  " + $name + "  " + $detail) }
  else { Write-Host ("FAIL  " + $name + "  " + $detail); $script:fail++ }
}

$repo = Split-Path -Parent $PSScriptRoot
$exe = if ($env:RUTHENIUM_PEER_EXE) { $env:RUTHENIUM_PEER_EXE } else { Join-Path $repo 'src-tauri\target\debug\Ruthenium.exe' }
$dirA = Join-Path $env:TEMP 'ruthenium-screenA'
$dirB = Join-Path $env:TEMP 'ruthenium-screenB'
Get-Process -Name Ruthenium -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 600
Remove-Item $dirA, $dirB, "$dirA-webview", "$dirB-webview" -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $dirA, $dirB | Out-Null

# 显示器休眠时 DXGI DuplicateOutput 返回 ACCESS_DENIED（采集不可用）：探针期间保持唤醒
Add-Type -TypeDefinition @'
using System; using System.Runtime.InteropServices;
public class DisplayWake {
  [DllImport("kernel32.dll")] public static extern uint SetThreadExecutionState(uint f);
  [DllImport("user32.dll")] public static extern void keybd_event(byte vk, byte scan, uint flags, UIntPtr extra);
}
'@
[DisplayWake]::SetThreadExecutionState([uint32]2147483651) | Out-Null
[DisplayWake]::keybd_event(0x7E, 0, 0, [UIntPtr]::Zero)
Start-Sleep -Milliseconds 200
[DisplayWake]::keybd_event(0x7E, 0, 2, [UIntPtr]::Zero)
Start-Sleep -Seconds 2

$script:cdp = 1000
function Eval($ws, [string]$expr) {
  $script:cdp++
  $id = $script:cdp
  $msg = @{ id = $id; method = 'Runtime.evaluate'; params = @{ expression = $expr; returnByValue = $true; awaitPromise = $true } } | ConvertTo-Json -Depth 8 -Compress
  $bytes = [Text.Encoding]::UTF8.GetBytes($msg)
  $ws.SendAsync((New-Object 'System.ArraySegment[byte]' -ArgumentList @(,$bytes)), [System.Net.WebSockets.WebSocketMessageType]::Text, $true, [Threading.CancellationToken]::None).Wait()
  while ($true) {
    $buf = New-Object byte[] 524288
    $res = $ws.ReceiveAsync((New-Object 'System.ArraySegment[byte]' -ArgumentList @(,$buf)), [Threading.CancellationToken]::None).Result
    $obj = ([Text.Encoding]::UTF8.GetString($buf, 0, $res.Count)) | ConvertFrom-Json
    if ($obj.id -eq $id) { return $obj.result.result.value }
  }
}
function Start-App([string]$dataDir, [int]$cdpPort, [int]$pairPort) {
  $env:RUTHENIUM_DATA_DIR = $dataDir
  $env:RUTHENIUM_PAIR_PORT = "$pairPort"
  $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS = "--remote-debugging-port=$cdpPort"
  $env:WEBVIEW2_USER_DATA_FOLDER = "$dataDir-webview"
  $p = Start-Process -FilePath $exe -PassThru
  $targets = $null
  for ($i = 0; $i -lt 120; $i++) {
    try { $targets = Invoke-RestMethod "http://127.0.0.1:$cdpPort/json"; if ($targets.Count -gt 0) { break } } catch { }
    Start-Sleep -Milliseconds 500
  }
  $page = $targets | Where-Object { $_.type -eq 'page' } | Select-Object -First 1
  if (-not $page) {
    Get-Process -Name Ruthenium -ErrorAction SilentlyContinue | Stop-Process -Force
    throw "CDP $cdpPort not ready"
  }
  $ws = New-Object System.Net.WebSockets.ClientWebSocket
  $ws.ConnectAsync([Uri]$page.webSocketDebuggerUrl, [Threading.CancellationToken]::None).Wait()
  return @{ ws = $ws; pid = $p.Id }
}
function Ready($ws) {
  for ($i = 0; $i -lt 80; $i++) { try { if (Eval $ws '!!document.querySelector(".titlebar")') { return $true } } catch { }; Start-Sleep -Milliseconds 300 }
  return $false
}
function Open-Peers($ws) {
  Eval $ws '(() => { const b = [...document.querySelectorAll(".seg button")].find(x => x.textContent.includes("\u8bbe\u5907")); b && b.click(); return !!b; })()' | Out-Null
  Start-Sleep -Milliseconds 900
}

$A = Start-App $dirA 9444 28410
$B = Start-App $dirB 9446 28411
Check 'A ready' (Ready $A.ws) ''
Check 'B ready' (Ready $B.ws) ''
Open-Peers $A.ws
Open-Peers $B.ws
Start-Sleep -Seconds 3

$typed = Eval $A.ws '(() => { const i = document.querySelector("[data-peer-addr]"); if (!i) return false; const set = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value").set; set.call(i, "127.0.0.1:28411"); i.dispatchEvent(new Event("input", { bubbles: true })); return true; })()'
Check 'A can enter pair address' ($typed -eq $true) ''
Eval $A.ws '(() => { const b = [...document.querySelectorAll(".peerspanel .tbtn")].find(x => x.textContent.includes("\u8fde\u63a5\u914d\u5bf9")); b && b.click(); return !!b; })()' | Out-Null
Start-Sleep -Seconds 2
$codeA = Eval $A.ws 'document.querySelector("[data-pair-code]")?.textContent || ""'
$codeB = Eval $B.ws 'document.querySelector("[data-pair-code]")?.textContent || ""'
Check 'same 6-digit code' ($codeA -match '^\d{6}$' -and $codeA -eq $codeB) "A=$codeA B=$codeB"
foreach ($k in @('files', 'control', 'config', 'screen')) {
  $expr = '(() => { const c = document.querySelector(''.modal [data-grant="' + $k + '"]''); if (!c) return false; c.click(); return c.checked; })()'
  Eval $B.ws $expr | Out-Null
  Start-Sleep -Milliseconds 100
}
Eval $B.ws '(() => { const b = document.querySelector("[data-pair-agree]"); b && b.click(); return !!b; })()' | Out-Null
Start-Sleep -Milliseconds 400
Eval $A.ws '(() => { const b = document.querySelector("[data-pair-agree]"); b && b.click(); return !!b; })()' | Out-Null
Start-Sleep -Seconds 3
$peerId = Eval $A.ws 'document.querySelector(".peerrow[data-peer-id]")?.getAttribute("data-peer-id") || ""'
Check 'paired' ($peerId -ne '') "peer=$peerId"

# Open screen
Eval $A.ws '(() => { const b = document.querySelector(".peerrow[data-peer-id] .peer-screen"); b && b.click(); return !!b; })()' | Out-Null
Start-Sleep -Seconds 2
Check 'B asks to view screen' ((Eval $B.ws '!!document.querySelector("[data-screen-allow]")') -eq $true) ''
Eval $B.ws '(() => { const b = document.querySelector("[data-screen-allow]"); if (b) b.click(); return !!b; })()' | Out-Null
Start-Sleep -Seconds 2

$sid = Eval $A.ws 'document.querySelector(".remoteview")?.getAttribute("data-screen-session") || ""'
Check 'A opened screen view' ($sid -ne '') "sid=$sid"
Check 'B shows viewed banner' ((Eval $B.ws 'document.querySelector("[data-viewed-id]") ? "y" : ""') -eq 'y') ''

# Wait for frames
$frames = 0
$size = ''
for ($i = 0; $i -lt 40; $i++) {
  $frames = [int](Eval $A.ws 'Number(document.querySelector(".remoteview")?.getAttribute("data-screen-frames") || 0)')
  $size = Eval $A.ws 'document.querySelector(".remoteview")?.getAttribute("data-screen-size") || ""'
  if ($frames -ge 1 -and $size -match '\d+x\d+') { break }
  Start-Sleep -Milliseconds 300
}
Check 'screen frames arrived' ($frames -ge 1 -and $size -match '^\d+x\d+$') "frames=$frames size=$size"

$pixels = Eval $A.ws '(() => { const c = document.querySelector(".rv-canvas"); if (!c || !c.width) return "no"; const ctx = c.getContext("2d"); const d = ctx.getImageData(Math.floor(c.width/2), Math.floor(c.height/2), 1, 1).data; return d[3] + ":" + (d[0]+d[1]+d[2]); })()'
Check 'canvas painted' ($pixels -ne 'no' -and $pixels -match '^255:') "px=$pixels"

$monitors = Eval $A.ws 'document.querySelectorAll("[data-screen-monitor] option").length'
Check 'monitor list populated' ([int]$monitors -ge 1) "count=$monitors"

# H.264（WebCodecs）协商 + 自适应统计
$codec = ''
for ($i = 0; $i -lt 40; $i++) {
  $codec = Eval $A.ws 'document.querySelector(".remoteview")?.getAttribute("data-screen-codec") || ""'
  if ($codec -eq 'h264') { break }
  Start-Sleep -Milliseconds 300
}
Check 'H.264 negotiated (WebCodecs)' ($codec -eq 'h264') "codec=$codec"
$statsText = Eval $A.ws 'document.querySelector("[data-screen-stats]")?.textContent || ""'
Check 'stats show codec/fps/bitrate' ($statsText -match 'H264' -and $statsText -match '\d+fps') "stats=$statsText"
Check 'keyframe resync control present' ((Eval $A.ws '!!document.querySelector("[data-screen-key]")') -eq $true) ''
Eval $A.ws '(() => { const b = document.querySelector("[data-screen-key]"); b && b.click(); return !!b; })()' | Out-Null
Start-Sleep -Milliseconds 600
$codecAfterKey = Eval $A.ws 'document.querySelector(".remoteview")?.getAttribute("data-screen-codec") || ""'
Check 'resync keeps stream alive' ($codecAfterKey -eq 'h264') "codec=$codecAfterKey"

# Idle bandwidth: frames should not keep growing at video rate (static desktop => few/none)
$fpsNow = [int](Eval $A.ws 'Number(document.querySelector(".remoteview")?.getAttribute("data-screen-fps") || 0)')
$f1 = [int](Eval $A.ws 'Number(document.querySelector(".remoteview")?.getAttribute("data-screen-frames") || 0)')
Start-Sleep -Seconds 3
$f2 = [int](Eval $A.ws 'Number(document.querySelector(".remoteview")?.getAttribute("data-screen-frames") || 0)')
$cap = [Math]::Max(10, $fpsNow) * 3 + 15
Check 'frame rate within adaptive cap' (($f2 - $f1) -le $cap) "fps=$fpsNow before=$f1 after=$f2 cap=$cap"

# Quality switch + per-device memory
Eval $A.ws '(() => { const s = document.querySelector("[data-screen-quality]"); if (!s) return false; s.value = "45"; s.dispatchEvent(new Event("change", { bubbles: true })); return true; })()' | Out-Null
Start-Sleep -Milliseconds 500
Check 'quality switch accepted' ((Eval $A.ws 'document.querySelector("[data-screen-quality]")?.value') -eq '45') ''
$savedQ = Eval $A.ws '(() => { const k = Object.keys(localStorage).find((x) => x.startsWith("ruthenium.rd.quality.")); return k ? localStorage.getItem(k) : ""; })()'
Check 'quality remembered per device' ($savedQ -eq '45') "q=$savedQ"

# 回退 JPEG（无 WebCodecs / 解码失败路径）后仍能出画面
Eval $A.ws "window.__TAURI_INTERNALS__.invoke('peer_screen_command', { sessionId: '$sid', cmd: { cmd: 'codec', codec: 'jpeg' } })" | Out-Null
$codecJpeg = ''
for ($i = 0; $i -lt 30; $i++) {
  $codecJpeg = Eval $A.ws 'document.querySelector(".remoteview")?.getAttribute("data-screen-codec") || ""'
  if ($codecJpeg -eq 'jpeg') { break }
  Start-Sleep -Milliseconds 300
}
Check 'can fall back to JPEG' ($codecJpeg -eq 'jpeg') "codec=$codecJpeg"
Eval $A.ws "window.__TAURI_INTERNALS__.invoke('peer_screen_command', { sessionId: '$sid', cmd: { cmd: 'codec', codec: 'h264' } })" | Out-Null
$codecBack = ''
for ($i = 0; $i -lt 30; $i++) {
  $codecBack = Eval $A.ws 'document.querySelector(".remoteview")?.getAttribute("data-screen-codec") || ""'
  if ($codecBack -eq 'h264') { break }
  Start-Sleep -Milliseconds 300
}
Check 'can switch back to H.264' ($codecBack -eq 'h264') "codec=$codecBack"

# 记录当前流的基准 fps/宽度：显示器原生宽度可能是 1920/2560 等，
# 自适应恢复应回到本机基准，不能硬编码 2560（否则 1080p 机器永远失败）。
$baseFps = [int](Eval $A.ws 'Number(document.querySelector(".remoteview")?.getAttribute("data-screen-fps") || 0)')
$baseWidth = [int](Eval $A.ws 'Number(document.querySelector(".remoteview")?.getAttribute("data-screen-stream-width") || 0)')
if ($baseFps -le 0) { $baseFps = 30 }
if ($baseWidth -le 0 -and $size -match '^\d+x\d+$') { $baseWidth = [int]$size.Split('x')[0] }
Write-Host "INFO  adaptive baseline fps=$baseFps width=$baseWidth"

# 自适应：模拟高 RTT（查看端每 2s 上报 stats，探针在这层加 500ms）
Eval $A.ws '(() => { const w = window.__TAURI_INTERNALS__; if (!window.__origInvoke) window.__origInvoke = w.invoke; w.invoke = function (cmd, args, ...rest) { if (cmd === "peer_screen_command" && args && args.cmd && args.cmd.cmd === "stats") { args.cmd.rttMs = Number(args.cmd.rttMs || 0) + 500; } return window.__origInvoke.call(this, cmd, args, ...rest); }; return true; })()' | Out-Null
$degraded = $false
for ($i = 0; $i -lt 40; $i++) {
  Eval $A.ws "window.__TAURI_INTERNALS__.invoke('peer_screen_command', { sessionId: '$sid', cmd: { cmd: 'stats', rttMs: 500 } })" | Out-Null
  $fpsNow = [int](Eval $A.ws 'Number(document.querySelector(".remoteview")?.getAttribute("data-screen-fps") || 0)')
  $wNow = [int](Eval $A.ws 'Number(document.querySelector(".remoteview")?.getAttribute("data-screen-stream-width") || 0)')
  if (($fpsNow -gt 0 -and $fpsNow -lt $baseFps) -or ($wNow -gt 0 -and $baseWidth -gt 0 -and $wNow -lt $baseWidth)) { $degraded = $true; break }
  Start-Sleep -Milliseconds 500
}
Check 'adaptive degrades on high RTT' $degraded "fps=$fpsNow/$baseFps width=$wNow/$baseWidth"
Eval $A.ws '(() => { if (window.__origInvoke) window.__TAURI_INTERNALS__.invoke = window.__origInvoke; return true; })()' | Out-Null
$recovered = $false
for ($i = 0; $i -lt 40; $i++) {
  Eval $A.ws "window.__TAURI_INTERNALS__.invoke('peer_screen_command', { sessionId: '$sid', cmd: { cmd: 'stats', rttMs: 20 } })" | Out-Null
  $fpsNow = [int](Eval $A.ws 'Number(document.querySelector(".remoteview")?.getAttribute("data-screen-fps") || 0)')
  $wNow = [int](Eval $A.ws 'Number(document.querySelector(".remoteview")?.getAttribute("data-screen-stream-width") || 0)')
  if ($fpsNow -eq $baseFps -and ($baseWidth -le 0 -or $wNow -eq $baseWidth)) { $recovered = $true; break }
  Start-Sleep -Milliseconds 500
}
Check 'adaptive recovers on low RTT' $recovered "fps=$fpsNow/$baseFps width=$wNow/$baseWidth"

# 光标形状：移动本机光标 → 查看端收到形状并绘制到叠加层
Add-Type -AssemblyName System.Windows.Forms
$scr = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
$pts = @(
  @([int]($scr.Left + $scr.Width * 0.72), [int]($scr.Top + $scr.Height * 0.72)),
  @([int]($scr.Left + $scr.Width * 0.5), [int]($scr.Top + $scr.Height * 0.85)),
  @([int]($scr.Left + $scr.Width * 0.85), [int]($scr.Top + $scr.Height * 0.45)),
  @([int]($scr.Left + $scr.Width * 0.35), [int]($scr.Top + $scr.Height * 0.55)),
  @([int]($scr.Left + $scr.Width * 0.9), [int]($scr.Top + $scr.Height * 0.9))
)
$cursorOk = '0'
$shapeOk = '0'
for ($i = 0; $i -lt 60; $i++) {
  $p = $pts[$i % $pts.Count]
  [System.Windows.Forms.Cursor]::Position = [System.Drawing.Point]::new($p[0], $p[1])
  $cursorOk = Eval $A.ws 'document.querySelector(".remoteview")?.getAttribute("data-screen-cursor") || "0"'
  $shapeOk = Eval $A.ws 'document.querySelector(".remoteview")?.getAttribute("data-screen-cursor-shape") || "0"'
  if ($cursorOk -eq '1' -and $shapeOk -eq '1') { break }
  Start-Sleep -Milliseconds 200
}
# DXGI 在部分驱动上把光标直接合成进画面（PointerPosition.Visible=false），
# 此时查看端不画叠加层才是正确的：shape 已收到即视为通过。
$cursorComposited = ($cursorOk -eq '0' -and $shapeOk -eq '1')
Check 'cursor shape received' ($shapeOk -eq '1' -and ($cursorOk -eq '1' -or $cursorComposited)) "visible=$cursorOk shape=$shapeOk"
$cursorAlpha = 0
if (-not $cursorComposited) {
  for ($i = 0; $i -lt 25; $i++) {
    $cursorAlpha = [int](Eval $A.ws '(() => { const c = document.querySelector("[data-cursor-layer]"); if (!c || !c.width) return 0; const d = c.getContext("2d").getImageData(0, 0, c.width, c.height).data; let s = 0; for (let i = 3; i < d.length; i += 16) s += d[i]; return s; })()')
    if ($cursorAlpha -gt 0) { break }
    Start-Sleep -Milliseconds 200
  }
} else {
  Write-Host 'INFO  cursor composited into frames (DXGI reports invisible); skip overlay check'
}
Check 'cursor overlay painted' ($cursorComposited -or $cursorAlpha -gt 0) "alphaSum=$cursorAlpha"

# 指定窗口：启动一个 WinForms 窗口，切换到窗口采集
$helper = Join-Path $repo 'scripts\peer-input-helper.ps1'
$textFile = Join-Path $env:TEMP 'ruthenium-window-text.txt'
$posFile = Join-Path $env:TEMP 'ruthenium-window-pos.txt'
$winProc = Start-Process -FilePath 'powershell.exe' -ArgumentList @('-NoProfile', '-STA', '-ExecutionPolicy', 'Bypass', '-File', $helper, $textFile, $posFile) -PassThru
Start-Sleep -Seconds 3
Eval $A.ws '(() => { const b = document.querySelector("[data-screen-window-toggle]"); b && b.click(); return !!b; })()' | Out-Null
$optionText = ''
for ($i = 0; $i -lt 30; $i++) {
  $optionText = Eval $A.ws '[...document.querySelectorAll("[data-screen-window] option")].map((o) => o.textContent).join("|")'
  if ($optionText -like '*Ruthenium Input Probe*') { break }
  Start-Sleep -Milliseconds 300
}
Check 'window list populated' ($optionText -like '*Ruthenium Input Probe*') "opts=$optionText"
$picked = Eval $A.ws '(() => { const s = document.querySelector("[data-screen-window]"); if (!s) return false; const o = [...s.options].find((x) => x.textContent.includes("Ruthenium Input Probe")); if (!o) return false; s.value = o.value; s.dispatchEvent(new Event("change", { bubbles: true })); return true; })()'
Check 'can pick a window' ($picked -eq $true) ''
$winActive = $false
for ($i = 0; $i -lt 40; $i++) {
  $src = Eval $A.ws 'document.querySelector(".remoteview")?.getAttribute("data-screen-source") || ""'
  $wf = [int](Eval $A.ws 'Number(document.querySelector(".remoteview")?.getAttribute("data-screen-frames") || 0)')
  if ($src -eq 'window' -and $wf -ge 1) { $winActive = $true; break }
  Start-Sleep -Milliseconds 300
}
Check 'window share active' $winActive "source=$src frames=$wf"
Check 'window hint shown' ((Eval $A.ws '!!document.querySelector("[data-screen-window-hint]")') -eq $true) ''
Check 'cursor hidden for window share' ((Eval $A.ws 'document.querySelector(".remoteview")?.getAttribute("data-screen-cursor")') -eq '0') ''
$winPixels = Eval $A.ws '(() => { const c = document.querySelector(".rv-canvas"); if (!c || !c.width) return ""; const d = c.getContext("2d").getImageData(Math.floor(c.width/2), Math.floor(c.height/2), 1, 1).data; return d[3] + ":" + (d[0]+d[1]+d[2]); })()'
Check 'window canvas painted' ($winPixels -match '^255:') "px=$winPixels"
Eval $A.ws '(() => { const s = document.querySelector("[data-screen-window]"); if (!s) return false; s.value = "0"; s.dispatchEvent(new Event("change", { bubbles: true })); return true; })()' | Out-Null
$backDesk = $false
for ($i = 0; $i -lt 30; $i++) {
  $src = Eval $A.ws 'document.querySelector(".remoteview")?.getAttribute("data-screen-source") || ""'
  if ($src -eq 'desktop') { $backDesk = $true; break }
  Start-Sleep -Milliseconds 300
}
Check 'can switch back to desktop' $backDesk "source=$src"
if (-not $winProc.HasExited) { Stop-Process -Id $winProc.Id -Force -ErrorAction SilentlyContinue }

# Kick
Eval $B.ws '(() => { const b = document.querySelector("[data-screen-kick]"); if (b) b.click(); return !!b; })()' | Out-Null
Start-Sleep -Seconds 3
$errText = Eval $A.ws 'document.querySelector(".rv-error")?.textContent || ""'
Check 'kick shows disconnect on viewer' ($errText -ne '') "err=$errText"
Check 'kick removes viewed banner' ((Eval $B.ws 'document.querySelector("[data-viewed-id]") ? "y" : ""') -eq '') ''

# 会话进行中撤销屏幕授权：服务端 5s 内复核并主动结束流
$peerIdB = Eval $B.ws 'document.querySelector(".peerrow[data-peer-id]")?.getAttribute("data-peer-id") || ""'
# 先把被踢掉的旧标签关掉（否则 store 会复用旧会话，不会新建连接）
Eval $A.ws '(() => { const t = [...document.querySelectorAll(".tabbar .tab")].find(x => x.textContent.includes("\u5c4f\u5e55")); const x = t && t.querySelector(".tx"); x && x.click(); return !!x; })()' | Out-Null
Start-Sleep -Milliseconds 500
Eval $B.ws "window.__TAURI_INTERNALS__.invoke('peer_update_grants', { peerId: '$peerIdB', grantFiles: true, grantControl: true, grantConfig: true, grantScreen: true, grantClipboard: false, grantAudio: false })" | Out-Null
Start-Sleep -Milliseconds 600
Eval $A.ws '(() => { const b = document.querySelector(".peerrow[data-peer-id] .peer-screen"); b && b.click(); return !!b; })()' | Out-Null
$newPrompt = $false
for ($i = 0; $i -lt 40; $i++) {
  if ((Eval $B.ws '!!document.querySelector("[data-screen-allow]")') -eq $true) {
    Eval $B.ws '(() => { const b = document.querySelector("[data-screen-allow]"); if (b) b.click(); return !!b; })()' | Out-Null
    $newPrompt = $true
    break
  }
  Start-Sleep -Milliseconds 300
}
Check 'reopen asks for consent again' $newPrompt ''
$reFrames = 0
for ($i = 0; $i -lt 40; $i++) {
  $reFrames = [int](Eval $A.ws 'Number(document.querySelector(".remoteview")?.getAttribute("data-screen-frames") || 0)')
  if ($reFrames -ge 1) { break }
  Start-Sleep -Milliseconds 300
}
Eval $B.ws "window.__TAURI_INTERNALS__.invoke('peer_update_grants', { peerId: '$peerIdB', grantFiles: true, grantControl: true, grantConfig: true, grantScreen: false, grantClipboard: false, grantAudio: false })" | Out-Null
$revokeErr = ''
for ($i = 0; $i -lt 40; $i++) {
  $revokeErr = Eval $A.ws 'document.querySelector(".rv-error")?.textContent || ""'
  if ($revokeErr -match '\u64a4\u9500') { break }
  Start-Sleep -Milliseconds 300
}
Check 'revoke during session ends stream' ($reFrames -ge 1 -and $revokeErr -match '\u64a4\u9500') "frames=$reFrames err=$revokeErr"

# Close the viewer tab, revoke screen grant, reopen must fail
Eval $A.ws '(() => { const t = [...document.querySelectorAll(".tabbar .tab")].find(x => x.textContent.includes("\u5c4f\u5e55")); const x = t && t.querySelector(".tx"); x && x.click(); return !!x; })()' | Out-Null
Start-Sleep -Milliseconds 500
Eval $B.ws "window.__TAURI_INTERNALS__.invoke('peer_update_grants', { peerId: '$peerIdB', grantFiles: true, grantControl: true, grantConfig: true, grantScreen: false, grantClipboard: false, grantAudio: false })" | Out-Null
Start-Sleep -Milliseconds 600
Eval $A.ws '(() => { const b = document.querySelector(".peerrow[data-peer-id] .peer-screen"); b && b.click(); return !!b; })()' | Out-Null
$toastA = ''
for ($i = 0; $i -lt 40; $i++) {
  $tt = Eval $A.ws 'document.querySelector(".toast")?.textContent || ""'
  if ($tt -ne '') { $toastA = $tt; break }
  Start-Sleep -Milliseconds 150
}
$tabsLeft = Eval $A.ws '[...document.querySelectorAll(".tabbar .tab .tn")].some(t => t.textContent.includes("\u5c4f\u5e55"))'
Check 'screen denied after revoke' ($tabsLeft -eq $false -and $toastA -ne '') "tabs=$tabsLeft toast=$toastA"

Get-Process -Name Ruthenium -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 800
Remove-Item Env:\RUTHENIUM_DATA_DIR, Env:\RUTHENIUM_PAIR_PORT, Env:\WEBVIEW2_USER_DATA_FOLDER -ErrorAction SilentlyContinue
Write-Host ("RESULT {0}/{1} passed, {2} failed" -f ($script:total - $script:fail), $script:total, $script:fail)
if ($script:fail -gt 0) { exit 1 }
