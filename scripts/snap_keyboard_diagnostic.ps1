$ErrorActionPreference='Stop'
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class SnapDiagNative {
 [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left,Top,Right,Bottom; }
 [StructLayout(LayoutKind.Sequential)] public struct MONITORINFO { public uint cbSize; public RECT rcMonitor; public RECT rcWork; public uint flags; }
 [StructLayout(LayoutKind.Explicit, Size=40)] public struct INPUT {
   [FieldOffset(0)] public uint type;
   [FieldOffset(8)] public KEYBDINPUT ki;
 }
 [StructLayout(LayoutKind.Sequential)] public struct KEYBDINPUT { public ushort wVk,wScan; public uint dwFlags,time; public UIntPtr dwExtraInfo; }
 [DllImport("user32.dll")] public static extern uint SendInput(uint n, INPUT[] inputs, int cb);
 [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
 [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
 [DllImport("user32.dll")] public static extern bool BringWindowToTop(IntPtr h);
 [DllImport("user32.dll")] public static extern IntPtr SetActiveWindow(IntPtr h);
 [DllImport("user32.dll")] public static extern IntPtr SetFocus(IntPtr h);
 [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h,IntPtr p);
 [DllImport("user32.dll")] public static extern bool AttachThreadInput(uint a,uint b,bool on);
 [DllImport("kernel32.dll")] public static extern uint GetCurrentThreadId();
 [DllImport("user32.dll",EntryPoint="IsWindowArranged")] [return:MarshalAs(UnmanagedType.Bool)] public static extern bool IsWindowArranged(IntPtr h);
 [DllImport("dwmapi.dll")] public static extern int DwmGetWindowAttribute(IntPtr h,uint a,out RECT r,uint s);
}
'@
$KEYUP=2;$VK_LWIN=0x5B;$VK_Z=0x5A;$VK_ESCAPE=0x1B
function Input([ushort]$vk,[uint]$flags=0){$i=New-Object SnapDiagNative+INPUT;$i.type=1;$k=New-Object SnapDiagNative+KEYBDINPUT;$k.wVk=$vk;$k.dwFlags=$flags;$i.ki=$k;$i}
function SendKeysExact([ushort[]]$keys){$arr=New-Object 'SnapDiagNative+INPUT[]' ($keys.Count*2);$n=0;foreach($k in $keys){$arr[$n]=Input $k 0;$n++};for($j=$keys.Count-1;$j-ge 0;$j--){$arr[$n]=Input $keys[$j] $KEYUP;$n++};$sent=[SnapDiagNative]::SendInput($arr.Length,$arr,[Runtime.InteropServices.Marshal]::SizeOf([type][SnapDiagNative+INPUT]));if($sent-ne$arr.Length){throw "SendInput sent $sent/$($arr.Length)"}}
function FocusVerified([IntPtr]$h){if([SnapDiagNative]::GetForegroundWindow()-eq$h){return};$ct=[SnapDiagNative]::GetCurrentThreadId();$fg=[SnapDiagNative]::GetForegroundWindow();$ft=if($fg-eq[IntPtr]::Zero){0}else{[SnapDiagNative]::GetWindowThreadProcessId($fg,[IntPtr]::Zero)};$tt=[SnapDiagNative]::GetWindowThreadProcessId($h,[IntPtr]::Zero);$af=$false;$at=$false;try{if($ft-ne 0-and$ft-ne$ct){$af=[SnapDiagNative]::AttachThreadInput($ct,$ft,$true)};if($tt-ne 0-and$tt-ne$ct-and$tt-ne$ft){$at=[SnapDiagNative]::AttachThreadInput($ct,$tt,$true)};[SnapDiagNative]::BringWindowToTop($h)|Out-Null;[SnapDiagNative]::SetActiveWindow($h)|Out-Null;[SnapDiagNative]::SetFocus($h)|Out-Null;[SnapDiagNative]::SetForegroundWindow($h)|Out-Null;$u=[DateTime]::UtcNow.AddMilliseconds(1000);while([DateTime]::UtcNow-lt$u){[Windows.Forms.Application]::DoEvents();if([SnapDiagNative]::GetForegroundWindow()-eq$h){return};Start-Sleep -Milliseconds 15};throw 'focus failed'}finally{if($at){[SnapDiagNative]::AttachThreadInput($ct,$tt,$false)|Out-Null};if($af){[SnapDiagNative]::AttachThreadInput($ct,$ft,$false)|Out-Null}}}
function Shot($name){$v=[Windows.Forms.SystemInformation]::VirtualScreen;$b=New-Object Drawing.Bitmap($v.Width,$v.Height);$g=[Drawing.Graphics]::FromImage($b);$g.CopyFromScreen($v.Left,$v.Top,0,0,$b.Size);$g.Dispose();$b.Save((Join-Path $out $name),[Drawing.Imaging.ImageFormat]::Png);$b.Dispose()}
$out=if($env:SNAP_DIAG_OUT){$env:SNAP_DIAG_OUT}else{Join-Path $env:TEMP 'capsule-snap-keyboard-diag'};New-Item -ItemType Directory -Force $out|Out-Null
$f=New-Object Windows.Forms.Form;$f.Text='Context Capsule Native Snap Diagnostic';$f.FormBorderStyle='Sizable';$f.MaximizeBox=$true;$f.MinimumSize=New-Object Drawing.Size(200,160);$f.Size=New-Object Drawing.Size(780,520);$f.StartPosition='CenterScreen';$f.Show();[Windows.Forms.Application]::DoEvents();Start-Sleep -Milliseconds 300;$h=$f.Handle;FocusVerified $h
SendKeysExact @($VK_LWIN,$VK_Z);Start-Sleep -Milliseconds 350;Shot '01-win-z.png'
SendKeysExact @([ushort]0x33);Start-Sleep -Milliseconds 350;Shot '02-layout-3.png'
SendKeysExact @([ushort]0x32);Start-Sleep -Milliseconds 450;Shot '03-zone-2.png'
$r=New-Object SnapDiagNative+RECT;[SnapDiagNative]::DwmGetWindowAttribute($h,9,[ref]$r,[Runtime.InteropServices.Marshal]::SizeOf([type][SnapDiagNative+RECT]))|Out-Null;[pscustomobject]@{arranged=[SnapDiagNative]::IsWindowArranged($h);pixels=@($r.Left,$r.Top,$r.Right,$r.Bottom)}|ConvertTo-Json|Set-Content -Encoding UTF8 (Join-Path $out 'result.json')
SendKeysExact @([ushort]$VK_ESCAPE);Start-Sleep -Milliseconds 100;$f.Close()
