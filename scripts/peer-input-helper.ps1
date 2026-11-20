# Input-injection probe helper (used by peer-input-probe.ps1).
# Shows a top-most WinForms window with a textbox and continuously writes:
#   $args[0] = textbox content file, $args[1] = cursor position file ("x,y")
param([string]$textFile, [string]$posFile)
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing

$form = New-Object System.Windows.Forms.Form
$form.Text = 'Ruthenium Input Probe'
$form.Width = 520
$form.Height = 220
$form.TopMost = $true
$form.StartPosition = 'Manual'
$form.Left = 80
$form.Top = 80

$tb = New-Object System.Windows.Forms.TextBox
$tb.Multiline = $true
$tb.Dock = 'Fill'
$form.Controls.Add($tb)

$timer = New-Object System.Windows.Forms.Timer
$timer.Interval = 150
$timer.add_Tick({
  try { [IO.File]::WriteAllText($textFile, $tb.Text) } catch { }
  try {
    $p = [System.Windows.Forms.Cursor]::Position
    [IO.File]::WriteAllText($posFile, ("{0},{1}" -f $p.X, $p.Y))
  } catch { }
})
$timer.Start()
$form.Add_Shown({ $form.Activate(); $tb.Focus() })
[System.Windows.Forms.Application]::Run($form)
