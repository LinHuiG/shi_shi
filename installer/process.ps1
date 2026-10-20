param(
    [Parameter(Mandatory=$true)][string]$InstallDir,
    [Parameter(Mandatory=$true)][ValidateSet('Probe','Stop')][string]$Action
)
$ErrorActionPreference = 'Stop'
try {
    $target = [IO.Path]::GetFullPath((Join-Path $InstallDir 'shishi.exe'))
    $matches = @(Get-Process -Name shishi -ErrorAction SilentlyContinue | Where-Object {
        $_.Path -and [string]::Equals([IO.Path]::GetFullPath($_.Path), $target, [StringComparison]::OrdinalIgnoreCase)
    })
    if ($Action -eq 'Probe') {
        if ($matches.Count -gt 0) { exit 10 }
        exit 0
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
    exit 0
} catch {
    Write-Output $_.Exception.Message
    exit 1
}
