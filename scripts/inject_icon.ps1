param(
    [string]$ExePath = "target\release\cicada.exe",
    [string]$IcoPath = "app.ico"
)

$ErrorActionPreference = "Stop"

if (!(Test-Path $ExePath)) {
    Write-Error "Executable not found at: $ExePath"
}
if (!(Test-Path $IcoPath)) {
    Write-Error "Icon file not found at: $IcoPath"
}

$code = @"
using System;
using System.IO;
using System.Runtime.InteropServices;

public class Win32IconInjector {
    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    private static extern IntPtr BeginUpdateResource(string pFileName, bool bDeleteExistingResources);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool UpdateResource(IntPtr hUpdate, IntPtr lpType, IntPtr lpName, ushort wLanguage, byte[] lpData, uint cbData);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern bool EndUpdateResource(IntPtr hUpdate, bool fDiscard);

    [DllImport("shell32.dll")]
    public static extern void SHChangeNotify(int wEventId, uint uFlags, IntPtr dwItem1, IntPtr dwItem2);

    private static readonly IntPtr RT_ICON = (IntPtr)3;
    private static readonly IntPtr RT_GROUP_ICON = (IntPtr)14;

    public static bool Inject(string exePath, string icoPath) {
        byte[] icoBytes = File.ReadAllBytes(icoPath);
        if (icoBytes.Length < 6) return false;

        ushort count = BitConverter.ToUInt16(icoBytes, 4);
        IntPtr hUpdate = BeginUpdateResource(exePath, false);
        if (hUpdate == IntPtr.Zero) return false;

        int grpSize = 6 + count * 14;
        byte[] grpBytes = new byte[grpSize];
        Array.Copy(icoBytes, 0, grpBytes, 0, 6);

        for (int i = 0; i < count; i++) {
            int icoEntryOffset = 6 + i * 16;
            int grpEntryOffset = 6 + i * 14;

            Array.Copy(icoBytes, icoEntryOffset, grpBytes, grpEntryOffset, 12);
            ushort nID = (ushort)(i + 1);
            byte[] idBytes = BitConverter.GetBytes(nID);
            grpBytes[grpEntryOffset + 12] = idBytes[0];
            grpBytes[grpEntryOffset + 13] = idBytes[1];

            uint bytesInRes = BitConverter.ToUInt32(icoBytes, icoEntryOffset + 8);
            uint imageOffset = BitConverter.ToUInt32(icoBytes, icoEntryOffset + 12);

            byte[] imgData = new byte[bytesInRes];
            Array.Copy(icoBytes, (int)imageOffset, imgData, 0, (int)bytesInRes);

            if (!UpdateResource(hUpdate, RT_ICON, (IntPtr)nID, 0, imgData, (uint)imgData.Length)) {
                EndUpdateResource(hUpdate, true);
                return false;
            }
        }

        if (!UpdateResource(hUpdate, RT_GROUP_ICON, (IntPtr)1, 0, grpBytes, (uint)grpBytes.Length)) {
            EndUpdateResource(hUpdate, true);
            return false;
        }

        bool success = EndUpdateResource(hUpdate, false);
        if (success) {
            // SHCNE_ASSOCCHANGED (0x08000000) 刷新 Windows Shell 图标缓存
            SHChangeNotify(0x08000000, 0, IntPtr.Zero, IntPtr.Zero);
        }
        return success;
    }
}
"@

if (-not ([System.Management.Automation.PSTypeName]'Win32IconInjector').Type) {
    Add-Type -TypeDefinition $code
}

$fullExe = (Resolve-Path $ExePath).Path
$fullIco = (Resolve-Path $IcoPath).Path

$ok = [Win32IconInjector]::Inject($fullExe, $fullIco)
if ($ok) {
    Write-Host "[OK] Successfully embedded $IcoPath into $ExePath" -ForegroundColor Green
} else {
    Write-Error "[FAIL] Failed to update PE resources for $ExePath"
}

