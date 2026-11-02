param(
    [Parameter(Mandatory=$true)][string]$Installer,
    [Parameter(Mandatory=$true)][string]$OutputDirectory
)
# Real NSIS extraction failure, with no running application or model calls.
$ErrorActionPreference='Stop'
$taskInstaller=(Resolve-Path -LiteralPath $Installer).Path
$taskBase=[IO.Path]::GetFullPath($OutputDirectory)
$taskRoot=Join-Path $taskBase ('write-failure-'+[guid]::NewGuid())
if(![IO.Path]::GetFullPath($taskRoot).StartsWith($taskBase.TrimEnd('\')+'\',[StringComparison]::OrdinalIgnoreCase)){
    throw 'Unexpected isolated installation path'
}
New-Item -ItemType Directory -Path $taskRoot -Force | Out-Null
function Invoke-IsolatedSetup {
    $taskRun=Start-Process -FilePath $taskInstaller -ArgumentList @('/S','/TEST',"/D=$taskRoot") -WindowStyle Hidden -PassThru
    try {
        if(!$taskRun.WaitForExit(30000)){throw 'Isolated installer timed out'}
        return $taskRun.ExitCode
    } finally { $taskRun.Dispose() }
}
if((Invoke-IsolatedSetup) -ne 0){throw 'Initial isolated installation failed'}
$taskMarker=Join-Path $taskRoot 'install-info.txt'
$taskInstalled=[IO.File]::ReadAllText($taskMarker)
$taskPrevious='Synthetic previous version marker'
[IO.File]::WriteAllText($taskMarker,$taskPrevious)
$taskLock=[IO.File]::Open((Join-Path $taskRoot 'README.txt'),[IO.FileMode]::Open,[IO.FileAccess]::Read,[IO.FileShare]::None)
try {
    if((Invoke-IsolatedSetup) -eq 0){throw 'Skipped extraction incorrectly reported success'}
    if([IO.File]::ReadAllText($taskMarker) -ne $taskPrevious){throw 'Failed extraction published a successful version marker'}
} finally { $taskLock.Dispose() }
if((Invoke-IsolatedSetup) -ne 0){throw 'Installation did not recover after releasing the file'}
if([IO.File]::ReadAllText($taskMarker) -ne $taskInstalled){throw 'Successful repair did not publish its version'}
[ordered]@{ok=$true;checks=@('fresh_install','extraction_failure_is_not_success','version_marker_retained_on_failure','repair_after_release');root=$taskRoot;real_model_calls=0} | ConvertTo-Json
