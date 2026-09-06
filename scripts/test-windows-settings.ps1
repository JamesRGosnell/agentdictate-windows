[CmdletBinding()]
param([string]$Binary)
if (!$Binary) { $Binary = Join-Path (Split-Path $PSScriptRoot) 'target/debug/examples/verify_windows_settings.exe' }
$ErrorActionPreference = 'Stop'
Add-Type -TypeDefinition @'
using System;
using System.IO;
using System.Text;
using System.Runtime.InteropServices;
public class AgentDictateSettingsProbe {
 [StructLayout(LayoutKind.Sequential, CharSet=CharSet.Unicode)] struct SI {public int cb; public string reserved; public string desktop; public string title; public int x,y,cx,cy,cxchars,cychars,fill,flags; public short show,reserved2;public IntPtr reservedPtr,input,output,error;}
 [StructLayout(LayoutKind.Sequential)] struct PI {public IntPtr process,thread;public int pid,tid;}
 [DllImport("user32.dll",CharSet=CharSet.Unicode,SetLastError=true)] static extern IntPtr CreateDesktop(string name,IntPtr device,IntPtr devmode,int flags,uint access,IntPtr security);
 [DllImport("user32.dll")] static extern bool CloseDesktop(IntPtr desktop);
 [DllImport("kernel32.dll",SetLastError=true)] static extern bool SetHandleInformation(IntPtr handle,int mask,int flags);
 [DllImport("kernel32.dll",CharSet=CharSet.Unicode,SetLastError=true)] static extern bool CreateProcess(string application,StringBuilder command,IntPtr psa,IntPtr tsa,bool inherit,uint flags,IntPtr environment,string directory,ref SI startup,out PI info);
 [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr handle);
 [DllImport("kernel32.dll")] static extern uint WaitForSingleObject(IntPtr handle,uint millis);
 [DllImport("kernel32.dll")] static extern bool TerminateProcess(IntPtr handle,uint code);
 [DllImport("kernel32.dll")] static extern bool GetExitCodeProcess(IntPtr handle,out uint code);
 public static void Run(string exe,string log) {
  var name="AgentDictateSettingsProbe-"+Guid.NewGuid().ToString("N");
  var desktop=CreateDesktop(name,IntPtr.Zero,IntPtr.Zero,0,0x000F01FF,IntPtr.Zero);
  if(desktop==IntPtr.Zero) throw new Exception("CreateDesktop: "+Marshal.GetLastWin32Error());
  PI pi=new PI();
  try {
   uint exit;
   using(var output=new FileStream(log,FileMode.Create,FileAccess.Write,FileShare.ReadWrite)) {
    var handle=output.SafeFileHandle.DangerousGetHandle();
    if(!SetHandleInformation(handle,1,1))throw new Exception("Output handle inheritance failed");
    var si=new SI{cb=Marshal.SizeOf(typeof(SI)),desktop=name,flags=0x100,output=handle,error=handle};
    if(!CreateProcess(exe,new StringBuilder("\""+exe+"\" --isolated-desktop"),IntPtr.Zero,IntPtr.Zero,true,0x08000000,IntPtr.Zero,Path.GetDirectoryName(exe),ref si,out pi))throw new Exception("CreateProcess: "+Marshal.GetLastWin32Error());
    if(WaitForSingleObject(pi.process,45000)!=0)throw new Exception("Native Settings probe timed out");
    GetExitCodeProcess(pi.process,out exit);
   }
   if(exit!=0)throw new Exception("Native Settings probe exited with "+exit+": "+File.ReadAllText(log));
  } finally {
   if(pi.process!=IntPtr.Zero){if(WaitForSingleObject(pi.process,0)!=0)TerminateProcess(pi.process,1);CloseHandle(pi.process);CloseHandle(pi.thread);}
   CloseDesktop(desktop);
  }
 }
}
'@
$taskSettingsLog = Join-Path (Split-Path $PSScriptRoot) ('target/windows-settings-probe-' + [guid]::NewGuid().ToString('N') + '.log')
[AgentDictateSettingsProbe]::Run([IO.Path]::GetFullPath($Binary), $taskSettingsLog)
$taskSettingsResults = Get-Content -LiteralPath $taskSettingsLog
if (($taskSettingsResults | Select-String '^PASS native Settings:').Count -ne 8) { throw "Expected all eight Settings fixtures to render. See $taskSettingsLog" }
$taskSettingsResults
