$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes

Add-Type @'
using System;
using System.Runtime.InteropServices;

public static class SnapDiagNative {
    [DllImport("user32.dll", SetLastError=true)]
    public static extern bool SetForegroundWindow(IntPtr hWnd);

    [DllImport("user32.dll")]
    public static extern void keybd_event(byte bVk, byte bScan, uint dwFlags, UIntPtr dwExtraInfo);
}
'@

$VK_LWIN = 0x5B
$VK_Z = 0x5A
$VK_ESCAPE = 0x1B
$KEYUP = 0x0002

function Send-Key([byte]$Vk) {
    [SnapDiagNative]::keybd_event($Vk, 0, 0, [UIntPtr]::Zero)
    [SnapDiagNative]::keybd_event($Vk, 0, $KEYUP, [UIntPtr]::Zero)
}

function Send-WinZ {
    [SnapDiagNative]::keybd_event($VK_LWIN, 0, 0, [UIntPtr]::Zero)
    [SnapDiagNative]::keybd_event($VK_Z, 0, 0, [UIntPtr]::Zero)
    [SnapDiagNative]::keybd_event($VK_Z, 0, $KEYUP, [UIntPtr]::Zero)
    [SnapDiagNative]::keybd_event($VK_LWIN, 0, $KEYUP, [UIntPtr]::Zero)
}

$outDir = if ($env:SNAP_DIAG_OUT) { $env:SNAP_DIAG_OUT } else { Join-Path $env:TEMP 'capsule-snap-keyboard-diag' }
New-Item -ItemType Directory -Force -Path $outDir | Out-Null

$form = New-Object System.Windows.Forms.Form
$form.Text = 'Context Capsule Native Snap Diagnostic'
$form.FormBorderStyle = [System.Windows.Forms.FormBorderStyle]::Sizable
$form.MaximizeBox = $true
$form.MinimizeBox = $true
$form.MinimumSize = New-Object System.Drawing.Size(200, 160)
$form.StartPosition = [System.Windows.Forms.FormStartPosition]::CenterScreen
$form.Size = New-Object System.Drawing.Size(780, 520)
$form.Show()
[System.Windows.Forms.Application]::DoEvents()
Start-Sleep -Milliseconds 350
$hwnd = $form.Handle
[SnapDiagNative]::SetForegroundWindow($hwnd) | Out-Null
Start-Sleep -Milliseconds 250
Send-WinZ
Start-Sleep -Milliseconds 650
[System.Windows.Forms.Application]::DoEvents()

# Capture what a user actually sees. This is a desktop capture, not PrintWindow,
# so DWM/Shell overlays such as the Snap flyout are included.
$virtual = [System.Windows.Forms.SystemInformation]::VirtualScreen
$bitmap = New-Object System.Drawing.Bitmap($virtual.Width, $virtual.Height)
$graphics = [System.Drawing.Graphics]::FromImage($bitmap)
$graphics.CopyFromScreen($virtual.Left, $virtual.Top, 0, 0, $bitmap.Size)
$graphics.Dispose()
$bitmap.Save((Join-Path $outDir 'win-z-flyout.png'), [System.Drawing.Imaging.ImageFormat]::Png)
$bitmap.Dispose()

$root = [System.Windows.Automation.AutomationElement]::RootElement
$visibleCondition = New-Object System.Windows.Automation.PropertyCondition([System.Windows.Automation.AutomationElement]::IsOffscreenProperty, $false)
$elements = $root.FindAll([System.Windows.Automation.TreeScope]::Descendants, $visibleCondition)
$rows = @()
for ($i = 0; $i -lt $elements.Count; $i++) {
    $e = $elements.Item($i)
    try {
        $r = $e.Current.BoundingRectangle
        if ($r.Width -le 0 -or $r.Height -le 0) { continue }
        $name = $e.Current.Name
        $auto = $e.Current.AutomationId
        $class = $e.Current.ClassName
        $control = $e.Current.ControlType.ProgrammaticName
        # Keep actionable/named UI and anything near the top half where the
        # Win+Z flyout is rendered. This keeps the dump useful without assuming
        # a particular ShellExperienceHost class name.
        if ([string]::IsNullOrWhiteSpace($name) -and [string]::IsNullOrWhiteSpace($auto) -and $r.Top -gt ($virtual.Top + ($virtual.Height / 2))) { continue }
        $rows += [pscustomobject]@{
            name = $name
            automation_id = $auto
            class_name = $class
            control_type = $control
            process_id = $e.Current.ProcessId
            left = [math]::Round($r.Left, 1)
            top = [math]::Round($r.Top, 1)
            width = [math]::Round($r.Width, 1)
            height = [math]::Round($r.Height, 1)
        }
    } catch {}
}
$rows | ConvertTo-Json -Depth 4 | Set-Content -Encoding UTF8 (Join-Path $outDir 'win-z-uia.json')
$rows | Sort-Object top,left | Format-Table -AutoSize | Out-String -Width 260 | Set-Content -Encoding UTF8 (Join-Path $outDir 'win-z-uia.txt')
$rows | Where-Object { $_.control_type -match 'Button|ListItem|MenuItem' -or $_.name -match 'snap|layout' } | Sort-Object top,left | Format-Table -AutoSize

Send-Key $VK_ESCAPE
Start-Sleep -Milliseconds 150
$form.Close()
