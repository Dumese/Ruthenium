# Two-instance probe: remote desktop audio.
# A views B's screen with the audio grant; verifies Opus packets decode, volume/mute,
# loopback capture of a real tone, and that revoking the grant hides the control.
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
$tone = Join-Path $repo 'scripts\audio-tone.ps1'
$dirA = Join-Path $env:TEMP 'ruthenium-audioA'
$dirB = Join-Path $env:TEMP 'ruthenium-audioB'
Get-Process -Name Ruthenium -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 600
Remove-Item $dirA, $dirB, "$dirA-webview", "$dirB-webview" -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $dirA, $dirB | Out-Null

# 显示器休眠会让 DXGI 采集失败（音频依赖屏幕会话）
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

$script:cdp = 1200
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
foreach ($k in @('files', 'control', 'config', 'screen', 'clipboard', 'audio')) {
  $expr = '(() => { const c = document.querySelector(''.modal [data-grant="' + $k + '"]''); if (!c) return false; c.click(); return c.checked; })()'
  Eval $B.ws $expr | Out-Null
  Start-Sleep -Milliseconds 100
}
$audioChecked = Eval $B.ws 'document.querySelector(''.modal [data-grant="audio"]'')?.checked === true'
Check 'pair grants include audio' ($audioChecked -eq $true) ''
Eval $B.ws '(() => { const b = document.querySelector("[data-pair-agree]"); b && b.click(); return !!b; })()' | Out-Null
Start-Sleep -Milliseconds 400
Eval $A.ws '(() => { const b = document.querySelector("[data-pair-agree]"); b && b.click(); return !!b; })()' | Out-Null
Start-Sleep -Seconds 3
$peerId = Eval $A.ws 'document.querySelector(".peerrow[data-peer-id]")?.getAttribute("data-peer-id") || ""'
Check 'paired' ($peerId -ne '') "peer=$peerId"

Eval $A.ws '(() => { const b = document.querySelector(".peerrow[data-peer-id] .peer-screen"); b && b.click(); return !!b; })()' | Out-Null
Start-Sleep -Seconds 2
Eval $B.ws '(() => { const b = document.querySelector("[data-screen-allow]"); if (b) b.click(); return !!b; })()' | Out-Null

$audioBtn = $false
for ($i = 0; $i -lt 40; $i++) {
  if ((Eval $A.ws '!!document.querySelector("[data-screen-audio]")') -eq $true) { $audioBtn = $true; break }
  Start-Sleep -Milliseconds 300
}
Check 'viewer offers audio toggle' $audioBtn ''

Eval $A.ws '(() => { const b = document.querySelector("[data-screen-audio]"); b && b.click(); return !!b; })()' | Out-Null
$decoded = 0
$rate = 0
for ($i = 0; $i -lt 50; $i++) {
  $decoded = [int](Eval $A.ws 'Number(document.querySelector(".remoteview")?.getAttribute("data-audio-decoded") || 0)')
  $rate = [int](Eval $A.ws 'Number(document.querySelector(".remoteview")?.getAttribute("data-audio-rate") || 0)')
  if ($decoded -ge 5) { break }
  Start-Sleep -Milliseconds 300
}
Check 'Opus audio decodes' ($decoded -ge 5 -and $rate -eq 48000) "decoded=$decoded rate=$rate"
Check 'volume + mute controls present' ((Eval $A.ws '!!document.querySelector("[data-audio-volume]") && !!document.querySelector("[data-audio-mute]")') -eq $true) ''

# 静音（避免本机回环把 A 自己的播放再采回去）
Eval $A.ws '(() => { const b = document.querySelector("[data-audio-mute]"); b && b.click(); return !!b; })()' | Out-Null
Start-Sleep -Milliseconds 300
Check 'mute applies' ((Eval $A.ws 'document.querySelector(".remoteview")?.getAttribute("data-audio-muted")') -eq '1') ''

# 在 B 的系统播放设备上播放 2s 440Hz，A 应通过 loopback 收到非零样本
$toneProc = Start-Process -FilePath 'powershell.exe' -ArgumentList @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $tone) -PassThru
$peak = 0.0
for ($i = 0; $i -lt 40; $i++) {
  $peak = [double](Eval $A.ws 'Number(document.querySelector(".remoteview")?.getAttribute("data-audio-peak") || 0)')
  if ($peak -ge 0.01) { break }
  Start-Sleep -Milliseconds 300
}
Check 'loopback captures system audio' ($peak -ge 0.01) "peak=$peak"
if (-not $toneProc.HasExited) { $toneProc.WaitForExit(4000) | Out-Null }

Eval $A.ws '(() => { const b = document.querySelector("[data-audio-mute]"); b && b.click(); return !!b; })()' | Out-Null
Start-Sleep -Milliseconds 200
Check 'unmute applies' ((Eval $A.ws 'document.querySelector(".remoteview")?.getAttribute("data-audio-muted")') -eq '0') ''

Eval $A.ws '(() => { const b = document.querySelector("[data-screen-audio]"); b && b.click(); return !!b; })()' | Out-Null
Start-Sleep -Milliseconds 600
Check 'audio can be stopped' ((Eval $A.ws 'document.querySelector(".remoteview")?.getAttribute("data-audio-state")') -eq 'off') ''

# 撤销授权 → 重新打开屏幕后不再有音频入口
$peerIdB = Eval $B.ws 'document.querySelector(".peerrow[data-peer-id]")?.getAttribute("data-peer-id") || ""'
Eval $B.ws "window.__TAURI_INTERNALS__.invoke('peer_update_grants', { peerId: '$peerIdB', grantFiles: true, grantControl: true, grantConfig: true, grantScreen: true, grantClipboard: true, grantAudio: false })" | Out-Null
Start-Sleep -Milliseconds 600
Eval $A.ws '(() => { const t = [...document.querySelectorAll(".tabbar .tab")].find(x => x.textContent.includes("\u5c4f\u5e55")); const x = t && t.querySelector(".tx"); x && x.click(); return !!x; })()' | Out-Null
Start-Sleep -Milliseconds 600
Eval $A.ws '(() => { const b = document.querySelector(".peerrow[data-peer-id] .peer-screen"); b && b.click(); return !!b; })()' | Out-Null
Start-Sleep -Seconds 2
Eval $B.ws '(() => { const b = document.querySelector("[data-screen-allow]"); if (b) b.click(); return !!b; })()' | Out-Null
$btn2 = $true
for ($i = 0; $i -lt 40; $i++) {
  $f = Eval $A.ws 'Number(document.querySelector(".remoteview")?.getAttribute("data-screen-frames") || 0)'
  if ([int]$f -ge 1) { break }
  Start-Sleep -Milliseconds 300
}
$btn2 = Eval $A.ws '!!document.querySelector("[data-screen-audio]")'
Check 'revoked audio grant hides control' ($btn2 -eq $false) "button=$btn2"

Get-Process -Name Ruthenium -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 800
Remove-Item Env:\RUTHENIUM_DATA_DIR, Env:\RUTHENIUM_PAIR_PORT, Env:\WEBVIEW2_USER_DATA_FOLDER -ErrorAction SilentlyContinue
Remove-Item "$env:TEMP\ruthenium-tone.wav" -Force -ErrorAction SilentlyContinue
Write-Host ("RESULT {0}/{1} passed, {2} failed" -f ($script:total - $script:fail), $script:total, $script:fail)
if ($script:fail -gt 0) { exit 1 }
