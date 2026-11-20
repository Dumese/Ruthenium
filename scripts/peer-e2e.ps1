# Peer end-to-end probe (pairing → transfer → control → revocation in one run).
#
# Requirements:
#   - `npm run dev` running (debug exe loads http://localhost:1420)
#   - `cargo build` done (src-tauri\target\debug\Ruthenium.exe)
#   - two instances on one machine: distinct RUTHENIUM_DATA_DIR / RUTHENIUM_PAIR_PORT
#     and distinct WEBVIEW2_USER_DATA_FOLDER (WebView2 cannot share one folder).
#
# Coverage: pairing (numeric comparison) -> SFTP file transfer -> config push + inbox import
#           -> remote terminal control (consent, command, resize, kick) -> revocation denials
#           -> one-tap pairing (Just Works; debug hook, see phase 6).
#
# Ports: A ssh 27410 (auto), pair 28410; B ssh 27411, pair 28411. CDP 9444 / 9446.

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
$exe = Join-Path $repo 'src-tauri\target\debug\Ruthenium.exe'
if (-not (Test-Path $exe)) { throw "missing exe: $exe (run cargo build first)" }

$dirA = Join-Path $env:TEMP 'ruthenium-e2e-A'
$dirB = Join-Path $env:TEMP 'ruthenium-e2e-B'
$dirC = Join-Path $env:TEMP 'ruthenium-e2e-C'
$dirD = Join-Path $env:TEMP 'ruthenium-e2e-D'
Get-Process -Name Ruthenium -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 600
Remove-Item $dirA, $dirB, $dirC, $dirD, "$dirA-webview", "$dirB-webview", "$dirC-webview", "$dirD-webview" -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $dirA, $dirB, $dirC, $dirD | Out-Null

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
    throw "CDP $cdpPort did not become ready"
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
function Invoke-PeerUi { param($ws, [string]$code) Eval $ws ("window.__TAURI_INTERNALS__.invoke(" + $code + ")") }

$A = Start-App $dirA 9444 28410
$B = Start-App $dirB 9446 28411
Check 'A ready' (Ready $A.ws) ''
Check 'B ready' (Ready $B.ws) ''

# Seed one host on A (config push carries real content).
$seed = Eval $A.ws 'window.__TAURI_INTERNALS__.invoke("upsert_host", { host: { id: "e2e-host-1", name: "e2e-host", addr: "127.0.0.1", user: "u", port: 22, group: "g", tag: "prod", auth: "key", keyRef: "C:/k", known: true, agent: false, note: "", log: false, initCommand: "" } }).then(h => h.id).catch(e => "err:" + e)'
Check 'A seed host' ($seed -eq 'e2e-host-1') "seed=$seed"

Open-Peers $A.ws
Open-Peers $B.ws
Start-Sleep -Seconds 3

# ---------- 1. pairing ----------
$typed = Eval $A.ws '(() => { const i = document.querySelector("[data-peer-addr]"); if (!i) return false; const set = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value").set; set.call(i, "127.0.0.1:28411"); i.dispatchEvent(new Event("input", { bubbles: true })); return true; })()'
Check 'A can enter pair address' ($typed -eq $true) ''
Eval $A.ws '(() => { const b = [...document.querySelectorAll(".peerspanel .tbtn")].find(x => x.textContent.includes("\u8fde\u63a5\u914d\u5bf9")); b && b.click(); return !!b; })()' | Out-Null
Start-Sleep -Seconds 2
$codeA = Eval $A.ws 'document.querySelector("[data-pair-code]")?.textContent || ""'
$codeB = Eval $B.ws 'document.querySelector("[data-pair-code]")?.textContent || ""'
Check 'same 6-digit code on both ends' ($codeA -match '^\d{6}$' -and $codeA -eq $codeB) "A=$codeA B=$codeB"
foreach ($k in @('files', 'control', 'config')) {
  $expr = '(() => { const c = document.querySelector(''.modal [data-grant="' + $k + '"]''); if (!c) return false; c.click(); return c.checked; })()'
  Eval $B.ws $expr | Out-Null
  Start-Sleep -Milliseconds 100
}
Eval $B.ws '(() => { const b = document.querySelector("[data-pair-agree]"); b && b.click(); return !!b; })()' | Out-Null
Start-Sleep -Milliseconds 400
Eval $A.ws '(() => { const b = document.querySelector("[data-pair-agree]"); b && b.click(); return !!b; })()' | Out-Null
Start-Sleep -Seconds 3
$peerId = Eval $A.ws 'document.querySelector(".peerrow[data-peer-id]")?.getAttribute("data-peer-id") || ""'
Check 'paired on A' ($peerId -ne '') "peer=$peerId"
$bPeers = [IO.File]::ReadAllText((Join-Path $dirB 'peers.toml'))
Check 'B wrote peers.toml with grants' ($bPeers -match 'grantFiles = true' -and $bPeers -match 'grantControl = true' -and $bPeers -match 'grantConfig = true') ''

# ---------- 2. SFTP file transfer ----------
Eval $A.ws '(() => { const b = document.querySelector(".peerrow[data-peer-id] .peer-sftp"); b && b.click(); return !!b; })()' | Out-Null
Start-Sleep -Seconds 3
$sid = Eval $A.ws '(() => { const t = [...document.querySelectorAll(".tabbar .tab")].find(x => x.textContent.includes("SFTP")); return t ? t.getAttribute("data-arg") : ""; })()'
Check 'peer SFTP tab opened' ($sid -ne '' -and $sid -ne 'null') "sid=$sid"
$root = Invoke-PeerUi $A.ws "'sftp_list', { sessionId: '$sid', path: '/' }"
$rootStr = ($root | ConvertTo-Json -Compress -Depth 5)
Check 'peer root lists drives' ($rootStr -match 'isDir') ''

$token = [Guid]::NewGuid().ToString('N').Substring(0, 8)
$upFile = Join-Path $env:TEMP "peer-e2e-up-$token.txt"
$downFile = Join-Path $env:TEMP "peer-e2e-down-$token.txt"
$remote = "/C/Users/$env:USERNAME/AppData/Local/Temp/peer-e2e-remote-$token.txt"
$remoteReal = Join-Path $env:TEMP "peer-e2e-remote-$token.txt"
$body = "peer-e2e-$token"
Set-Content -Path $upFile -Value $body -Encoding utf8 -NoNewline
Remove-Item $downFile, $remoteReal -Force -ErrorAction SilentlyContinue
$up = Invoke-PeerUi $A.ws "'sftp_upload', { sessionId: '$sid', localPath: '$($upFile.Replace('\','\\'))', remotePath: '$remote' }"
Check 'upload to peer' ($null -ne $up) ''
Start-Sleep -Milliseconds 600
Check 'peer disk has uploaded bytes' ((Test-Path $remoteReal) -and ((Get-Content $remoteReal -Raw) -eq $body)) ''
$down = Invoke-PeerUi $A.ws "'sftp_download', { sessionId: '$sid', remotePath: '$remote', localPath: '$($downFile.Replace('\','\\'))' }"
Check 'download from peer' ($null -ne $down) ''
Start-Sleep -Milliseconds 600
Check 'download content matches' ((Test-Path $downFile) -and ((Get-Content $downFile -Raw) -eq $body)) ''

# ---------- 3. config push + inbox import ----------
$sent = Invoke-PeerUi $A.ws "'peer_send_config', { peerId: '$peerId' }"
Check 'config push accepted' ($null -ne $sent) "bytes=$sent"
Start-Sleep -Seconds 1
$inbox = Get-ChildItem (Join-Path $dirB 'peers\inbox') -Filter *.json -ErrorAction SilentlyContinue
Check 'B inbox has one item' (@($inbox).Count -eq 1) "count=$(@($inbox).Count)"
Check 'B shows inbox item in UI' ((Eval $B.ws 'document.querySelectorAll("[data-inbox-id]").length') -eq 1) ''
Eval $B.ws '(() => { const b = document.querySelector("[data-inbox-import]"); b && b.click(); return !!b; })()' | Out-Null
Start-Sleep -Seconds 2
$bHosts = [IO.File]::ReadAllText((Join-Path $dirB 'hosts.toml'))
Check 'imported config landed in hosts.toml' ($bHosts -match 'e2e-host-1') ''

# ---------- 4. remote terminal control ----------
Eval $A.ws '(() => { const b = document.querySelector(".peerrow[data-peer-id] .peer-control"); b && b.click(); return !!b; })()' | Out-Null
Start-Sleep -Seconds 2
Check 'B asks for consent' ((Eval $B.ws '!!document.querySelector("[data-control-allow]")') -eq $true) ''
Eval $B.ws '(() => { const b = document.querySelector("[data-control-allow]"); if (b) b.click(); return !!b; })()' | Out-Null
Start-Sleep -Seconds 4
$termSid = Eval $A.ws '(() => { const t = [...document.querySelectorAll(".tabbar .tab")].find(x => x.textContent.includes("\u8fdc\u7a0b\u7ec8\u7aef")); return t ? t.getAttribute("data-arg") : ""; })()'
Check 'A got control terminal tab' ($termSid -ne '' -and $termSid -ne 'null') "sid=$termSid"
Check 'B shows control banner' ((Eval $B.ws 'document.querySelector("[data-control-banner]") ? "y" : ""') -eq 'y') ''

$marker = Join-Path $env:TEMP "peer-e2e-ctrl-$token.txt"
Remove-Item $marker -Force -ErrorAction SilentlyContinue
$cmd1 = 'echo ctrl-ok > "' + $marker + '"' + "`r"
$b64 = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($cmd1))
Invoke-PeerUi $A.ws "'write_terminal', { sessionId: '$termSid', data: '$b64' }" | Out-Null
$seen = $false
for ($i = 0; $i -lt 30; $i++) { if (Test-Path $marker) { $seen = $true; break }; Start-Sleep -Milliseconds 300 }
Check 'command executed on peer shell' $seen ''

$widthFile = Join-Path $env:TEMP "peer-e2e-width-$token.txt"
Remove-Item $widthFile -Force -ErrorAction SilentlyContinue
Invoke-PeerUi $A.ws "'resize_terminal', { sessionId: '$termSid', cols: 100, rows: 30 }" | Out-Null
Start-Sleep -Milliseconds 600
$cmd2 = '[Console]::WindowWidth | Set-Content -LiteralPath "' + $widthFile + '"' + "`r"
$b64b = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($cmd2))
Invoke-PeerUi $A.ws "'write_terminal', { sessionId: '$termSid', data: '$b64b' }" | Out-Null
$w = ''
for ($i = 0; $i -lt 30; $i++) { if (Test-Path $widthFile) { $w = (Get-Content $widthFile -Raw).Trim(); break }; Start-Sleep -Milliseconds 300 }
Check 'resize propagated to peer PTY (width=100)' ($w -eq '100') "width=$w"

Eval $B.ws '(() => { const b = document.querySelector("[data-kick]"); if (b) b.click(); return !!b; })()' | Out-Null
Start-Sleep -Seconds 3
$disc = Eval $A.ws '[...document.querySelectorAll(".tabbar .tab")].some(t => t.getAttribute("data-status") === "disconnected")'
Check 'kick marks controller tab disconnected' ($disc -eq $true) ''
Check 'kick removes banner' ((Eval $B.ws 'document.querySelector("[data-control-banner]") ? "y" : ""') -eq '') ''

# denial path
Eval $A.ws '(() => { const t = [...document.querySelectorAll(".tabbar .tab")].find(x => x.textContent.includes("\u8fdc\u7a0b\u7ec8\u7aef")); const x = t && t.querySelector(".tx"); x && x.click(); return !!x; })()' | Out-Null
Start-Sleep -Milliseconds 500
Eval $A.ws '(() => { const b = document.querySelector(".peerrow[data-peer-id] .peer-control"); b && b.click(); return !!b; })()' | Out-Null
Start-Sleep -Seconds 2
Eval $B.ws '(() => { const b = document.querySelector("[data-control-reject]"); if (b) b.click(); return !!b; })()' | Out-Null
$toast = ''
for ($i = 0; $i -lt 20; $i++) {
  $t = Eval $A.ws 'document.querySelector(".toast")?.textContent || ""'
  if ($t -ne '') { $toast = $t; break }
  Start-Sleep -Milliseconds 150
}
Check 'denied control shows an error' ($toast -like '*E_PEER*' -or $toast.Length -gt 0) "toast=$toast"

# 会话中撤销控制授权：服务端 5s 复核并主动断开
# （上一请求刚被拒绝，先等过确认框频控窗口）
Start-Sleep -Seconds 6
Eval $A.ws '(() => { const b = document.querySelector(".peerrow[data-peer-id] .peer-control"); b && b.click(); return !!b; })()' | Out-Null
$allowSeen = $false
for ($i = 0; $i -lt 40; $i++) {
  if ((Eval $B.ws '!!document.querySelector("[data-control-allow]")') -eq $true) {
    Eval $B.ws '(() => { const b = document.querySelector("[data-control-allow]"); if (b) b.click(); return !!b; })()' | Out-Null
    $allowSeen = $true
    break
  }
  Start-Sleep -Milliseconds 300
}
$sid2 = ''
for ($i = 0; $i -lt 60; $i++) {
  $sid2 = Eval $A.ws '(() => { const t = [...document.querySelectorAll(".tabbar .tab")].filter(x => x.textContent.includes("\u8fdc\u7a0b\u7ec8\u7aef")) .map(x => x.getAttribute("data-arg")); return t[t.length - 1] || ""; })()'
  $stNow = Eval $A.ws "document.querySelector('.tabbar .tab[data-arg=\"$sid2\"]')?.getAttribute('data-status') || ''"
  if ($sid2 -match '^[0-9a-fA-F-]{36}$' -and $stNow -eq 'connected') { break }
  Start-Sleep -Milliseconds 300
}
$peerIdB2 = Eval $B.ws 'document.querySelector(".peerrow[data-peer-id]")?.getAttribute("data-peer-id") || ""'
Invoke-PeerUi $B.ws "'peer_update_grants', { peerId: '$peerIdB2', grantFiles: true, grantControl: false, grantConfig: true, grantScreen: false, grantClipboard: false, grantAudio: false }" | Out-Null
$revokedControl = $false
$tabDump = ''
$st = ''
for ($i = 0; $i -lt 40; $i++) {
  $st = Eval $A.ws '[...document.querySelectorAll(".tabbar .tab")].some(t => t.getAttribute("data-status") === "disconnected")'
  $banner = Eval $B.ws 'document.querySelector("[data-control-banner]") ? "y" : ""'
  $tabDump = Eval $A.ws '[...document.querySelectorAll(".tabbar .tab")].map(x => (x.getAttribute("data-arg")||"")+":"+(x.getAttribute("data-status")||"")).join("|")'
  if ($banner -eq '' -and $st -eq $true) { $revokedControl = $true; break }
  Start-Sleep -Milliseconds 300
}
Check 'revoke during control session disconnects' ($allowSeen -and $sid2 -ne '' -and $revokedControl) "allow=$allowSeen sid=$sid2 status=$st banner=$banner tabs=$tabDump"

# ---------- 5. revocation ----------
$peerIdB = Eval $B.ws 'document.querySelector(".peerrow[data-peer-id]")?.getAttribute("data-peer-id") || ""'
Invoke-PeerUi $B.ws "'peer_update_grants', { peerId: '$peerIdB', grantFiles: true, grantControl: true, grantConfig: false, grantScreen: false, grantClipboard: false, grantAudio: false }" | Out-Null
Start-Sleep -Milliseconds 600
$send2 = Eval $A.ws "window.__TAURI_INTERNALS__.invoke('peer_send_config', { peerId: '$peerId' }).then(() => 'ok').catch(e => 'err:' + JSON.stringify(e))"
Check 'config push rejected after revoke' ($send2 -like 'err:*') "res=$send2"
Invoke-PeerUi $B.ws "'peer_update_grants', { peerId: '$peerIdB', grantFiles: false, grantControl: false, grantConfig: false, grantScreen: false, grantClipboard: false, grantAudio: false }" | Out-Null
Start-Sleep -Milliseconds 600
$open2 = Eval $A.ws "window.__TAURI_INTERNALS__.invoke('peer_open_sftp', { peerId: '$peerId' }).then(() => 'ok').catch(e => 'err:' + JSON.stringify(e))"
Check 'SFTP rejected after revoke' ($open2 -like 'err:*') "res=$open2"
$term2 = Eval $A.ws "window.__TAURI_INTERNALS__.invoke('peer_open_terminal', { peerId: '$peerId', cols: 120, rows: 32 }).then(() => 'ok').catch(e => 'err:' + JSON.stringify(e))"
Check 'control rejected after revoke' ($term2 -like 'err:*') "res=$term2"

# ---------- 6. one-tap pairing (fresh instances, debug hook) ----------
# 同机双实例无法同时绑定发现端口 27412，这里用调试构建钩子把“手填地址”
# 按发现列表处理，覆盖一键模式：发起端只显示等待行，接受端不显示 6 位码。
Get-Process -Name Ruthenium -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 800
$env:RUTHENIUM_PAIR_JUSTWORKS = '1'
$C = Start-App $dirC 9448 28412
$D = Start-App $dirD 9450 28413
Remove-Item Env:\RUTHENIUM_PAIR_JUSTWORKS -ErrorAction SilentlyContinue
Check 'C ready' (Ready $C.ws) ''
Check 'D ready' (Ready $D.ws) ''
Open-Peers $C.ws
Open-Peers $D.ws
Start-Sleep -Seconds 1
Eval $C.ws '(() => { const i = document.querySelector("[data-peer-addr]"); if (!i) return false; const set = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value").set; set.call(i, "127.0.0.1:28413"); i.dispatchEvent(new Event("input", { bubbles: true })); return true; })()' | Out-Null
Eval $C.ws '(() => { const b = [...document.querySelectorAll(".peerspanel .tbtn")].find(x => x.textContent.includes("\u8fde\u63a5\u914d\u5bf9")); b && b.click(); return !!b; })()' | Out-Null
Start-Sleep -Seconds 1
$waiting = Eval $C.ws '!!document.querySelector("[data-pair-wait-id]")'
$codeC = Eval $C.ws 'document.querySelector("[data-pair-code]")?.textContent || ""'
$numericD = Eval $D.ws 'document.querySelector("[data-pair-agree]")?.getAttribute("data-pair-numeric") || ""'
$codeD = Eval $D.ws 'document.querySelector("[data-pair-code]")?.textContent || ""'
Check 'initiator shows waiting row without a code dialog' ($waiting -eq $true -and $codeC -eq '') "waiting=$waiting code=$codeC"
Check 'acceptor gets one-tap dialog without a code' ($numericD -eq 'false' -and $codeD -eq '') "numeric=$numericD code=$codeD"
foreach ($k in @('files', 'control', 'config')) {
  $expr = '(() => { const c = document.querySelector(''.modal [data-grant="' + $k + '"]''); if (!c) return false; c.click(); return c.checked; })()'
  Eval $D.ws $expr | Out-Null
  Start-Sleep -Milliseconds 100
}
Eval $D.ws '(() => { const b = document.querySelector("[data-pair-agree]"); b && b.click(); return !!b; })()' | Out-Null
Start-Sleep -Seconds 3
$peerC = Eval $C.ws 'document.querySelector(".peerrow[data-peer-id]")?.getAttribute("data-peer-id") || ""'
$waitGone = Eval $C.ws '!document.querySelector("[data-pair-wait-id]")'
Check 'one-tap pairing completed' ($peerC -ne '' -and $waitGone -eq $true) "peer=$peerC"
$dPeers = [IO.File]::ReadAllText((Join-Path $dirD 'peers.toml'))
Check 'one-tap acceptor wrote grants' ($dPeers -match 'grantFiles = true' -and $dPeers -match 'grantControl = true' -and $dPeers -match 'grantConfig = true') ''

# ---------- cleanup ----------
Get-Process -Name Ruthenium -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 800
Remove-Item Env:\RUTHENIUM_DATA_DIR, Env:\RUTHENIUM_PAIR_PORT, Env:\WEBVIEW2_USER_DATA_FOLDER -ErrorAction SilentlyContinue
Remove-Item $upFile, $downFile, $remoteReal, $marker, $widthFile -Force -ErrorAction SilentlyContinue

Write-Host ("RESULT {0}/{1} passed, {2} failed" -f ($script:total - $script:fail), $script:total, $script:fail)
if ($script:fail -gt 0) { exit 1 }
