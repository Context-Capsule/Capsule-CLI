$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing

Add-Type @'
using System;
using System.Runtime.InteropServices;

public static class SnapDigitNative {
    [StructLayout(LayoutKind.Sequential)]
    public struct RECT { public int Left, Top, Right, Bottom; }

    [StructLayout(LayoutKind.Explicit, Size = 40)]
    public struct INPUT {
        [FieldOffset(0)] public uint type;
        [FieldOffset(8)] public KEYBDINPUT keyboard;
    }

    [StructLayout(LayoutKind.Sequential)]
    public struct KEYBDINPUT {
        public ushort vk;
        public ushort scan;
        public uint flags;
        public uint time;
        public UIntPtr extra;
    }

    [DllImport("user32.dll")] public static extern uint SendInput(uint count, INPUT[] inputs, int size);
    [DllImport("user32.dll")] public static extern IntPtr GetKeyboardLayout(uint threadId);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern short VkKeyScanExW(char ch, IntPtr layout);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hwnd);
    [DllImport("user32.dll")] public static extern bool BringWindowToTop(IntPtr hwnd);
    [DllImport("user32.dll")] public static extern IntPtr SetActiveWindow(IntPtr hwnd);
    [DllImport("user32.dll")] public static extern IntPtr SetFocus(IntPtr hwnd);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hwnd, IntPtr processId);
    [DllImport("user32.dll")] public static extern bool AttachThreadInput(uint from, uint to, bool attach);
    [DllImport("kernel32.dll")] public static extern uint GetCurrentThreadId();
    [DllImport("user32.dll", EntryPoint = "IsWindowArranged")]
    [return: MarshalAs(UnmanagedType.Bool)] public static extern bool IsWindowArranged(IntPtr hwnd);
    [DllImport("dwmapi.dll")] public static extern int DwmGetWindowAttribute(IntPtr hwnd, uint attr, out RECT rect, uint size);
}
'@

$KEY_UP = 0x0002
$VK_SHIFT = 0x10
$VK_CONTROL = 0x11
$VK_MENU = 0x12
$VK_LWIN = 0x5B
$VK_Z = 0x5A
$VK_ESCAPE = 0x1B

function New-KeyInput([ushort]$VirtualKey, [uint32]$Flags = 0) {
    $input = New-Object SnapDigitNative+INPUT
    $input.type = 1
    $keyboard = New-Object SnapDigitNative+KEYBDINPUT
    $keyboard.vk = $VirtualKey
    $keyboard.flags = $Flags
    $input.keyboard = $keyboard
    return $input
}

function Send-KeySequence([ushort[]]$VirtualKeys) {
    $list = New-Object System.Collections.Generic.List[SnapDigitNative+INPUT]
    foreach ($virtualKey in $VirtualKeys) { $list.Add((New-KeyInput $virtualKey)) }
    for ($i = $VirtualKeys.Count - 1; $i -ge 0; $i--) { $list.Add((New-KeyInput $VirtualKeys[$i] $KEY_UP)) }
    $inputs = $list.ToArray()
    $sent = [SnapDigitNative]::SendInput($inputs.Length, $inputs, [Runtime.InteropServices.Marshal]::SizeOf([type][SnapDigitNative+INPUT]))
    if ($sent -ne $inputs.Length) { throw "SendInput accepted $sent/$($inputs.Length) events" }
}

function Send-Character([char]$Character) {
    $layout = [SnapDigitNative]::GetKeyboardLayout(0)
    $mapping = [int][SnapDigitNative]::VkKeyScanExW($Character, $layout)
    if ($mapping -eq -1) { throw "Active keyboard layout cannot produce '$Character'" }
    $virtualKey = [ushort]($mapping -band 0xFF)
    $shiftState = ($mapping -shr 8) -band 0xFF
    $keys = New-Object System.Collections.Generic.List[ushort]
    if (($shiftState -band 1) -ne 0) { $keys.Add([ushort]$VK_SHIFT) }
    if (($shiftState -band 2) -ne 0) { $keys.Add([ushort]$VK_CONTROL) }
    if (($shiftState -band 4) -ne 0) { $keys.Add([ushort]$VK_MENU) }
    $keys.Add($virtualKey)
    Send-KeySequence $keys.ToArray()
}

function Focus-Verified([IntPtr]$Hwnd) {
    if ([SnapDigitNative]::GetForegroundWindow() -eq $Hwnd) { return }
    $current = [SnapDigitNative]::GetCurrentThreadId()
    $foreground = [SnapDigitNative]::GetForegroundWindow()
    $foregroundThread = if ($foreground -eq [IntPtr]::Zero) { 0 } else { [SnapDigitNative]::GetWindowThreadProcessId($foreground, [IntPtr]::Zero) }
    $targetThread = [SnapDigitNative]::GetWindowThreadProcessId($Hwnd, [IntPtr]::Zero)
    $attachedForeground = $false
    $attachedTarget = $false
    try {
        if ($foregroundThread -ne 0 -and $foregroundThread -ne $current) { $attachedForeground = [SnapDigitNative]::AttachThreadInput($current, $foregroundThread, $true) }
        if ($targetThread -ne 0 -and $targetThread -ne $current -and $targetThread -ne $foregroundThread) { $attachedTarget = [SnapDigitNative]::AttachThreadInput($current, $targetThread, $true) }
        [SnapDigitNative]::BringWindowToTop($Hwnd) | Out-Null
        [SnapDigitNative]::SetActiveWindow($Hwnd) | Out-Null
        [SnapDigitNative]::SetFocus($Hwnd) | Out-Null
        [SnapDigitNative]::SetForegroundWindow($Hwnd) | Out-Null
        $deadline = [DateTime]::UtcNow.AddMilliseconds(1000)
        while ([DateTime]::UtcNow -lt $deadline) {
            [System.Windows.Forms.Application]::DoEvents()
            if ([SnapDigitNative]::GetForegroundWindow() -eq $Hwnd) { return }
            Start-Sleep -Milliseconds 15
        }
        throw 'Could not focus diagnostic target; refusing Win+Z input'
    } finally {
        if ($attachedTarget) { [SnapDigitNative]::AttachThreadInput($current, $targetThread, $false) | Out-Null }
        if ($attachedForeground) { [SnapDigitNative]::AttachThreadInput($current, $foregroundThread, $false) | Out-Null }
    }
}

function Save-Screenshot([string]$Name) {
    $virtual = [System.Windows.Forms.SystemInformation]::VirtualScreen
    $bitmap = New-Object System.Drawing.Bitmap($virtual.Width, $virtual.Height)
    $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
    $graphics.CopyFromScreen($virtual.Left, $virtual.Top, 0, 0, $bitmap.Size)
    $graphics.Dispose()
    $bitmap.Save((Join-Path $outDir $Name), [System.Drawing.Imaging.ImageFormat]::Png)
    $bitmap.Dispose()
}

$outDir = if ($env:SNAP_DIAG_OUT) { $env:SNAP_DIAG_OUT } else { Join-Path $env:TEMP 'capsule-snap-keyboard-diag' }
New-Item -ItemType Directory -Force -Path $outDir | Out-Null
$form = New-Object System.Windows.Forms.Form
$form.Text = 'Context Capsule Native Snap Diagnostic'
$form.FormBorderStyle = [System.Windows.Forms.FormBorderStyle]::Sizable
$form.MaximizeBox = $true
$form.MinimumSize = New-Object System.Drawing.Size(200, 160)
$form.Size = New-Object System.Drawing.Size(780, 520)
$form.StartPosition = [System.Windows.Forms.FormStartPosition]::CenterScreen
$form.Show()
[System.Windows.Forms.Application]::DoEvents()
Start-Sleep -Milliseconds 300
$hwnd = $form.Handle
Focus-Verified $hwnd

Send-KeySequence @([ushort]$VK_LWIN, [ushort]$VK_Z)
Start-Sleep -Milliseconds 300
Save-Screenshot '01-open.png'
Send-Character '3'
Start-Sleep -Milliseconds 300
Save-Screenshot '02-first-3.png'
Send-Character '3'
Start-Sleep -Milliseconds 300
Save-Screenshot '03-second-3.png'
Send-Character '2'
Start-Sleep -Milliseconds 550
Save-Screenshot '04-zone-2.png'

$rect = New-Object SnapDigitNative+RECT
[SnapDigitNative]::DwmGetWindowAttribute($hwnd, 9, [ref]$rect, [Runtime.InteropServices.Marshal]::SizeOf([type][SnapDigitNative+RECT])) | Out-Null
[pscustomobject]@{
    arranged = [SnapDigitNative]::IsWindowArranged($hwnd)
    pixels = @($rect.Left, $rect.Top, $rect.Right, $rect.Bottom)
    keyboard_layout = ('0x{0:X}' -f ([SnapDigitNative]::GetKeyboardLayout(0).ToInt64()))
} | ConvertTo-Json | Set-Content -Encoding UTF8 (Join-Path $outDir 'result.json')

Send-KeySequence @([ushort]$VK_ESCAPE)
Start-Sleep -Milliseconds 100
$form.Close()
