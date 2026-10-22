param(
    [Parameter(Mandatory=$true)][string]$InitialInstaller,
    [Parameter(Mandatory=$true)][string]$TargetInstaller,
    [Parameter(Mandatory=$true)][string]$TargetVersion,
    [Parameter(Mandatory=$true)][string]$TestRoot,
    [Parameter(Mandatory=$true)][string]$Report
)
$ErrorActionPreference='Stop'
$taskRoot=[IO.Path]::GetFullPath($TestRoot)
if(Test-Path -LiteralPath $taskRoot){throw 'Use a new isolated test directory'}
New-Item -ItemType Directory -Path $taskRoot | Out-Null
$taskInstall=Join-Path $taskRoot 'installed with spaces'
$taskOther=Join-Path $taskRoot 'other installation'
$taskProcesses=[Collections.Generic.List[Diagnostics.Process]]::new()
function Start-Fixture([string]$File,[string[]]$Arguments,[switch]$Stdin,[string]$RawArguments) {
    $taskInfo=[Diagnostics.ProcessStartInfo]::new()
    $taskInfo.FileName=$File;$taskInfo.UseShellExecute=$false;$taskInfo.CreateNoWindow=$true;$taskInfo.WindowStyle='Hidden'
    $taskInfo.RedirectStandardInput=$Stdin.IsPresent
    $taskInfo.RedirectStandardOutput=$true;$taskInfo.RedirectStandardError=$true
    if($RawArguments){$taskInfo.Arguments=$RawArguments}
    else{foreach($taskArgument in $Arguments){$taskInfo.ArgumentList.Add($taskArgument)}}
    $taskProcess=[Diagnostics.Process]::Start($taskInfo)
    $taskProcesses.Add($taskProcess)
    return $taskProcess
}
function Start-Mcp([string]$Directory) {
    $taskMcp=Start-Fixture (Join-Path $Directory 'shishi.exe') @('--context-mcp-root',$Directory) -Stdin
    $taskMcp.StandardInput.WriteLine('{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}')
    $taskMcp.StandardInput.Flush()
    $taskRead=$taskMcp.StandardOutput.ReadLineAsync()
    if(!$taskRead.Wait(10000) -or ($taskRead.Result | ConvertFrom-Json).id -ne 1){throw 'Synthetic MCP startup failed'}
    return $taskMcp
}
try {
    foreach($taskDirectory in @($taskInstall,$taskOther)) {
        # NSIS requires /D to be last and unquoted, including paths with spaces.
        # ProcessStartInfo.ArgumentList would quote this special NSIS argument.
        $taskSetup=Start-Fixture $InitialInstaller @() -RawArguments "/S /TEST /D=$taskDirectory"
        if(!$taskSetup.WaitForExit(30000) -or $taskSetup.ExitCode -ne 0){throw 'Initial isolated install failed'}
    }
    $taskSeed=Start-Fixture (Join-Path $taskInstall 'shishi.exe') @('--ui-smoke','--root',$taskInstall)
    if(!$taskSeed.WaitForExit(45000) -or $taskSeed.ExitCode -ne 0){throw 'Synthetic data seed failed'}
    $taskData=Join-Path $taskInstall 'data'
    $taskCache=Join-Path $taskData 'updates'
    New-Item -ItemType Directory -Path $taskCache -Force | Out-Null
    $taskSentinel=Join-Path $taskData 'synthetic-preserved.txt'
    [IO.File]::WriteAllText($taskSentinel,'Synthetic upgrade preservation')
    $taskState=Get-Content -LiteralPath (Join-Path $taskData 'state.json') -Raw | ConvertFrom-Json
    $taskExpectedConfig=$taskState.config | ConvertTo-Json -Depth 50 -Compress
    # The test owner keeps stdin open. These servers are outside the desktop's
    # owned process jobs, as with an external Codex client.
    $taskMcp=Start-Mcp $taskInstall
    $taskOtherMcp=Start-Mcp $taskOther
    $taskProbe=Start-Fixture (Join-Path $env:SystemRoot 'System32/WindowsPowerShell/v1.0/powershell.exe') @('-NoProfile','-NonInteractive','-ExecutionPolicy','Bypass','-File',(Join-Path $PSScriptRoot '../installer/process.ps1'),'-InstallDir',$taskInstall,'-Action','Probe')
    if(!$taskProbe.WaitForExit(10000) -or $taskProbe.ExitCode -ne 10){throw 'The held MCP did not reproduce the installer blocker'}
    # Keep an isolated desktop hidden before ui_ready. If a desktop reopens,
    # preparation must block before stopping even an otherwise valid MCP.
    $taskUi=Join-Path $taskRoot 'not-ready-ui'
    New-Item -ItemType Directory -Path $taskUi | Out-Null
    [IO.File]::WriteAllText((Join-Path $taskUi 'manifest.txt'),'')
    [IO.File]::WriteAllText((Join-Path $taskUi 'index.html'),'<html><head><!--STYLES--></head><body><script>/*SCRIPTS*/</script></body></html>')
    $taskDesktop=Start-Fixture (Join-Path $taskInstall 'shishi.exe') @('--ui-smoke','--root',$taskInstall,'--ui-dir',$taskUi)
    $taskGuard=Start-Fixture (Join-Path $env:SystemRoot 'System32/WindowsPowerShell/v1.0/powershell.exe') @('-NoProfile','-NonInteractive','-ExecutionPolicy','Bypass','-File',(Join-Path $PSScriptRoot '../installer/process.ps1'),'-InstallDir',$taskInstall,'-Action','ReleaseHelpers')
    if(!$taskGuard.WaitForExit(15000) -or $taskGuard.ExitCode -ne 10){throw 'A reopened desktop did not block automatic replacement'}
    if($taskDesktop.HasExited -or $taskMcp.HasExited){throw 'Preparation stopped a desktop or partially stopped helpers despite the desktop blocker'}
    $taskDesktop.Kill()
    if(!$taskDesktop.WaitForExit(10000)){throw 'Synthetic desktop did not exit'}
    $taskDigest=(Get-FileHash -LiteralPath $TargetInstaller -Algorithm SHA256).Hash.ToLowerInvariant()
    $taskPackage=Join-Path $taskCache ("Shishi-Setup-$TargetVersion-"+$taskDigest.Substring(0,12)+'.exe')
    Copy-Item -LiteralPath $TargetInstaller -Destination $taskPackage
    $taskHelperDir=Join-Path $taskCache ('apply-'+[guid]::NewGuid().ToString())
    New-Item -ItemType Directory -Path $taskHelperDir | Out-Null
    Copy-Item -LiteralPath (Join-Path $taskInstall 'shishi.exe') -Destination (Join-Path $taskHelperDir 'shishi-update.exe')
    Copy-Item -LiteralPath (Join-Path $taskInstall 'WebView2Loader.dll') -Destination $taskHelperDir
    $taskSmoke=Join-Path $taskData 'ui-smoke.json'
    Remove-Item -LiteralPath $taskSmoke
    $taskParent=Start-Fixture (Join-Path $taskInstall 'shishi.exe') @('--ui-smoke','--root',$taskInstall)
    $taskPlan=Join-Path $taskHelperDir 'plan.json'
    @{parent=$taskParent.Id;root=$taskInstall;data=$taskData;executable=(Join-Path $taskInstall 'shishi.exe');installer=$taskPackage;args=@('--ui-smoke','--root',$taskInstall);package=@{version=$TargetVersion;tag="v$TargetVersion";url="https://github.com/LinHuiG/shi_shi/releases/download/v$TargetVersion/Shishi-Setup-$TargetVersion.exe";size=(Get-Item -LiteralPath $taskPackage).Length;digest=$taskDigest;published=4070908800000}} | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $taskPlan -Encoding utf8NoBOM
    $taskHelper=Start-Fixture (Join-Path $taskHelperDir 'shishi-update.exe') @('--apply-update',$taskPlan)
    if(!$taskHelper.WaitForExit(90000) -or $taskHelper.ExitCode -ne 0){throw ('Detached update failed: '+$taskHelper.StandardError.ReadToEnd())}
    if((Get-Content -LiteralPath (Join-Path $taskInstall 'install-info.txt') -TotalCount 1) -ne "Shishi $TargetVersion"){throw 'Installed version was not replaced'}
    if(!$taskMcp.WaitForExit(5000)){throw 'The installation MCP still holds the old executable'}
    if($taskOtherMcp.HasExited){throw 'An unrelated installation MCP was stopped'}
    $taskOtherMcp.StandardInput.WriteLine('{"jsonrpc":"2.0","id":2,"method":"ping"}')
    $taskOtherMcp.StandardInput.Flush()
    $taskPing=$taskOtherMcp.StandardOutput.ReadLineAsync()
    if(!$taskPing.Wait(5000) -or ($taskPing.Result | ConvertFrom-Json).id -ne 2){throw 'Other-installation MCP stopped serving requests'}
    $taskHistory=Get-Content -LiteralPath (Join-Path $taskCache 'history.json') -Raw | ConvertFrom-Json
    if($taskHistory.version -ne $TargetVersion){throw 'Successful history was not committed'}
    $taskLimit=[DateTime]::UtcNow.AddSeconds(45)
    while([DateTime]::UtcNow -lt $taskLimit) {
        if((Test-Path -LiteralPath $taskSmoke) -and (Get-Item -LiteralPath $taskSmoke).LastWriteTimeUtc -gt [DateTimeOffset]::FromUnixTimeMilliseconds($taskHistory.time).UtcDateTime){break}
        Start-Sleep -Milliseconds 100
    }
    if(!(Test-Path -LiteralPath $taskSmoke) -or !(Get-Content -LiteralPath $taskSmoke -Raw | ConvertFrom-Json).ok){throw 'Updated desktop did not restart successfully'}
    if((Get-Content -LiteralPath $taskSentinel -Raw) -ne 'Synthetic upgrade preservation'){throw 'Saved data changed'}
    $taskAfter=(Get-Content -LiteralPath (Join-Path $taskData 'state.json') -Raw | ConvertFrom-Json).config | ConvertTo-Json -Depth 50 -Compress
    if($taskAfter -ne $taskExpectedConfig){throw 'Saved configuration changed'}
    $taskResult=@{target_version=$TargetVersion;held_external_mcp='released';reopened_desktop='blocked without stopping desktop or helpers';unrelated_installation_mcp='still serving';external_owner='preserved';upgrade='passed';saved_data='preserved';saved_configuration='preserved';restart='passed';directory=$taskRoot}
    $taskResult | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $Report -Encoding utf8
    $taskResult | ConvertTo-Json -Depth 5
} finally {
    foreach($taskOwned in $taskProcesses) {
        if(!$taskOwned.HasExited){$taskOwned.Kill();$taskOwned.WaitForExit(10000) | Out-Null}
        $taskOwned.Dispose()
    }
}
