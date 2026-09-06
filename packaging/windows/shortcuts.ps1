# Assign the same explicit identity to the executable and its primary shortcuts.
# Windows uses this metadata for application discovery, taskbar grouping and pinning.
if (!('AgentDictateShortcutIdentity' -as [type])) {
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
using System.Runtime.InteropServices.ComTypes;
public static class AgentDictateShortcutIdentity {
 [StructLayout(LayoutKind.Sequential)] struct Key {public Guid format;public uint id;}
 [StructLayout(LayoutKind.Explicit,Size=24)] struct Value {[FieldOffset(0)]public ushort type;[FieldOffset(8)]public IntPtr text;}
 [ComImport,Guid("886D8EEB-8CF2-4446-8D02-CDBA1DBDCF99"),InterfaceType(ComInterfaceType.InterfaceIsIUnknown)] interface Store {
  [PreserveSig]int GetCount(out uint count);
  [PreserveSig]int GetAt(uint index,out Key key);
  [PreserveSig]int GetValue(ref Key key,out Value value);
  [PreserveSig]int SetValue(ref Key key,ref Value value);
  [PreserveSig]int Commit();
 }
 [DllImport("shell32.dll",CharSet=CharSet.Unicode)] static extern void SHChangeNotify(uint change,uint flags,string item,IntPtr other);
 public static void Set(string path,string identity) {
  object link=Activator.CreateInstance(Type.GetTypeFromCLSID(new Guid("00021401-0000-0000-C000-000000000046")));
  var value=new Value{type=31,text=Marshal.StringToCoTaskMemUni(identity)};
  try {
   var file=(IPersistFile)link;file.Load(path,2);
   var key=new Key{format=new Guid("9F4C2855-9F79-4B39-A8D0-E1D42DE1D5F3"),id=5};
   var store=(Store)link;
   Marshal.ThrowExceptionForHR(store.SetValue(ref key,ref value));
   Marshal.ThrowExceptionForHR(store.Commit());file.Save(path,true);
  } finally {Marshal.FreeCoTaskMem(value.text);Marshal.FinalReleaseComObject(link);}
  SHChangeNotify(2,5,path,IntPtr.Zero);
 }
}
'@
}
function New-AgentDictateShortcut([string]$Path, [string]$Directory, [string]$Arguments = '') {
    $shortcut = (New-Object -ComObject WScript.Shell).CreateShortcut($Path)
    $shortcut.TargetPath = Join-Path $Directory 'agentdictate.exe'
    $shortcut.Arguments = $Arguments
    $shortcut.WorkingDirectory = $Directory
    $shortcut.IconLocation = Join-Path $Directory 'agentdictate.ico'
    $shortcut.Description = 'AgentDictate voice dictation and settings'
    $shortcut.Save()
    $identity = 'local.agentdictate.AgentDictate'
    if ($Arguments) { $identity += '.Login' }
    [AgentDictateShortcutIdentity]::Set($Path, $identity)
}
