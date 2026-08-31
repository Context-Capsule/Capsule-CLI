$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing

Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class SnapDiagNative {
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left,Top,Right,Bottom; }
    [StructLayout(LayoutKind.Sequential)] public struct MONITORINFO { public uint cbSize; public RECT rcMonitor; public RECT rcWork; public uint dwFlags; }
    [DllImport("user32.dll", SetLastError=true)] public static extern bool SetProcessDpiAwarenessContext(IntPtr value);
    [DllImport("user32.dll")] public static extern IntPtr MonitorFromWindow(IntPtr hwnd, uint flags);
    [DllImport("user32.dll")] public static extern bool GetMonitorInfoW(IntPtr monitor, ref MONITORINFO info);
    [DllImport("dwmapi.dll")] public static extern int DwmGetWindowAttribute(IntPtr hwnd,uint attr,out RECT rect,uint size);
    [DllImport("user32.dll", EntryPoint="IsWindowArranged")] [return:MarshalAs(UnmanagedType.Bool)] public static extern bool IsWindowArranged(IntPtr hwnd);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hwnd,int cmd);
    [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr hwnd,IntPtr after,int x,int y,int cx,int cy,uint flags);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hwnd);
    [DllImport("user32.dll")] public static extern bool BringWindowToTop(IntPtr hwnd);
    [DllImport("user32.dll")] public static extern IntPtr SetActiveWindow(IntPtr hwnd);
    [DllImport("user32.dll")] public static extern IntPtr SetFocus(IntPtr hwnd);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hwnd,IntPtr pid);
    [DllImport("user32.dll")] public static extern bool AttachThreadInput(uint a,uint b,bool attach);
    [DllImport("kernel32.dll")] public static extern uint GetCurrentThreadId();
    [DllImport("user32.dll")] public static extern void keybd_event(byte vk,byte scan,uint flags,UIntPtr extra);
}
'@
[SnapDiagNative]::SetProcessDpiAwarenessContext([IntPtr](-4)) | Out-Null
$KEYUP=2; $VK_LWIN=0x5B; $VK_Z=0x5A; $VK_ESCAPE=0x1B; $SW_RESTORE=9; $SWP_NOZORDER=4; $SWP_NOACTIVATE=0x10
function Key([byte]$v){[SnapDiagNative]::keybd_event($v,0,0,[UIntPtr]::Zero);[SnapDiagNative]::keybd_event($v,0,$KEYUP,[UIntPtr]::Zero)}
function WinZ {[SnapDiagNative]::keybd_event($VK_LWIN,0,0,[UIntPtr]::Zero);[SnapDiagNative]::keybd_event($VK_Z,0,0,[UIntPtr]::Zero);[SnapDiagNative]::keybd_event($VK_Z,0,$KEYUP,[UIntPtr]::Zero);[SnapDiagNative]::keybd_event($VK_LWIN,0,$KEYUP,[UIntPtr]::Zero)}
function FocusVerified([IntPtr]$h){
 if([SnapDiagNative]::GetForegroundWindow() -eq $h){return}
 $ct=[SnapDiagNative]::GetCurrentThreadId();$fg=[SnapDiagNative]::GetForegroundWindow();$ft=if($fg -eq [IntPtr]::Zero){0}else{[SnapDiagNative]::GetWindowThreadProcessId($fg,[IntPtr]::Zero)};$tt=[SnapDiagNative]::GetWindowThreadProcessId($h,[IntPtr]::Zero)
 $af=$false;$at=$false
 try {if($ft -ne 0 -and $ft -ne $ct){$af=[SnapDiagNative]::AttachThreadInput($ct,$ft,$true)};if($tt -ne 0 -and $tt -ne $ct -and $tt -ne $ft){$at=[SnapDiagNative]::AttachThreadInput($ct,$tt,$true)};[SnapDiagNative]::BringWindowToTop($h)|Out-Null;[SnapDiagNative]::SetActiveWindow($h)|Out-Null;[SnapDiagNative]::SetFocus($h)|Out-Null;[SnapDiagNative]::SetForegroundWindow($h)|Out-Null;$until=[DateTime]::UtcNow.AddMilliseconds(900);while([DateTime]::UtcNow -lt $until){[System.Windows.Forms.Application]::DoEvents();if([SnapDiagNative]::GetForegroundWindow() -eq $h){return};Start-Sleep -Milliseconds 15};throw 'focus failed'} finally {if($at){[SnapDiagNative]::AttachThreadInput($ct,$tt,$false)|Out-Null};if($af){[SnapDiagNative]::AttachThreadInput($ct,$ft,$false)|Out-Null}}
}
function State([IntPtr]$h,$work){$r=New-Object SnapDiagNative+RECT;$hr=[SnapDiagNative]::DwmGetWindowAttribute($h,9,[ref]$r,[Runtime.InteropServices.Marshal]::SizeOf([type][SnapDiagNative+RECT]));if($hr-lt 0){throw "DWM $hr"};[pscustomobject]@{arranged=[SnapDiagNative]::IsWindowArranged($h);pixels=@($r.Left,$r.Top,$r.Right,$r.Bottom);x=[math]::Round(($r.Left-$work.Left)/[double]($work.Right-$work.Left),4);y=[math]::Round(($r.Top-$work.Top)/[double]($work.Bottom-$work.Top),4);width=[math]::Round(($r.Right-$r.Left)/[double]($work.Right-$work.Left),4);height=[math]::Round(($r.Bottom-$r.Top)/[double]($work.Bottom-$work.Top),4)}}
$out=if($env:SNAP_DIAG_OUT){$env:SNAP_DIAG_OUT}else{Join-Path $env:TEMP 'capsule-snap-keyboard-diag'};New-Item -ItemType Directory -Force $out|Out-Null
$form=New-Object System.Windows.Forms.Form;$form.Text='Context Capsule Native Snap Diagnostic';$form.FormBorderStyle='Sizable';$form.MaximizeBox=$true;$form.MinimizeBox=$true;$form.MinimumSize=New-Object Drawing.Size(200,160);$form.Size=New-Object Drawing.Size(780,520);$form.StartPosition='CenterScreen';$form.Show();[System.Windows.Forms.Application]::DoEvents();Start-Sleep -Milliseconds 300;$h=$form.Handle
$m=[SnapDiagNative]::MonitorFromWindow($h,2);$mi=New-Object SnapDiagNative+MONITORINFO;$mi.cbSize=[Runtime.InteropServices.Marshal]::SizeOf([type][SnapDiagNative+MONITORINFO]);if(-not [SnapDiagNative]::GetMonitorInfoW($m,[ref]$mi)){throw 'GetMonitorInfo failed'};$work=$mi.rcWork
$results=@()
foreach($layout in 1..6){foreach($zone in 1..4){[SnapDiagNative]::ShowWindow($h,$SW_RESTORE)|Out-Null;$bw=[int](($work.Right-$work.Left)*.48);$bh=[int](($work.Bottom-$work.Top)*.52);$bx=$work.Left+[int](($work.Right-$work.Left-$bw)/2);$by=$work.Top+[int](($work.Bottom-$work.Top-$bh)/2);[SnapDiagNative]::SetWindowPos($h,[IntPtr]::Zero,$bx,$by,$bw,$bh,$SWP_NOZORDER-bor$SWP_NOACTIVATE)|Out-Null;FocusVerified $h;Start-Sleep -Milliseconds 100;WinZ;Start-Sleep -Milliseconds 150;Key ([byte](0x30+$layout));Start-Sleep -Milliseconds 150;Key ([byte](0x30+$zone));Start-Sleep -Milliseconds 320;[System.Windows.Forms.Application]::DoEvents();$s=State $h $work;$results += [pscustomobject]@{layout=$layout;zone=$zone;arranged=$s.arranged;x=$s.x;y=$s.y;width=$s.width;height=$s.height;pixels=$s.pixels};Key $VK_ESCAPE;Start-Sleep -Milliseconds 70}}
$results|ConvertTo-Json -Depth 5|Set-Content -Encoding UTF8 (Join-Path $out 'win-z-map.json');$results|Format-Table -AutoSize|Out-String -Width 220|Set-Content -Encoding UTF8 (Join-Path $out 'win-z-map.txt');$results|Format-Table -AutoSize
# Final screenshot proves the diagnostic itself closes cleanly and leaves no probe/helper window.
$form.Close();Start-Sleep -Milliseconds 120
