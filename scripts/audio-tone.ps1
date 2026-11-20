# 播放 2 秒 440Hz 正弦（用于音频探针验证 loopback 采集）
param([string]$Path = "$env:TEMP\ruthenium-tone.wav")
$rate = 48000
$seconds = 2
$samples = $rate * $seconds
$data = New-Object byte[] ($samples * 2)
for ($i = 0; $i -lt $samples; $i++) {
  $v = [int16]([math]::Sin(2 * [math]::PI * 440 * $i / $rate) * 12000)
  $b = [BitConverter]::GetBytes($v)
  $data[$i * 2] = $b[0]
  $data[$i * 2 + 1] = $b[1]
}
$ms = New-Object System.IO.MemoryStream
$bw = New-Object System.IO.BinaryWriter($ms)
$bw.Write([Text.Encoding]::ASCII.GetBytes('RIFF'))
$bw.Write([int](36 + $data.Length))
$bw.Write([Text.Encoding]::ASCII.GetBytes('WAVEfmt '))
$bw.Write([int]16)
$bw.Write([int16]1)
$bw.Write([int16]1)
$bw.Write([int]$rate)
$bw.Write([int]($rate * 2))
$bw.Write([int16]2)
$bw.Write([int16]16)
$bw.Write([Text.Encoding]::ASCII.GetBytes('data'))
$bw.Write([int]$data.Length)
$bw.Write($data)
$bw.Flush()
[IO.File]::WriteAllBytes($Path, $ms.ToArray())
$bw.Dispose()
$player = New-Object System.Media.SoundPlayer($Path)
$player.PlaySync()
