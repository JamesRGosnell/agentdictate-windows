[CmdletBinding()]
param(
    [string]$Binary,
    [ValidateSet('live','streaming','transcript','settings','collapsed','empty','welcome','wrapup','overview','process')][string]$State = 'live',
    [string]$Output,
    # With 'process': leave the window visible to capture so it can be inspected.
    [switch]$Visible,
    # With 'process': replay this WAV as the other side of a call through the
    # real engine and ChatGPT account, then capture the live result.
    [string]$Replay
)
# 'process' launches the real `agentdictated --copilot` with isolated data and
# verifies its native window contract; other states render the UI fixture.
if (!$Binary) {
    $Binary = if ($State -eq 'process') { Join-Path (Split-Path $PSScriptRoot) 'target/debug/agentdictated.exe' }
              else { Join-Path (Split-Path $PSScriptRoot) 'target/debug/examples/verify_copilot.exe' }
}
if (!$Output) { $Output = Join-Path (Split-Path $PSScriptRoot) "target/windows-copilot-$State.png" }
$ErrorActionPreference = 'Stop'
Add-Type -ReferencedAssemblies System.Drawing -TypeDefinition @'
using System;
using System.IO;
using System.Text;
using System.Runtime.InteropServices;
public class AgentDictateCopilotProbe {
 [StructLayout(LayoutKind.Sequential)] struct SA { public int size; public IntPtr descriptor; public int inherit; }
 [StructLayout(LayoutKind.Sequential, CharSet=CharSet.Unicode)] struct SI {public int cb; public string reserved; public string desktop; public string title; public int x,y,cx,cy,cxchars,cychars,fill,flags; public short show,reserved2;public IntPtr reservedPtr,input,output,error;}
 [StructLayout(LayoutKind.Sequential)] struct PI {public IntPtr process,thread;public int pid,tid;}
 [StructLayout(LayoutKind.Sequential)] struct RECT {public int left,top,right,bottom;}
 [StructLayout(LayoutKind.Sequential)] struct POINT {public int x,y;}
 [StructLayout(LayoutKind.Sequential)] struct MONITORINFO {public int size;public RECT monitor,work;public uint flags;}
 [DllImport("user32.dll",CharSet=CharSet.Unicode,SetLastError=true)] static extern IntPtr CreateDesktop(string name,IntPtr device,IntPtr devmode,int flags,uint access,IntPtr security);
 [DllImport("user32.dll")] static extern bool CloseDesktop(IntPtr desktop);
 [DllImport("kernel32.dll",SetLastError=true)] static extern bool CreatePipe(out IntPtr read,out IntPtr write,ref SA security,int size);
 [DllImport("kernel32.dll")] static extern bool SetHandleInformation(IntPtr handle,int mask,int flags);
 [DllImport("kernel32.dll",CharSet=CharSet.Unicode,SetLastError=true)] static extern bool CreateProcess(string application,StringBuilder command,IntPtr psa,IntPtr tsa,bool inherit,uint flags,IntPtr environment,string directory,ref SI startup,out PI info);
 [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr handle);
 [DllImport("kernel32.dll")] static extern uint WaitForSingleObject(IntPtr handle,uint millis);
 [DllImport("kernel32.dll")] static extern bool TerminateProcess(IntPtr handle,uint code);
 [DllImport("kernel32.dll",SetLastError=true)] static extern bool PeekNamedPipe(IntPtr pipe,IntPtr data,int size,IntPtr read,out uint available,IntPtr left);
 delegate bool EnumWindow(IntPtr hwnd,IntPtr state);
 [DllImport("user32.dll")] static extern bool EnumDesktopWindows(IntPtr desktop,EnumWindow callback,IntPtr state);
 [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr hwnd,out int pid);
 [DllImport("user32.dll")] static extern bool GetWindowRect(IntPtr hwnd,out RECT rect);
 [DllImport("user32.dll")] static extern IntPtr GetWindowLongPtr(IntPtr hwnd,int index);
 [DllImport("user32.dll")] static extern bool GetWindowDisplayAffinity(IntPtr hwnd,out uint affinity);
 [DllImport("user32.dll")] static extern bool PrintWindow(IntPtr hwnd,IntPtr dc,uint flags);
 [DllImport("user32.dll")] static extern bool IsWindowVisible(IntPtr hwnd);
 [DllImport("user32.dll")] static extern IntPtr GetForegroundWindow();
 [DllImport("user32.dll")] static extern IntPtr MonitorFromPoint(POINT point,uint flags);
 [DllImport("user32.dll",CharSet=CharSet.Unicode)] static extern bool GetMonitorInfo(IntPtr monitor,ref MONITORINFO info);
 [DllImport("user32.dll")] static extern IntPtr SetThreadDpiAwarenessContext(IntPtr context);
 static IntPtr FindWindow(IntPtr desktop,int pid){
  IntPtr result=IntPtr.Zero;
  EnumDesktopWindows(desktop,delegate(IntPtr hwnd,IntPtr st){int owner;GetWindowThreadProcessId(hwnd,out owner);if(owner!=pid||!IsWindowVisible(hwnd))return true;RECT rect;GetWindowRect(hwnd,out rect);
   if(rect.right-rect.left<100||rect.bottom-rect.top<40)return true;result=hwnd;return false;},IntPtr.Zero);
  return result;
 }
 public static string Run(string exe,string state,string output,bool visible,int settleMilliseconds) {
  var dpiContext=SetThreadDpiAwarenessContext(new IntPtr(-4));
  var original=GetForegroundWindow();
  var name="AgentDictateCopilotProbe-"+Guid.NewGuid().ToString("N");
  var desktop=CreateDesktop(name,IntPtr.Zero,IntPtr.Zero,0,0x000F01FF,IntPtr.Zero);
  if(desktop==IntPtr.Zero) throw new Exception("CreateDesktop: "+Marshal.GetLastWin32Error());
  bool real=state=="process";
  PI pi=new PI();IntPtr parentRead=IntPtr.Zero,childWrite=IntPtr.Zero;
  var report=new StringBuilder();
  try {
   SA sa=new SA{size=Marshal.SizeOf(typeof(SA)),inherit=1};
   if(!CreatePipe(out parentRead,out childWrite,ref sa,0))throw new Exception("CreatePipe failed");
   SetHandleInformation(parentRead,1,0);
   var si=new SI{cb=Marshal.SizeOf(typeof(SI)),desktop=name,flags=0x100,output=childWrite,error=childWrite};
   var command=real?"\""+exe+"\" --copilot":"\""+exe+"\" --isolated-desktop "+state;
   if(!CreateProcess(exe,new StringBuilder(command),IntPtr.Zero,IntPtr.Zero,true,0x08000000,IntPtr.Zero,Path.GetDirectoryName(exe),ref si,out pi))throw new Exception("CreateProcess: "+Marshal.GetLastWin32Error());
   CloseHandle(childWrite);childWrite=IntPtr.Zero;
   IntPtr window=IntPtr.Zero;
   if(real){
    var deadline=DateTime.UtcNow.AddSeconds(30);
    while(DateTime.UtcNow<deadline&&(window=FindWindow(desktop,pi.pid))==IntPtr.Zero){
     if(WaitForSingleObject(pi.process,0)==0)throw new Exception("Copilot exited before opening its window");
     System.Threading.Thread.Sleep(100);
    }
    System.Threading.Thread.Sleep(settleMilliseconds);
   } else {
    var status=new StringBuilder();var deadline=DateTime.UtcNow.AddSeconds(40);
    using(var read=new FileStream(new Microsoft.Win32.SafeHandles.SafeFileHandle(parentRead,true),FileAccess.Read)){
     parentRead=IntPtr.Zero;
     while(DateTime.UtcNow<deadline&&!status.ToString().Contains("READY")){
      uint available;if(!PeekNamedPipe(read.SafeFileHandle.DangerousGetHandle(),IntPtr.Zero,0,IntPtr.Zero,out available,IntPtr.Zero))break;
      if(available>0){var buffer=new byte[available];int got=read.Read(buffer,0,buffer.Length);status.Append(Encoding.UTF8.GetString(buffer,0,got));}
      else System.Threading.Thread.Sleep(50);
     }
    }
    if(!status.ToString().Contains("READY"))throw new Exception("Copilot fixture did not render: "+status);
   }
   window=FindWindow(desktop,pi.pid);
   if(window==IntPtr.Zero)throw new Exception("No copilot window found");
   RECT rect;GetWindowRect(window,out rect);
   int w=rect.right-rect.left,h=rect.bottom-rect.top;
   if(real){
    long ex=GetWindowLongPtr(window,-20).ToInt64();
    if((ex&0x80)==0)throw new Exception("Copilot is missing WS_EX_TOOLWINDOW: "+ex.ToString("X"));
    if((ex&0x8)==0)throw new Exception("Copilot is not topmost: "+ex.ToString("X"));
    if((ex&0x08000000)==0)throw new Exception("Copilot can take focus without typing: "+ex.ToString("X"));
    uint affinity;GetWindowDisplayAffinity(window,out affinity);
    if(affinity!=(visible?0u:0x11u))throw new Exception("Unexpected screen-capture affinity "+affinity);
    var info=new MONITORINFO{size=Marshal.SizeOf(typeof(MONITORINFO))};
    GetMonitorInfo(MonitorFromPoint(new POINT(),1),ref info);
    int expectedX=info.work.left+(info.work.right-info.work.left-w)/2;
    if(Math.Abs(rect.left-expectedX)>2||Math.Abs(rect.top-info.work.top)>2)throw new Exception("Copilot is not at the top center: "+rect.left+","+rect.top+" expected "+expectedX+","+info.work.top);
    report.Append("Window contract: tool window, topmost, no-activate, "+(visible?"capture-visible":"WDA_EXCLUDEFROMCAPTURE")+", top-center at "+rect.left+","+rect.top+" size "+w+"x"+h+". ");
   }
   using(var bitmap=new System.Drawing.Bitmap(w,h)){
    using(var g=System.Drawing.Graphics.FromImage(bitmap)){g.Clear(System.Drawing.Color.FromArgb(255,58,64,82));var dc=g.GetHdc();try{PrintWindow(window,dc,2);}finally{g.ReleaseHdc(dc);}}
    bitmap.Save(output,System.Drawing.Imaging.ImageFormat.Png);
   }
   if(GetForegroundWindow()!=original)throw new Exception("User foreground window changed during isolated probe");
   report.Append("Captured "+state+" copilot to "+output);
   return report.ToString();
  } finally {
   if(pi.process!=IntPtr.Zero){if(WaitForSingleObject(pi.process,0)!=0)TerminateProcess(pi.process,0);CloseHandle(pi.process);CloseHandle(pi.thread);}
   foreach(var handle in new[]{parentRead,childWrite})if(handle!=IntPtr.Zero)CloseHandle(handle);
   CloseDesktop(desktop);SetThreadDpiAwarenessContext(dpiContext);
  }
 }
}
'@
$previousDataHome = $env:AGENTDICTATE_DATA_HOME
try {
    if ($State -eq 'process') {
        $env:AGENTDICTATE_DATA_HOME = Join-Path (Split-Path $PSScriptRoot) ('target/windows-copilot-probe-' + [guid]::NewGuid().ToString('N'))
        New-Item -ItemType Directory -Force -Path $env:AGENTDICTATE_DATA_HOME | Out-Null
        if ($Visible) {
            Set-Content -LiteralPath (Join-Path $env:AGENTDICTATE_DATA_HOME 'copilot.json') -Value '{"hide_from_screen_share": false}' -Encoding ascii
        }
    }
    $settle = 3000
    if ($Replay) {
        $env:AGENTDICTATE_COPILOT_REPLAY = [IO.Path]::GetFullPath($Replay)
        # The isolated data folder has no sign-in; use the installed one.
        $env:AGENTDICTATE_CODEX_HOME = Join-Path $env:LOCALAPPDATA 'AgentDictate\codex-login'
        $settle = 32000
    }
    [AgentDictateCopilotProbe]::Run([IO.Path]::GetFullPath($Binary), $State, [IO.Path]::GetFullPath($Output), [bool]$Visible, $settle)
} finally {
    $env:AGENTDICTATE_DATA_HOME = $previousDataHome
    Remove-Item Env:AGENTDICTATE_COPILOT_REPLAY -ErrorAction SilentlyContinue
    Remove-Item Env:AGENTDICTATE_CODEX_HOME -ErrorAction SilentlyContinue
}
