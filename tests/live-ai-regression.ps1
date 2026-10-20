param(
    [Parameter(Mandatory=$true)][string]$ConfigRoot,
    [switch]$AllowLiveAi,
    [ValidateSet('codex','dsh','api')][string]$OnlySource,
    [string]$OnlyScenario,
    [string]$OutputDirectory,
    [string]$Binary
)
$ErrorActionPreference='Stop'
if(!$AllowLiveAi){throw '真实模型回归需要用户明确授权，再传入 -AllowLiveAi'}
$repo=Split-Path -Parent $PSScriptRoot
if(!$OutputDirectory){$OutputDirectory=Join-Path ([IO.Path]::GetTempPath()) ('shishi-live-matrix-'+[guid]::NewGuid().ToString('N'))}
if(!$Binary){$Binary=Join-Path $repo 'target/release/shishi.exe'}
$binary=$Binary
if(!(Test-Path -LiteralPath $binary -PathType Leaf)){throw '请先构建 release 主程序'}
$testArgs=@('--live-ai-regression','--allow-live-ai','--config-root',$ConfigRoot,'--root',$OutputDirectory)
if($OnlySource){$testArgs+=@('--only-source',$OnlySource)}
if($OnlyScenario){$testArgs+=@('--only-scenario',$OnlyScenario)}
$startInfo=[Diagnostics.ProcessStartInfo]::new($binary)
$startInfo.UseShellExecute=$false
$startInfo.CreateNoWindow=$true
foreach($testArg in $testArgs){$startInfo.ArgumentList.Add($testArg)}
$testProcess=[Diagnostics.Process]::Start($startInfo)
$testProcess.WaitForExit()
$resultCode=$testProcess.ExitCode
Write-Output ('Report: '+(Join-Path $OutputDirectory 'report.json'))
if($resultCode -ne 0){throw '真实接入矩阵存在失败，请查看隔离报告'}
