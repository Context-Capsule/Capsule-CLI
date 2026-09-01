$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing

Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class SnapMapNative {
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
    [StructLayout(LayoutKind.Sequential)] public struct MONITORINFO { public uint cbSize; public RECT rcMonitor; public RECT rcWork; public uint flags; }
    [StructLayout(LayoutKind.Explicit, Size = 40)] public struct INPUT {
        [FieldOffset(0)] public uint type;
        [FieldOffset(8)] public KEYBDINPUT keyboard;
    }
    [StructLayout(LayoutKind.Sequential)] public struct KEYBDINPUT {
        public ushort vk, scan; public uint flags, time; public UIntPtr extra;
    }
    [DllImport("user32.dll")] public static extern bool SetProcessDpiAwarenessContext(IntPtr value);
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
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hwnd, int command);
    [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr hwnd, IntPtr after, int x, int y, int w, int h, uint flags);
    [DllImport("user32.dll")] public static extern IntPtr MonitorFromWindow(IntPtr hwnd, uint flags);
    [DllImport("user32.dll")] public static extern bool GetMonitorInfoW(IntPtr monitor, ref MONITORINFO info);
    [DllImport("dwmapi.dll")] public static extern int DwmGetWindowAttribute(IntPtr hwnd, uint attr, out RECT rect, uint size);
}
'@

[SnapMapNative]::SetProcessDpiAwarenessContext([IntPtr](-4)) | Out-Null
$KEY_UP = 2; $VK_SHIFT = 0x10; $VK_CONTROL = 0x11; $VK_MENU = 0x12; $VK_LWIN = 0x5B; $VK_Z = 0x5A; $VK_ESCAPE = 0x1B
$SW_RESTORE = 9; $SWP_NOZORDER = 4; $SWP_NOACTIVATE = 0x10

function New-KeyInput([ushort]$vk, [uint32]$flags = 0) {
    $i = New-Object SnapMapNative+INPUT; $i.type = 1
    $k = New-Object SnapMapNative+KEYBDINPUT; $k.vk = $vk; $k.flags = $flags; $i.keyboard = $k
    return $i
}
function Send-KeySequence([ushort[]]$keys) {
    $list = New-Object System.Collections.Generic.List[SnapMapNative+INPUT]
    foreach ($key in $keys) { $list.Add((New-KeyInput $key)) }
    for ($n = $keys.Count - 1; $n -ge 0; $n--) { $list.Add((New-KeyInput $keys[$n] $KEY_UP)) }
    $inputs = $list.ToArray()
    $sent = [SnapMapNative]::SendInput($inputs.Length, $inputs, [Runtime.InteropServices.Marshal]::SizeOf([type][SnapMapNative+INPUT]))
    if ($sent -ne $inputs.Length) { throw "SendInput accepted $sent/$($inputs.Length) events" }
}
function Send-Character([char]$character, [uint32]$targetThread) {
    $layout = [SnapMapNative]::GetKeyboardLayout($targetThread)
    $mapping = [int][SnapMapNative]::VkKeyScanExW($character, $layout)
    if ($mapping -eq -1) { throw "Target keyboard layout cannot generate '$character'" }
    $vk = [ushort]($mapping -band 0xff); $shift = ($mapping -shr 8) -band 0xff
    $keys = New-Object System.Collections.Generic.List[ushort]
    if (($shift -band 1) -ne 0) { $keys.Add([ushort]$VK_SHIFT) }
    if (($shift -band 2) -ne 0) { $keys.Add([ushort]$VK_CONTROL) }
    if (($shift -band 4) -ne 0) { $keys.Add([ushort]$VK_MENU) }
    $keys.Add($vk); Send-KeySequence $keys.ToArray()
}
function Focus-Verified([IntPtr]$hwnd) {
    if ([SnapMapNative]::GetForegroundWindow() -eq $hwnd) { return }
    $current = [SnapMapNative]::GetCurrentThreadId(); $foreground = [SnapMapNative]::GetForegroundWindow()
    $foregroundThread = if ($foreground -eq [IntPtr]::Zero) { 0 } else { [SnapMapNative]::GetWindowThreadProcessId($foreground, [IntPtr]::Zero) }
    $targetThread = [SnapMapNative]::GetWindowThreadProcessId($hwnd, [IntPtr]::Zero)
    $af = $false; $at = $false
    try {
        if ($foregroundThread -ne 0 -and $foregroundThread -ne $current) { $af = [SnapMapNative]::AttachThreadInput($current, $foregroundThread, $true) }
        if ($targetThread -ne 0 -and $targetThread -ne $current -and $targetThread -ne $foregroundThread) { $at = [SnapMapNative]::AttachThreadInput($current, $targetThread, $true) }
        [SnapMapNative]::BringWindowToTop($hwnd) | Out-Null; [SnapMapNative]::SetActiveWindow($hwnd) | Out-Null; [SnapMapNative]::SetFocus($hwnd) | Out-Null; [SnapMapNative]::SetForegroundWindow($hwnd) | Out-Null
        $deadline = [DateTime]::UtcNow.AddMilliseconds(1000)
        while ([DateTime]::UtcNow -lt $deadline) { [Windows.Forms.Application]::DoEvents(); if ([SnapMapNative]::GetForegroundWindow() -eq $hwnd) { return }; Start-Sleep -Milliseconds 15 }
        throw 'Could not focus test window; refusing Snap input'
    } finally {
        if ($at) { [SnapMapNative]::AttachThreadInput($current, $targetThread, $false) | Out-Null }
        if ($af) { [SnapMapNative]::AttachThreadInput($current, $foregroundThread, $false) | Out-Null }
    }
}
function Read-State([IntPtr]$hwnd, $work) {
    $r = New-Object SnapMapNative+RECT
    $hr = [SnapMapNative]::DwmGetWindowAttribute($hwnd, 9, [ref]$r, [Runtime.InteropServices.Marshal]::SizeOf([type][SnapMapNative+RECT]))
    if ($hr -lt 0) { throw "DWM bounds failed: $hr" }
    $ww = [double]($work.Right - $work.Left); $wh = [double]($work.Bottom - $work.Top)
    [pscustomobject]@{ arranged=[SnapMapNative]::IsWindowArranged($hwnd); pixels=@($r.Left,$r.Top,$r.Right,$r.Bottom); x=[math]::Round(($r.Left-$work.Left)/$ww,4); y=[math]::Round(($r.Top-$work.Top)/$wh,4); width=[math]::Round(($r.Right-$r.Left)/$ww,4); height=[math]::Round(($r.Bottom-$r.Top)/$wh,4) }
}

$outDir = if ($env:SNAP_DIAG_OUT) { $env:SNAP_DIAG_OUT } else { Join-Path $env:TEMP 'capsule-snap-layout-map' }
New-Item -ItemType Directory -Force -Path $outDir | Out-Null
$form = New-Object Windows.Forms.Form; $form.Text='Context Capsule Snap Map'; $form.FormBorderStyle='Sizable'; $form.MaximizeBox=$true; $form.MinimumSize=New-Object Drawing.Size(200,160); $form.Size=New-Object Drawing.Size(780,520); $form.StartPosition='CenterScreen'; $form.Show(); [Windows.Forms.Application]::DoEvents(); Start-Sleep -Milliseconds 250
$hwnd=$form.Handle; $targetThread=[SnapMapNative]::GetWindowThreadProcessId($hwnd,[IntPtr]::Zero)
$monitor=[SnapMapNative]::MonitorFromWindow($hwnd,2); $mi=New-Object SnapMapNative+MONITORINFO; $mi.cbSize=[Runtime.InteropServices.Marshal]::SizeOf([type][SnapMapNative+MONITORINFO]); if(-not [SnapMapNative]::GetMonitorInfoW($monitor,[ref]$mi)){throw 'GetMonitorInfo failed'}; $work=$mi.rcWork
$results=@()
foreach($layout in 1..6){
  foreach($zone in 1..4){
    [SnapMapNative]::ShowWindow($hwnd,$SW_RESTORE)|Out-Null
    $w=[int](($work.Right-$work.Left)*0.52); $h=[int](($work.Bottom-$work.Top)*0.56); $x=$work.Left+[int](($work.Right-$work.Left-$w)/2); $y=$work.Top+[int](($work.Bottom-$work.Top-$h)/2)
    [SnapMapNative]::SetWindowPos($hwnd,[IntPtr]::Zero,$x,$y,$w,$h,$SWP_NOZORDER-bor$SWP_NOACTIVATE)|Out-Null; Start-Sleep -Milliseconds 100
    Focus-Verified $hwnd; Send-KeySequence @([ushort]$VK_LWIN,[ushort]$VK_Z); Start-Sleep -Milliseconds 260
    Send-Character ([char]([int][char]'0'+$layout)) $targetThread; Start-Sleep -Milliseconds 220
    Send-Character ([char]([int][char]'0'+$zone)) $targetThread; Start-Sleep -Milliseconds 420
    $s=Read-State $hwnd $work; $results += [pscustomobject]@{layout=$layout;zone=$zone;arranged=$s.arranged;x=$s.x;y=$s.y;width=$s.width;height=$s.height;pixels=$s.pixels}
    Send-KeySequence @([ushort]$VK_ESCAPE); Start-Sleep -Milliseconds 80
  }
}
$report=[pscustomobject]@{machine=$env:COMPUTERNAME;work_area=@($work.Left,$work.Top,$work.Right,$work.Bottom);keyboard_layout=('0x{0:X}' -f ([SnapMapNative]::GetKeyboardLayout($targetThread).ToInt64()));results=$results}
$report|ConvertTo-Json -Depth 6|Set-Content -Encoding UTF8 (Join-Path $outDir 'layout-map.json')
$results|Format-Table -AutoSize|Out-String -Width 220|Set-Content -Encoding UTF8 (Join-Path $outDir 'layout-map.txt')
$results|Format-Table -AutoSize
$form.Close()
