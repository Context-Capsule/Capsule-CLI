$ErrorActionPreference='Stop'
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
Add-Type @'
using System; using System.Runtime.InteropServices;
public static class N {
 [StructLayout(LayoutKind.Sequential)] public struct R{public int L,T,Ri,B;}
 [StructLayout(LayoutKind.Explicit,Size=40)] public struct I{[FieldOffset(0)]public uint type;[FieldOffset(8)]public K k;}
 [StructLayout(LayoutKind.Sequential)] public struct K{public ushort vk,scan;public uint flags,time;public UIntPtr extra;}
 [DllImport("user32.dll")]public static extern uint SendInput(uint n,I[] a,int s);
 [DllImport("user32.dll")]public static extern IntPtr GetForegroundWindow();
 [DllImport("user32.dll")]public static extern bool SetForegroundWindow(IntPtr h);
 [DllImport("user32.dll")]public static extern bool BringWindowToTop(IntPtr h);
 [DllImport("user32.dll")]public static extern IntPtr SetActiveWindow(IntPtr h);
 [DllImport("user32.dll")]public static extern IntPtr SetFocus(IntPtr h);
 [DllImport("user32.dll")]public static extern uint GetWindowThreadProcessId(IntPtr h,IntPtr p);
 [DllImport("user32.dll")]public static extern bool AttachThreadInput(uint a,uint b,bool on);
 [DllImport("kernel32.dll")]public static extern uint GetCurrentThreadId();
 [DllImport("user32.dll",EntryPoint="IsWindowArranged")][return:MarshalAs(UnmanagedType.Bool)]public static extern bool IsWindowArranged(IntPtr h);
 [DllImport("dwmapi.dll")]public static extern int DwmGetWindowAttribute(IntPtr h,uint a,out R r,uint s);
}
'@
$KU=2;$WIN=0x5B;$Z=0x5A;$ESC=0x1B
function In([ushort]$v,[uint]$f=0){$i=New-Object N+I;$i.type=1;$k=New-Object N+K;$k.vk=$v;$k.flags=$f;$i.k=$k;$i}
function Keys([ushort[]]$ks){$a=New-Object 'N+I[]'($ks.Count*2);$n=0;foreach($k in$ks){$a[$n]=In $k;$n++};for($j=$ks.Count-1;$j-ge 0;$j--){$a[$n]=In $ks[$j] $KU;$n++};if([N]::SendInput($a.Length,$a,[Runtime.InteropServices.Marshal]::SizeOf([type][N+I]))-ne$a.Length){throw 'input blocked'}}
function Focus([IntPtr]$h){$ct=[N]::GetCurrentThreadId();$fg=[N]::GetForegroundWindow();$ft=if($fg-eq[IntPtr]::Zero){0}else{[N]::GetWindowThreadProcessId($fg,[IntPtr]::Zero)};$tt=[N]::GetWindowThreadProcessId($h,[IntPtr]::Zero);$af=$false;$at=$false;try{if($ft-ne 0-and$ft-ne$ct){$af=[N]::AttachThreadInput($ct,$ft,$true)};if($tt-ne 0-and$tt-ne$ct-and$tt-ne$ft){$at=[N]::AttachThreadInput($ct,$tt,$true)};[N]::BringWindowToTop($h)|Out-Null;[N]::SetActiveWindow($h)|Out-Null;[N]::SetFocus($h)|Out-Null;[N]::SetForegroundWindow($h)|Out-Null;$u=[DateTime]::UtcNow.AddMilliseconds(1000);while([DateTime]::UtcNow-lt$u){[Windows.Forms.Application]::DoEvents();if([N]::GetForegroundWindow()-eq$h){return};Start-Sleep -Milliseconds 15};throw 'focus'}finally{if($at){[N]::AttachThreadInput($ct,$tt,$false)|Out-Null};if($af){[N]::AttachThreadInput($ct,$ft,$false)|Out-Null}}}
function Shot($n){$v=[Windows.Forms.SystemInformation]::VirtualScreen;$b=New-Object Drawing.Bitmap($v.Width,$v.Height);$g=[Drawing.Graphics]::FromImage($b);$g.CopyFromScreen($v.Left,$v.Top,0,0,$b.Size);$g.Dispose();$b.Save((Join-Path $out $n),[Drawing.Imaging.ImageFormat]::Png);$b.Dispose()}
$out=if($env:SNAP_DIAG_OUT){$env:SNAP_DIAG_OUT}else{Join-Path $env:TEMP 'capsule-snap-keyboard-diag'};New-Item -Type Directory -Force $out|Out-Null
$f=New-Object Windows.Forms.Form;$f.Text='Context Capsule Native Snap Diagnostic';$f.FormBorderStyle='Sizable';$f.MaximizeBox=$true;$f.MinimumSize=New-Object Drawing.Size(200,160);$f.Size=New-Object Drawing.Size(780,520);$f.StartPosition='CenterScreen';$f.Show();[Windows.Forms.Application]::DoEvents();Start-Sleep -Milliseconds 300;$h=$f.Handle;Focus $h
Keys @($WIN,$Z);Start-Sleep -Milliseconds 320;Shot '01-suggestions.png'
Keys @([ushort]0x33);Start-Sleep -Milliseconds 320;Shot '02-numbered.png'
Keys @([ushort]0x33);Start-Sleep -Milliseconds 320;Shot '03-layout3-selected.png'
Keys @([ushort]0x32);Start-Sleep -Milliseconds 520;Shot '04-zone2-result.png'
$r=New-Object N+R;[N]::DwmGetWindowAttribute($h,9,[ref]$r,[Runtime.InteropServices.Marshal]::SizeOf([type][N+R]))|Out-Null;[pscustomobject]@{arranged=[N]::IsWindowArranged($h);pixels=@($r.L,$r.T,$r.Ri,$r.B)}|ConvertTo-Json|Set-Content -Encoding UTF8(Join-Path $out 'result.json');Keys @([ushort]$ESC);$f.Close()
