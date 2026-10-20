param(
 [Parameter(Mandatory=$true)][string]$Binary,
 [Parameter(Mandatory=$true)][string]$OutputDirectory
)
$ErrorActionPreference='Stop'
$Binary=(Resolve-Path -LiteralPath $Binary).Path
$OutputDirectory=[IO.Path]::GetFullPath($OutputDirectory)
New-Item -ItemType Directory -Force -Path $OutputDirectory | Out-Null
# Capture only HWNDs belonging to our newly launched, isolated fixture process.
# Never read the desktop, the installed app, or another process's window.
$captureAssemblies=@('System.Drawing.Common','System.Drawing.Primitives','System.Runtime.InteropServices')
$captureAssemblies+=@(Get-ChildItem -LiteralPath $PSHOME -Filter 'System.Private.Windows.*.dll' | ForEach-Object {$_.FullName})
Add-Type -ReferencedAssemblies $captureAssemblies -TypeDefinition @'
using System;
using System.Drawing;
using System.Drawing.Imaging;
using System.Runtime.InteropServices;
public static class BackdropCapture {
 [DllImport("user32.dll")] static extern bool PrintWindow(IntPtr h, IntPtr dc, uint flags);
 [DllImport("user32.dll")] static extern bool GetClientRect(IntPtr h, out Rect r);
 [DllImport("user32.dll")] static extern bool IsWindowVisible(IntPtr h);
 delegate bool WindowCallback(IntPtr h,IntPtr p);
 [DllImport("user32.dll")] static extern bool EnumWindows(WindowCallback callback,IntPtr p);
 [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr h,out uint pid);
 [DllImport("user32.dll")] public static extern bool MoveWindow(IntPtr h,int x,int y,int w,int height,bool repaint);
 [StructLayout(LayoutKind.Sequential)] struct Rect { public int l,t,r,b; }
 public static bool Ready(IntPtr h) {Rect r;return h!=IntPtr.Zero&&IsWindowVisible(h)&&GetClientRect(h,out r)&&r.r>=900&&r.b>=600;}
 public static IntPtr Find(int pid) {
  IntPtr found=IntPtr.Zero;
  EnumWindows((h,p)=>{uint owner;GetWindowThreadProcessId(h,out owner);if(owner==(uint)pid&&Ready(h)){found=h;return false;}return true;},IntPtr.Zero);
  return found;
 }
 public static void Save(IntPtr hwnd,string path) {
  Rect r; if(!GetClientRect(hwnd,out r))throw new Exception("GetClientRect failed");
  using(var bitmap=new Bitmap(r.r,r.b,PixelFormat.Format24bppRgb)) {
   using(var graphics=Graphics.FromImage(bitmap)) {
    var dc=graphics.GetHdc(); bool ok;
    try {ok=PrintWindow(hwnd,dc,3);} finally {graphics.ReleaseHdc(dc);}
    if(!ok)throw new Exception("PrintWindow failed");
   }
   int bright=0;
   for(int y=10;y<r.b;y+=5)for(int x=10;x<r.r;x+=5)if(bitmap.GetPixel(x,y).R>100)bright++;
   if(bright<10)throw new Exception("Capture has no rendered text/content");
   foreach(var point in new[]{new Point(140,350),new Point(r.r-40,r.b/2)}) {
    var c=bitmap.GetPixel(point.X,point.Y);
    if(c.R>80||c.G>80||c.B>80)throw new Exception("Light background detected: "+c);
   }
   bitmap.Save(path,ImageFormat.Png);
  }
 }
}
'@
$results=@()
foreach($mode in @('--ui-smoke-mica','--ui-smoke-mica-fallback')){
 $fixture=Join-Path $OutputDirectory ($mode.TrimStart('-')+'-'+[guid]::NewGuid().ToString('N'))
 New-Item -ItemType Directory -Path $fixture | Out-Null
 $process=Start-Process -FilePath $Binary -ArgumentList @($mode,'--root',('"'+$fixture+'"')) -WindowStyle Hidden -PassThru
 try {
  $deadline=[DateTime]::UtcNow.AddSeconds(45)
  do {
   Start-Sleep -Milliseconds 200
   $process.Refresh()
   if($process.HasExited){throw 'Fixture exited before its window appeared'}
   $captureWindow=[BackdropCapture]::Find($process.Id)
  }while($captureWindow -eq [IntPtr]::Zero -and [DateTime]::UtcNow -lt $deadline)
  if($captureWindow -eq [IntPtr]::Zero){throw 'Native window timed out'}
  Start-Sleep -Milliseconds 1000
  [BackdropCapture]::Save($captureWindow,(Join-Path $fixture 'native.png'))
  if(![BackdropCapture]::MoveWindow($captureWindow,100,100,960,700,$true)){throw 'Resize failed'}
  Start-Sleep -Milliseconds 500
  [BackdropCapture]::Save($captureWindow,(Join-Path $fixture 'resized.png'))
  if(!$process.WaitForExit(25000)){throw 'Smoke exit timed out'}
  if($process.ExitCode -ne 0){throw "Native smoke failed: $($process.ExitCode)"}
  $report=Get-Content -LiteralPath (Join-Path $fixture 'data/ui-smoke.json') -Raw | ConvertFrom-Json
  if(!$report.ok -or !$report.closed_to_tray -or !$report.tray_message -or !$report.tray_removed){throw 'Native lifecycle failed'}
  if($mode -eq '--ui-smoke-mica-fallback' -and $report.mica){throw 'Failure did not fall back'}
  $results+=@{mode=$mode;mica=$report.mica;directory=$fixture;capture='passed';resize='passed';lifecycle='passed'}
 }finally{if(!$process.HasExited){Stop-Process -Id $process.Id -Force}}
}
$results | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath (Join-Path $OutputDirectory 'native-validation.json') -Encoding utf8NoBOM
$results | ConvertTo-Json
