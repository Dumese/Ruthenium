# Two-instance probe: remote desktop input control.
# A views B's screen with "allow input" checked; verifies mouse move + Unicode text injection,
# then read-only enforcement (no checkbox -> server rejects the input subsystem).
# Requires: npm run dev + cargo build (debug exe).
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
$helper = Join-Path $repo 'scripts\peer-input-helper.ps1'
$dirA = Join-Path $env:TEMP 'ruthenium-inputA'
$dirB = Join-Path $env:TEMP 'ruthenium-inputB'
$textFile = Join-Path $env:TEMP 'ruthenium-input-text.txt'
$posFile = Join-Path $env:TEMP 'ruthenium-input-pos.txt'
Get-Process -Name Ruthenium -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 600
Remove-Item $dirA, $dirB, "$dirA-webview", "$dirB-webview", $textFile, $posFile -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $dirA, $dirB | Out-Null

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

# Keep the display awake for the whole probe: DXGI Desktop Duplication fails with
# ACCESS_DENIED while the monitor is powered off (screen capture is then unavailable).
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

$A = Start-App $dirA 9444 28410
$B = Start-App $dirB 9446 28411
Check 'A ready' (Ready $A.ws) ''
Check 'B ready' (Ready $B.ws) ''
Open-Peers $A.ws
Open-Peers $B.ws
Start-Sleep -Seconds 3

# pairing (all four grants)
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

# WinForms helper on B (focused textbox + cursor logger)
$helperProc = Start-Process -FilePath 'powershell.exe' -ArgumentList @('-NoProfile', '-STA', '-ExecutionPolicy', 'Bypass', '-File', $helper, $textFile, $posFile) -PassThru
Start-Sleep -Seconds 3
Check 'input helper running' ((Test-Path $posFile) -and -not $helperProc.HasExited) ''

# open screen with input allowed（入口：查看并操作 → 确认框才展示「允许操作」）
Eval $A.ws '(() => { const b = document.querySelector(".peerrow[data-peer-id] .peer-screen-control"); b && b.click(); return !!b; })()' | Out-Null
Start-Sleep -Seconds 2
Check 'B asks to view screen' ((Eval $B.ws '!!document.querySelector("[data-screen-allow]")') -eq $true) ''
$allowChk = Eval $B.ws '(() => { const c = document.querySelector("[data-screen-allow-input]"); if (!c) return "missing"; c.click(); return String(c.checked); })()'
Start-Sleep -Milliseconds 400
$allowState = Eval $B.ws 'document.querySelector("[data-screen-allow-input]")?.getAttribute("data-allow-state") || "?"'
Write-Host ("INFO  allow-input checkbox -> " + $allowChk + " state=" + $allowState)
Eval $B.ws '(() => { const b = document.querySelector("[data-screen-allow]"); if (b) b.click(); return !!b; })()' | Out-Null

# wait for frames + auth (control toggle)
$controlReady = $false
for ($i = 0; $i -lt 40; $i++) {
  if ((Eval $A.ws '!!document.querySelector("[data-screen-control]")') -eq $true) { $controlReady = $true; break }
  Start-Sleep -Milliseconds 300
}
Check 'viewer offers control after auth' $controlReady ''

Eval $A.ws '(() => { const b = document.querySelector("[data-screen-control]"); if (b) b.click(); return !!b; })()' | Out-Null
Start-Sleep -Milliseconds 1200
$ctlText = Eval $A.ws 'document.querySelector("[data-screen-control]")?.textContent || ""'
$ctlToast = Eval $A.ws 'document.querySelector(".toast")?.textContent || ""'
Write-Host ("INFO  control button=" + $ctlText + " toast=" + $ctlToast)

# Pointer Lock 会让合成 mousemove 的 movementX=0；先退出锁定，用绝对坐标验证
Eval $A.ws 'document.pointerLockElement && document.exitPointerLock(); true' | Out-Null
Start-Sleep -Milliseconds 300

# 缩放切换（适应 → 原始尺寸 → 适应）
Eval $A.ws '(() => { const b = document.querySelector("[data-screen-zoom]"); b && b.click(); return true; })()' | Out-Null
Start-Sleep -Milliseconds 300
$zoom1 = Eval $A.ws 'document.querySelector(".rv-canvas")?.classList.contains("original") ?? false'
Eval $A.ws '(() => { const b = document.querySelector("[data-screen-zoom]"); b && b.click(); return true; })()' | Out-Null
Start-Sleep -Milliseconds 300
$zoom2 = Eval $A.ws 'document.querySelector(".rv-canvas")?.classList.contains("original") ?? true'
Check 'zoom toggle works' ($zoom1 -eq $true -and $zoom2 -eq $false) "original=$zoom1 then=$zoom2"
Check 'fullscreen button present' ((Eval $A.ws '!!document.querySelector("[data-screen-fullscreen]")') -eq $true) ''

# 输入延迟（ping/pong，每 2s 一次）
$rttText = ''
for ($i = 0; $i -lt 30; $i++) {
  $t = Eval $A.ws 'document.querySelector("[data-screen-stats]")?.textContent || ""'
  if ($t -match 'RTT \d+ms') { $rttText = $t; break }
  Start-Sleep -Milliseconds 300
}
Check 'input latency reported' ($rttText -match 'RTT \d+ms') "stats=$rttText"

# mouse move to 25% / 25% of the canvas
$moved = Eval $A.ws '(() => {
  const c = document.querySelector(".rv-canvas");
  if (!c) return false;
  const r = c.getBoundingClientRect();
  c.dispatchEvent(new MouseEvent("mousemove", { bubbles: true, clientX: r.left + r.width * 0.25, clientY: r.top + r.height * 0.25 }));
  return true;
})()'
Check 'mousemove dispatched to viewer' ($moved -eq $true) ''
Start-Sleep -Milliseconds 800
Add-Type -AssemblyName System.Windows.Forms
$scr = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
$expX = [int]($scr.Left + $scr.Width * 0.25)
$expY = [int]($scr.Top + $scr.Height * 0.25)
$pos = if (Test-Path $posFile) { [IO.File]::ReadAllText($posFile) } else { '' }
$px = 0; $py = 0
if ($pos -match '^(-?\d+),(-?\d+)$') { $px = [int]$Matches[1]; $py = [int]$Matches[2] }
$near = ([Math]::Abs($px - $expX) -le 80) -and ([Math]::Abs($py - $expY) -le 80)
Check 'mouse move injected on peer' $near "got=$px,$py expected=$expX,$expY"

# keyboard: unicode text + VK letter
Eval $A.ws 'window.dispatchEvent(new KeyboardEvent("keydown", { key: "\u6f22", code: "Unidentified", bubbles: true }))' | Out-Null
Eval $A.ws 'window.dispatchEvent(new KeyboardEvent("keyup", { key: "\u6f22", code: "Unidentified", bubbles: true }))' | Out-Null
Eval $A.ws 'window.dispatchEvent(new KeyboardEvent("keydown", { key: "a", code: "KeyA", bubbles: true }))' | Out-Null
Eval $A.ws 'window.dispatchEvent(new KeyboardEvent("keyup", { key: "a", code: "KeyA", bubbles: true }))' | Out-Null
Start-Sleep -Milliseconds 900
$typedText = if (Test-Path $textFile) { [IO.File]::ReadAllText($textFile) } else { '' }
Check 'unicode + key text injected' ($typedText -match '漢' -and $typedText -match 'a') "text=[$typedText]"

# kick
Eval $B.ws '(() => { const b = document.querySelector("[data-screen-kick]"); if (b) b.click(); return !!b; })()' | Out-Null
Start-Sleep -Seconds 3
$errText = Eval $A.ws 'document.querySelector(".rv-error")?.textContent || ""'
Check 'kick disconnects viewer' ($errText -ne '') "err=$errText"

# close tab, reopen WITHOUT input permission -> read-only
Eval $A.ws '(() => { const t = [...document.querySelectorAll(".tabbar .tab")].find(x => x.textContent.includes("\u5c4f\u5e55")); const x = t && t.querySelector(".tx"); x && x.click(); return !!x; })()' | Out-Null
Start-Sleep -Milliseconds 600
Eval $A.ws '(() => { const b = document.querySelector(".peerrow[data-peer-id] .peer-screen"); b && b.click(); return !!b; })()' | Out-Null
Start-Sleep -Seconds 2
Eval $B.ws '(() => { const b = document.querySelector("[data-screen-allow]"); if (b) b.click(); return !!b; })()' | Out-Null
$readonly = $false
for ($i = 0; $i -lt 30; $i++) {
  if ((Eval $A.ws '!!document.querySelector("[data-screen-frames]")') -eq $true) {
    if ((Eval $A.ws 'document.querySelector("[data-screen-control]") ? "y" : ""') -eq '') { $readonly = $true; break }
  }
  Start-Sleep -Milliseconds 300
}
Check 'view-only session has no control toggle' $readonly ''
$sid2 = Eval $A.ws 'document.querySelector(".remoteview")?.getAttribute("data-screen-session") || ""'
$roRes = Eval $A.ws "window.__TAURI_INTERNALS__.invoke('peer_input_start', { screenSessionId: '$sid2' }).then(() => 'ok').catch(e => 'err:' + JSON.stringify(e))"
Check 'server rejects input for view-only session' ($roRes -like 'err:*') "res=$roRes"

# 会话中申请操作权限：对方允许后出现操作按钮，输入通道可用
Start-Sleep -Seconds 5  # 等过确认框频控窗口
Eval $A.ws '(() => { const b = document.querySelector("[data-screen-request-input]"); b && b.click(); return !!b; })()' | Out-Null
$upPrompt = $false
for ($i = 0; $i -lt 40; $i++) {
  if ((Eval $B.ws '!!document.querySelector("[data-screen-allow]")') -eq $true) {
    Eval $B.ws '(() => { const b = document.querySelector("[data-screen-allow]"); if (b) b.click(); return !!b; })()' | Out-Null
    $upPrompt = $true
    break
  }
  Start-Sleep -Milliseconds 300
}
$upgraded = $false
for ($i = 0; $i -lt 40; $i++) {
  if ((Eval $A.ws 'document.querySelector("[data-screen-control]") ? "y" : ""') -eq 'y') { $upgraded = $true; break }
  Start-Sleep -Milliseconds 300
}
Check 'can request control mid-session' ($upPrompt -and $upgraded) "prompt=$upPrompt button=$upgraded"
# 通过 UI 开启操作（会带上 auth token；直接调命令需要服务端会话 id，测试里不暴露）
Eval $A.ws '(() => { const b = document.querySelector("[data-screen-control]"); b && b.click(); return !!b; })()' | Out-Null
$upgradedInput = $false
for ($i = 0; $i -lt 30; $i++) {
  if ((Eval $A.ws 'document.querySelector("[data-screen-control]")?.classList.contains("on")') -eq $true) { $upgradedInput = $true; break }
  Start-Sleep -Milliseconds 300
}
Check 'input works after mid-session grant' $upgradedInput ''

Get-Process -Name Ruthenium -ErrorAction SilentlyContinue | Stop-Process -Force
if (-not $helperProc.HasExited) { Stop-Process -Id $helperProc.Id -Force -ErrorAction SilentlyContinue }
Start-Sleep -Milliseconds 800
Remove-Item Env:\RUTHENIUM_DATA_DIR, Env:\RUTHENIUM_PAIR_PORT, Env:\WEBVIEW2_USER_DATA_FOLDER -ErrorAction SilentlyContinue
Remove-Item $textFile, $posFile -Force -ErrorAction SilentlyContinue
Write-Host ("RESULT {0}/{1} passed, {2} failed" -f ($script:total - $script:fail), $script:total, $script:fail)
if ($script:fail -gt 0) { exit 1 }
