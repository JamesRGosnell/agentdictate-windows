[CmdletBinding()]
param([string]$Binary, [ValidateSet('recording','transcribing','cleaning')][string]$State = 'recording')
if (!$Binary) { $Binary = Join-Path (Split-Path $PSScriptRoot) 'target/debug/agentdictated.exe' }
$ErrorActionPreference = 'Stop'
Add-Type -ReferencedAssemblies System.Drawing -TypeDefinition @'
using System;
using System.IO;
using System.Text;
using System.Runtime.InteropServices;
using Microsoft.Win32.SafeHandles;
public class AgentDictateOverlayProbe {
 [StructLayout(LayoutKind.Sequential)] struct SA { public int size; public IntPtr descriptor; public int inherit; }
 [StructLayout(LayoutKind.Sequential, CharSet=CharSet.Unicode)] struct SI {public int cb; public string reserved; public string desktop; public string title; public int x,y,cx,cy,cxchars,cychars,fill,flags; public short show,reserved2;public IntPtr reservedPtr,input,output,error;}
 [StructLayout(LayoutKind.Sequential)] struct PI {public IntPtr process,thread;public int pid,tid;}
 [StructLayout(LayoutKind.Sequential)] struct RECT {public int left,top,right,bottom;}
 [DllImport("user32.dll",CharSet=CharSet.Unicode,SetLastError=true)] static extern IntPtr CreateDesktop(string name,IntPtr device,IntPtr devmode,int flags,uint access,IntPtr security);
 [DllImport("user32.dll")] static extern bool CloseDesktop(IntPtr desktop);
 [DllImport("kernel32.dll",SetLastError=true)] static extern bool CreatePipe(out IntPtr read,out IntPtr write,ref SA security,int size);
 [DllImport("kernel32.dll")] static extern bool SetHandleInformation(IntPtr handle,int mask,int flags);
 [DllImport("kernel32.dll",CharSet=CharSet.Unicode,SetLastError=true)] static extern bool CreateProcess(string application,StringBuilder command,IntPtr psa,IntPtr tsa,bool inherit,uint flags,IntPtr environment,string directory,ref SI startup,out PI info);
 [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr handle);
 [DllImport("kernel32.dll")] static extern uint WaitForSingleObject(IntPtr handle,uint millis);
 [DllImport("kernel32.dll")] static extern bool TerminateProcess(IntPtr handle,uint code);
 [DllImport("kernel32.dll")] static extern bool GetExitCodeProcess(IntPtr handle,out uint code);
 [DllImport("kernel32.dll",SetLastError=true)] static extern bool PeekNamedPipe(IntPtr pipe,IntPtr data,int size,IntPtr read,out uint available,IntPtr left);
 delegate bool EnumWindow(IntPtr hwnd,IntPtr state);
 [DllImport("user32.dll")] static extern bool EnumDesktopWindows(IntPtr desktop,EnumWindow callback,IntPtr state);
 [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr hwnd,out int pid);
 [DllImport("user32.dll")] static extern IntPtr GetWindowLongPtr(IntPtr hwnd,int index);
 [DllImport("user32.dll")] static extern bool GetWindowRect(IntPtr hwnd,out RECT rect);
 [DllImport("user32.dll")] static extern bool GetClientRect(IntPtr hwnd,out RECT rect);
 [DllImport("user32.dll")] static extern bool ClientToScreen(IntPtr hwnd,ref POINT point);
 [DllImport("user32.dll")] static extern bool PrintWindow(IntPtr hwnd,IntPtr dc,uint flags);
 [DllImport("user32.dll")] static extern IntPtr GetForegroundWindow();
 [DllImport("user32.dll")] static extern bool IsWindowVisible(IntPtr hwnd);
 [StructLayout(LayoutKind.Sequential)] struct POINT {public int x,y;}
 [StructLayout(LayoutKind.Sequential)] struct MONITORINFO {public int size;public RECT monitor,work;public uint flags;}
 [DllImport("user32.dll")] static extern IntPtr MonitorFromPoint(POINT point,uint flags);
 [DllImport("user32.dll",CharSet=CharSet.Unicode)] static extern bool GetMonitorInfo(IntPtr monitor,ref MONITORINFO info);
 [DllImport("user32.dll")] static extern uint GetDpiForWindow(IntPtr hwnd);
 [DllImport("user32.dll")] static extern IntPtr SetThreadDpiAwarenessContext(IntPtr context);
 public static string Run(string exe,string data,string json) {
  var dpiContext=SetThreadDpiAwarenessContext(new IntPtr(-4));
  var original=GetForegroundWindow();var name="AgentDictateProbe-"+Guid.NewGuid().ToString("N");
  var desktop=CreateDesktop(name,IntPtr.Zero,IntPtr.Zero,0,0x000F01FF,IntPtr.Zero);
  if(desktop==IntPtr.Zero) throw new Exception("CreateDesktop: "+Marshal.GetLastWin32Error());
  PI pi=new PI();IntPtr childRead=IntPtr.Zero,parentWrite=IntPtr.Zero,parentRead=IntPtr.Zero,childWrite=IntPtr.Zero;
  try {
   SA sa=new SA{size=Marshal.SizeOf(typeof(SA)),inherit=1};
   if(!CreatePipe(out childRead,out parentWrite,ref sa,0)||!CreatePipe(out parentRead,out childWrite,ref sa,0))throw new Exception("CreatePipe failed");
   SetHandleInformation(parentWrite,1,0);SetHandleInformation(parentRead,1,0);
   var si=new SI{cb=Marshal.SizeOf(typeof(SI)),desktop=name,flags=0x100,input=childRead,output=childWrite,error=childWrite};
   if(!CreateProcess(exe,new StringBuilder("\""+exe+"\" --overlay-helper"),IntPtr.Zero,IntPtr.Zero,true,0x08000000,IntPtr.Zero,Path.GetDirectoryName(exe),ref si,out pi))throw new Exception("CreateProcess: "+Marshal.GetLastWin32Error());
   CloseHandle(childRead);childRead=IntPtr.Zero;CloseHandle(childWrite);childWrite=IntPtr.Zero;
   using(var write=new FileStream(new SafeFileHandle(parentWrite,true),FileAccess.Write)){
    parentWrite=IntPtr.Zero;var bytes=Encoding.UTF8.GetBytes(json+"\n");write.Write(bytes,0,bytes.Length);write.Flush();
    var deadline=DateTime.UtcNow.AddSeconds(20);var status=new StringBuilder();
    using(var read=new FileStream(new SafeFileHandle(parentRead,true),FileAccess.Read)){
     while(DateTime.UtcNow<deadline){
      uint available; if(!PeekNamedPipe(parentRead,IntPtr.Zero,0,IntPtr.Zero,out available,IntPtr.Zero))break;
      if(available>0){var buffer=new byte[available];int got=read.Read(buffer,0,buffer.Length);status.Append(Encoding.UTF8.GetString(buffer,0,got));if(status.ToString().Contains("frame_submitted"))break;}
      System.Threading.Thread.Sleep(20);
     }
     parentRead=IntPtr.Zero;
    }
    File.WriteAllText(Path.Combine(data,"overlay-status.txt"),status.ToString());
    if(!status.ToString().Contains("frame_submitted"))throw new Exception("Overlay did not submit a frame: "+status);
    bool found=false;EnumDesktopWindows(desktop,delegate(IntPtr hwnd,IntPtr state){int pid;GetWindowThreadProcessId(hwnd,out pid);if(pid!=pi.pid)return true;long style=GetWindowLongPtr(hwnd,-20).ToInt64();RECT rect;GetWindowRect(hwnd,out rect);if(IsWindowVisible(hwnd)&&rect.right-rect.left>50&&rect.bottom-rect.top>20){if((style&0x08000000)==0||(style&0x80)==0)throw new Exception("Overlay is missing no-activate/tool-window styles: "+style.ToString("X")+" bounds "+rect.left+","+rect.top+","+rect.right+","+rect.bottom);var info=new MONITORINFO{size=Marshal.SizeOf(typeof(MONITORINFO))};
      if(!GetMonitorInfo(MonitorFromPoint(new POINT(),1),ref info))throw new Exception("Monitor metrics unavailable");
      long normalStyle=GetWindowLongPtr(hwnd,-16).ToInt64();
      if((normalStyle&0x80000000L)==0||(normalStyle&0x00C40000L)!=0)throw new Exception("Overlay must be a borderless WS_POPUP, without caption or resize frame");
      RECT client;var origin=new POINT();
      if(!GetClientRect(hwnd,out client)||!ClientToScreen(hwnd,ref origin)||client.right!=rect.right-rect.left||client.bottom!=rect.bottom-rect.top||origin.x!=rect.left||origin.y!=rect.top)throw new Exception("Overlay has a non-client frame or inset");
      var scale=GetDpiForWindow(hwnd)/96.0;var width=(int)Math.Round(200*scale);var height=(int)Math.Round(56*scale);var gap=(int)Math.Round(72*scale);
      int expectedX=info.work.left+(info.work.right-info.work.left-width)/2;int expectedY=info.work.bottom-height-gap;
      if(Math.Abs(rect.left-expectedX)>2||Math.Abs(rect.top-expectedY)>2||Math.Abs(rect.right-rect.left-width)>2||Math.Abs(rect.bottom-rect.top-height)>2)throw new Exception("Overlay is not centered above the primary taskbar: actual "+rect.left+","+rect.top+","+rect.right+","+rect.bottom+" expected "+expectedX+","+expectedY+","+width+","+height);
      using(var bitmap=new System.Drawing.Bitmap(rect.right-rect.left,rect.bottom-rect.top)){
       using(var graphics=System.Drawing.Graphics.FromImage(bitmap)){
        var dc=graphics.GetHdc();try{PrintWindow(hwnd,dc,2);}finally{graphics.ReleaseHdc(dc);}
       }
       bitmap.Save(Path.Combine(data,"overlay.png"),System.Drawing.Imaging.ImageFormat.Png);
      }
      found=true;}return true;},IntPtr.Zero);
    if(!found)throw new Exception("No native overlay window found");
   }
   if(WaitForSingleObject(pi.process,10000)!=0)throw new Exception("Overlay did not exit after its input pipe closed");
   uint exit;GetExitCodeProcess(pi.process,out exit);if(exit!=0)throw new Exception("Overlay exited with "+exit);
   if(GetForegroundWindow()!=original)throw new Exception("User foreground window changed during isolated probe");
   return "Native overlay submitted a frame, was centered above the primary taskbar with no-activate/tool-window styles, and exited on pipe close on an isolated desktop.";
  } finally {
   if(pi.process!=IntPtr.Zero){if(WaitForSingleObject(pi.process,0)!=0)TerminateProcess(pi.process,1);CloseHandle(pi.process);CloseHandle(pi.thread);}
   foreach(var h in new[]{childRead,parentWrite,parentRead,childWrite})if(h!=IntPtr.Zero)CloseHandle(h);
   CloseDesktop(desktop);SetThreadDpiAwarenessContext(dpiContext);
  }
 }
}
'@
$taskProbeData = Join-Path (Split-Path $PSScriptRoot) ('target/windows-overlay-probe-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force -Path $taskProbeData | Out-Null
$previousDataHome = $env:AGENTDICTATE_DATA_HOME
try {
    $env:AGENTDICTATE_DATA_HOME = $taskProbeData
    $taskPhase = @{phase='recording';job_id=[guid]::NewGuid().ToString()}
    if ($State -ne 'recording') { $taskPhase.phase = 'processing'; $taskPhase.stage = $State }
    $taskActiveRecording = $null
    if ($State -eq 'recording') {
        $taskWavePath = Join-Path $taskProbeData 'synthetic-waveform.wav'
        $taskWriter = [IO.BinaryWriter]::new([IO.File]::Create($taskWavePath))
        try {
            $taskWriter.Write([Text.Encoding]::ASCII.GetBytes('RIFF')); $taskWriter.Write([int]5668)
            $taskWriter.Write([Text.Encoding]::ASCII.GetBytes('WAVEfmt ')); $taskWriter.Write([int]16)
            $taskWriter.Write([int16]1); $taskWriter.Write([int16]1); $taskWriter.Write([int]16000)
            $taskWriter.Write([int]32000); $taskWriter.Write([int16]2); $taskWriter.Write([int16]16)
            $taskWriter.Write([Text.Encoding]::ASCII.GetBytes('data')); $taskWriter.Write([int]5632)
            for ($taskSample = 0; $taskSample -lt 2816; $taskSample++) {
                $taskWriter.Write([int16](22000 * [Math]::Sin($taskSample * 0.17) * (0.55 + 0.45 * [Math]::Sin($taskSample * 0.007))))
            }
        } finally { $taskWriter.Dispose() }
        $taskActiveRecording = @{audio_path=$taskWavePath;started_at_unix_millis=[DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds() - 6000}
    }
    $taskOverlayJson = @{workflow=@{phase=$taskPhase};active_recording=$taskActiveRecording} | ConvertTo-Json -Depth 5 -Compress
    [AgentDictateOverlayProbe]::Run([IO.Path]::GetFullPath($Binary),$taskProbeData,$taskOverlayJson)
    Write-Host "Native window capture: $taskProbeData\overlay.png"
} finally { $env:AGENTDICTATE_DATA_HOME = $previousDataHome }
