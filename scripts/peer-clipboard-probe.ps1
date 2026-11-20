# Two-instance probe: clipboard text sync.
# A views B's screen with the clipboard grant; verifies the toolbar toggle, remote read/write,
# and that revoking the grant hides/refuses the channel.
# Requires: npm run dev + cargo build (debug exe), like the other peer probes.
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
$dirA = Join-Path $env:TEMP 'ruthenium-clipA'
$dirB = Join-Path $env:TEMP 'ruthenium-clipB'
Get-Process -Name Ruthenium -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 600
Remove-Item $dirA, $dirB, "$dirA-webview", "$dirB-webview" -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $dirA, $dirB | Out-Null

# 保存/恢复开发者剪贴板（探针会改写它）
$originalClip = ''
try { $originalClip = Get-Clipboard -Raw -ErrorAction Stop } catch { }
$marker1 = 'RUTHENIUM-CLIP-' + [Guid]::NewGuid().ToString('N')
$marker2 = 'RUTHENIUM-SET-' + [Guid]::NewGuid().ToString('N')
Set-Clipboard -Value $marker1

# 显示器休眠时 DXGI 会 ACCESS_DENIED：保持唤醒
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
foreach ($k in @('files', 'control', 'config', 'screen', 'clipboard')) {
  $expr = '(() => { const c = document.querySelector(''.modal [data-grant="' + $k + '"]''); if (!c) return false; c.click(); return c.checked; })()'
  Eval $B.ws $expr | Out-Null
  Start-Sleep -Milliseconds 100
}
$clipChecked = Eval $B.ws 'document.querySelector(''.modal [data-grant="clipboard"]'')?.checked === true'
Check 'pair grants include clipboard' ($clipChecked -eq $true) ''
Eval $B.ws '(() => { const b = document.querySelector("[data-pair-agree]"); b && b.click(); return !!b; })()' | Out-Null
Start-Sleep -Milliseconds 400
Eval $A.ws '(() => { const b = document.querySelector("[data-pair-agree]"); b && b.click(); return !!b; })()' | Out-Null
Start-Sleep -Seconds 3
$peerId = Eval $A.ws 'document.querySelector(".peerrow[data-peer-id]")?.getAttribute("data-peer-id") || ""'
Check 'paired' ($peerId -ne '') "peer=$peerId"

# 打开屏幕（clipboard 授权已在配对时勾选）
Eval $A.ws '(() => { const b = document.querySelector(".peerrow[data-peer-id] .peer-screen"); b && b.click(); return !!b; })()' | Out-Null
Start-Sleep -Seconds 2
Eval $B.ws '(() => { const b = document.querySelector("[data-screen-allow]"); if (b) b.click(); return !!b; })()' | Out-Null

$clipBtn = $false
for ($i = 0; $i -lt 40; $i++) {
  if ((Eval $A.ws '!!document.querySelector("[data-screen-clipboard]")') -eq $true) { $clipBtn = $true; break }
  Start-Sleep -Milliseconds 300
}
Check 'viewer offers clipboard toggle' $clipBtn ''

Eval $A.ws '(() => { const b = document.querySelector("[data-screen-clipboard]"); b && b.click(); return !!b; })()' | Out-Null
$clipId = ''
for ($i = 0; $i -lt 30; $i++) {
  $clipId = Eval $A.ws 'document.querySelector(".remoteview")?.getAttribute("data-clipboard-id") || ""'
  if ($clipId -ne '') { break }
  Start-Sleep -Milliseconds 300
}
Check 'clipboard channel opened' ($clipId -ne '') "id=$clipId"
Check 'pull/push buttons visible' ((Eval $A.ws '!!document.querySelector("[data-clip-pull]") && !!document.querySelector("[data-clip-push]")') -eq $true) ''

$got = Eval $A.ws "window.__TAURI_INTERNALS__.invoke('peer_clipboard_get', { id: '$clipId' }).then(t => 'ok:' + t).catch(e => 'err:' + JSON.stringify(e))"
Check 'remote clipboard read' ($got -like "ok:*$marker1*") "res=$got"

$setRes = Eval $A.ws "window.__TAURI_INTERNALS__.invoke('peer_clipboard_set', { id: '$clipId', text: '$marker2' }).then(() => 'ok').catch(e => 'err:' + JSON.stringify(e))"
Start-Sleep -Milliseconds 400
$now = Get-Clipboard -Raw
Check 'remote clipboard write' ($setRes -eq 'ok' -and $now -eq $marker2) "res=$setRes clip=$now"

# UI 两个按钮：拉取 / 发送（成功时给出 toast）
Set-Clipboard -Value $marker1
Eval $A.ws '(() => { const b = document.querySelector("[data-clip-pull]"); b && b.click(); return !!b; })()' | Out-Null
Start-Sleep -Milliseconds 900
$pullToast = Eval $A.ws 'document.querySelector(".toast")?.textContent || ""'
Check 'pull button works' ($pullToast -like '*剪贴板*' -or $pullToast -like '*Clipboard*') "toast=$pullToast"
Set-Clipboard -Value $marker2
Eval $A.ws '(() => { const b = document.querySelector("[data-clip-push]"); b && b.click(); return !!b; })()' | Out-Null
Start-Sleep -Milliseconds 900
$pushToast = Eval $A.ws 'document.querySelector(".toast")?.textContent || ""'
Check 'push button works' ($pushToast -like '*剪贴板*' -or $pushToast -like '*Clipboard*') "toast=$pushToast"

# 关闭通道
Eval $A.ws '(() => { const b = document.querySelector("[data-screen-clipboard]"); b && b.click(); return !!b; })()' | Out-Null
Start-Sleep -Milliseconds 600
$afterOff = Eval $A.ws 'document.querySelector(".remoteview")?.getAttribute("data-clipboard-id") || ""'
Check 'clipboard toggle off' ($afterOff -eq '') "id=$afterOff"
$old = Eval $A.ws "window.__TAURI_INTERNALS__.invoke('peer_clipboard_get', { id: '$clipId' }).then(() => 'ok').catch(e => 'err:' + JSON.stringify(e))"
Check 'closed channel rejects read' ($old -like 'err:*') "res=$old"

# 撤销授权 → 重新打开屏幕后不再有剪贴板入口
$peerIdB = Eval $B.ws 'document.querySelector(".peerrow[data-peer-id]")?.getAttribute("data-peer-id") || ""'
Eval $B.ws "window.__TAURI_INTERNALS__.invoke('peer_update_grants', { peerId: '$peerIdB', grantFiles: true, grantControl: true, grantConfig: true, grantScreen: true, grantClipboard: false, grantAudio: true })" | Out-Null
Start-Sleep -Milliseconds 600
Eval $A.ws '(() => { const t = [...document.querySelectorAll(".tabbar .tab")].find(x => x.textContent.includes("\u5c4f\u5e55")); const x = t && t.querySelector(".tx"); x && x.click(); return !!x; })()' | Out-Null
Start-Sleep -Milliseconds 600
Eval $A.ws '(() => { const b = document.querySelector(".peerrow[data-peer-id] .peer-screen"); b && b.click(); return !!b; })()' | Out-Null
Start-Sleep -Seconds 2
Eval $B.ws '(() => { const b = document.querySelector("[data-screen-allow]"); if (b) b.click(); return !!b; })()' | Out-Null
$btn2 = $false
for ($i = 0; $i -lt 40; $i++) {
  $f = Eval $A.ws 'Number(document.querySelector(".remoteview")?.getAttribute("data-screen-frames") || 0)'
  if ([int]$f -ge 1) { break }
  Start-Sleep -Milliseconds 300
}
$btn2 = Eval $A.ws '!!document.querySelector("[data-screen-clipboard]")'
Check 'revoked grant hides clipboard' ($btn2 -eq $false) "button=$btn2"

Get-Process -Name Ruthenium -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 800
if ($originalClip -ne '') { Set-Clipboard -Value $originalClip } else { Set-Clipboard -Value ' ' }
Remove-Item Env:\RUTHENIUM_DATA_DIR, Env:\RUTHENIUM_PAIR_PORT, Env:\WEBVIEW2_USER_DATA_FOLDER -ErrorAction SilentlyContinue
Write-Host ("RESULT {0}/{1} passed, {2} failed" -f ($script:total - $script:fail), $script:total, $script:fail)
if ($script:fail -gt 0) { exit 1 }
