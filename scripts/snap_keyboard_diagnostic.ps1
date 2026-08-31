$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing

Add-Type @'
using System;
using System.Runtime.InteropServices;

public static class SnapDiagNative {
    [StructLayout(LayoutKind.Sequential)]
    public struct RECT { public int Left, Top, Right, Bottom; }

    [DllImport("user32.dll", SetLastError=true)]
    public static extern bool SetForegroundWindow(IntPtr hWnd);

    [DllImport("user32.dll", SetLastError=true)]
    public static extern bool ShowWindow(IntPtr hWnd, int nCmdShow);

    [DllImport("user32.dll", SetLastError=true)]
    public static extern bool SetWindowPos(IntPtr hWnd, IntPtr hWndInsertAfter, int X, int Y, int cx, int cy, uint uFlags);

    [DllImport("user32.dll")]
    public static extern void keybd_event(byte bVk, byte bScan, uint dwFlags, UIntPtr dwExtraInfo);

    [DllImport("user32.dll", EntryPoint="IsWindowArranged")]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static extern bool IsWindowArranged(IntPtr hWnd);

    [DllImport("dwmapi.dll")]
    public static extern int DwmGetWindowAttribute(IntPtr hwnd, uint dwAttribute, out RECT pvAttribute, uint cbAttribute);
}
'@

$VK_LWIN = 0x5B
$VK_LEFT = 0x25
$VK_UP = 0x26
$VK_RIGHT = 0x27
$VK_DOWN = 0x28
$KEYUP = 0x0002
$SW_RESTORE = 9
$SWP_NOZORDER = 0x0004
$SWP_NOACTIVATE = 0x0010
$DWMWA_EXTENDED_FRAME_BOUNDS = 9

function Send-WinArrow([char]$Arrow) {
    $vk = switch ($Arrow) {
        'L' { $VK_LEFT }
        'R' { $VK_RIGHT }
        'U' { $VK_UP }
        'D' { $VK_DOWN }
        default { throw "Unknown arrow '$Arrow'" }
    }
    [SnapDiagNative]::keybd_event($VK_LWIN, 0, 0, [UIntPtr]::Zero)
    [SnapDiagNative]::keybd_event($vk, 0, 0, [UIntPtr]::Zero)
    [SnapDiagNative]::keybd_event($vk, 0, $KEYUP, [UIntPtr]::Zero)
    [SnapDiagNative]::keybd_event($VK_LWIN, 0, $KEYUP, [UIntPtr]::Zero)
    Start-Sleep -Milliseconds 260
}

function Read-State([IntPtr]$Hwnd, $Work) {
    $rect = New-Object SnapDiagNative+RECT
    $hr = [SnapDiagNative]::DwmGetWindowAttribute($Hwnd, $DWMWA_EXTENDED_FRAME_BOUNDS, [ref]$rect, [Runtime.InteropServices.Marshal]::SizeOf([type][SnapDiagNative+RECT]))
    if ($hr -lt 0) { throw "DwmGetWindowAttribute failed: $hr" }
    $w = [double]$Work.Width
    $h = [double]$Work.Height
    $arranged = $false
    try { $arranged = [SnapDiagNative]::IsWindowArranged($Hwnd) } catch { $arranged = $null }
    [pscustomobject]@{
        arranged = $arranged
        left = $rect.Left
        top = $rect.Top
        right = $rect.Right
        bottom = $rect.Bottom
        x = [math]::Round(($rect.Left - $Work.Left) / $w, 4)
        y = [math]::Round(($rect.Top - $Work.Top) / $h, 4)
        width = [math]::Round(($rect.Right - $rect.Left) / $w, 4)
        height = [math]::Round(($rect.Bottom - $rect.Top) / $h, 4)
    }
}

$form = New-Object System.Windows.Forms.Form
$form.Text = 'Context Capsule Native Snap Diagnostic'
$form.FormBorderStyle = [System.Windows.Forms.FormBorderStyle]::Sizable
$form.MaximizeBox = $true
$form.MinimizeBox = $true
$form.MinimumSize = New-Object System.Drawing.Size(200, 160)
$form.StartPosition = [System.Windows.Forms.FormStartPosition]::Manual
$form.Size = New-Object System.Drawing.Size(780, 520)
$form.Show()
[System.Windows.Forms.Application]::DoEvents()
Start-Sleep -Milliseconds 300
$hwnd = $form.Handle
$screen = [System.Windows.Forms.Screen]::FromHandle($hwnd)
$work = $screen.WorkingArea

$outDir = if ($env:SNAP_DIAG_OUT) { $env:SNAP_DIAG_OUT } else { Join-Path $env:TEMP 'capsule-snap-keyboard-diag' }
New-Item -ItemType Directory -Force -Path $outDir | Out-Null

$sequences = [System.Collections.Generic.List[string]]::new()
foreach ($s in @('L','R','U','D','LL','RR','LU','LD','RU','RD','UL','UR','DL','DR','LLL','RRR','LUR','LDR','RUL','RDL','LR','RL','LRL','RLR','LUL','LDL','RUR','RDR')) { $sequences.Add($s) }

$results = @()
foreach ($sequence in $sequences) {
    [SnapDiagNative]::ShowWindow($hwnd, $SW_RESTORE) | Out-Null
    $bw = [math]::Max(420, [int]($work.Width * 0.52))
    $bh = [math]::Max(300, [int]($work.Height * 0.56))
    $bx = $work.Left + [int](($work.Width - $bw) / 2)
    $by = $work.Top + [int](($work.Height - $bh) / 2)
    [SnapDiagNative]::SetWindowPos($hwnd, [IntPtr]::Zero, $bx, $by, $bw, $bh, $SWP_NOZORDER -bor $SWP_NOACTIVATE) | Out-Null
    [SnapDiagNative]::SetForegroundWindow($hwnd) | Out-Null
    Start-Sleep -Milliseconds 180
    foreach ($c in $sequence.ToCharArray()) { Send-WinArrow $c; [System.Windows.Forms.Application]::DoEvents() }
    Start-Sleep -Milliseconds 180
    $state = Read-State $hwnd $work
    $results += [pscustomobject]@{
        sequence = $sequence
        arranged = $state.arranged
        x = $state.x
        y = $state.y
        width = $state.width
        height = $state.height
        pixels = @($state.left,$state.top,$state.right,$state.bottom)
    }
}

$report = [pscustomobject]@{
    machine = $env:COMPUTERNAME
    screen = [pscustomobject]@{ device=$screen.DeviceName; left=$work.Left; top=$work.Top; width=$work.Width; height=$work.Height }
    results = $results
}
$report | ConvertTo-Json -Depth 6 | Set-Content -Encoding UTF8 (Join-Path $outDir 'keyboard-sequences.json')
$results | Format-Table -AutoSize | Out-String -Width 220 | Set-Content -Encoding UTF8 (Join-Path $outDir 'keyboard-sequences.txt')
$results | Format-Table -AutoSize
$form.Close()
