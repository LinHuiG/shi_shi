param(
    [Parameter(Mandatory=$true)][string]$InstallDir,
    [Parameter(Mandatory=$true)][ValidateSet('Probe','Stop','ReleaseHelpers')][string]$Action
)
$ErrorActionPreference = 'Stop'
function Get-ShishiHelperRole([string]$CommandLine) {
    if (-not ('ShishiProcessArguments' -as [type])) {
        Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class ShishiProcessArguments {
    [DllImport("shell32.dll", CharSet=CharSet.Unicode, SetLastError=true)]
    private static extern IntPtr CommandLineToArgvW(string text, out int count);
    [DllImport("kernel32.dll")]
    private static extern IntPtr LocalFree(IntPtr value);
    public static string[] Parse(string text) {
        int count;
        IntPtr memory=CommandLineToArgvW(text, out count);
        if(memory==IntPtr.Zero) throw new System.ComponentModel.Win32Exception();
        try {
            var result=new string[count];
            for(int i=0;i<count;i++) result[i]=Marshal.PtrToStringUni(Marshal.ReadIntPtr(memory,i*IntPtr.Size));
            return result;
        } finally { LocalFree(memory); }
    }
}
'@
    }
    if ([string]::IsNullOrWhiteSpace($CommandLine)) { return '' }
    $arguments=[ShishiProcessArguments]::Parse($CommandLine)
    # Match main.rs precedence and exact argv values, never text inside a quoted path.
    foreach ($mode in @('--data-transfer','--installer-confirm','--native-dialog-smoke','--live-ai-regression','--desktop-bridge-check')) {
        if ($arguments -ccontains $mode) { return '' }
    }
    foreach ($mode in @('--analysis-submit-mcp','--analysis-mcp','--context-mcp-root','--context-mcp')) {
        if ($arguments -ccontains $mode) { return $mode }
    }
    return ''
}
try {
    $target = [IO.Path]::GetFullPath((Join-Path $InstallDir 'shishi.exe'))
    $matches = @(Get-Process -Name shishi -ErrorAction SilentlyContinue | Where-Object {
        $_.Path -and [string]::Equals([IO.Path]::GetFullPath($_.Path), $target, [StringComparison]::OrdinalIgnoreCase)
    })
    if ($Action -eq 'Probe') {
        if ($matches.Count -gt 0) { exit 10 }
        exit 0
    }
    if ($Action -eq 'ReleaseHelpers') {
        # Inspect the whole candidate set before stopping anything. A newly opened
        # desktop or unknown mode blocks automatic replacement without being killed.
        foreach ($item in $matches) {
            $record=Get-CimInstance Win32_Process -Filter "ProcessId = $($item.Id)" -ErrorAction Stop
            if (-not $record) { continue }
            if (-not [string]::Equals($record.ExecutablePath,$target,[StringComparison]::OrdinalIgnoreCase) -or
                -not (Get-ShishiHelperRole $record.CommandLine)) {
                [Console]::Error.WriteLine('The desktop or an unknown process is still using this installation. Exit Shishi from the tray and retry.')
                exit 10
            }
        }
    }
    foreach ($item in $matches) {
        # Recheck identity immediately before stopping; never close another installation.
        $current = Get-Process -Id $item.Id -ErrorAction SilentlyContinue
        if ($current -and $current.StartTime -eq $item.StartTime -and
            [string]::Equals($current.Path, $target, [StringComparison]::OrdinalIgnoreCase)) {
            Stop-Process -InputObject $current -Force
            if (-not $current.WaitForExit(10000)) { throw 'Timed out waiting for the application to exit' }
        }
    }
    if ($Action -eq 'ReleaseHelpers') {
        $remaining=@(Get-Process -Name shishi -ErrorAction SilentlyContinue | Where-Object {
            $_.Path -and [string]::Equals([IO.Path]::GetFullPath($_.Path),$target,[StringComparison]::OrdinalIgnoreCase)
        })
        if ($remaining.Count -gt 0) {
            [Console]::Error.WriteLine('A new process opened this installation while preparing the update. Please retry.')
            exit 10
        }
    }
    exit 0
} catch {
    if ($Action -eq 'ReleaseHelpers') { [Console]::Error.WriteLine($_.Exception.Message) }
    Write-Output $_.Exception.Message
    exit 1
}
