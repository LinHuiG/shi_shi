# Load only the pure argv classifier; never run the installer or stop a process.
$ErrorActionPreference='Stop'
$taskTokens=$null;$taskErrors=$null
$taskAst=[Management.Automation.Language.Parser]::ParseFile((Join-Path $PSScriptRoot '../installer/process.ps1'),[ref]$taskTokens,[ref]$taskErrors)
if($taskErrors.Count){throw 'Installer process script syntax errors'}
$taskFunction=$taskAst.Find({param($node)$node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq 'Get-ShishiHelperRole'},$true)
if(!$taskFunction){throw 'Helper classifier missing'}
. ([ScriptBlock]::Create($taskFunction.Extent.Text))
foreach($taskMode in @('--context-mcp-root','--context-mcp','--analysis-mcp','--analysis-submit-mcp')) {
    if((Get-ShishiHelperRole ('"D:\Fixture App\shishi.exe" '+$taskMode+' "D:\Data With Spaces\input.json"')) -cne $taskMode){throw 'Known helper mode was not recognized'}
    if((Get-ShishiHelperRole ('"D:\Fixture App\shishi.exe" "'+$taskMode+'" "D:\Data\input.json"')) -cne $taskMode){throw 'Quoted argv was not recognized'}
}
foreach($taskLine in @('', '"D:\Fixture App\shishi.exe"', '"D:\Fixture App\shishi.exe" --ui-smoke', '"D:\Fixture App\shishi.exe" --root "D:\Data --context-mcp-root folder"', '"D:\Fixture App\shishi.exe" --context-mcp-root-other x', '"D:\Fixture App\shishi.exe" --CONTEXT-MCP-ROOT x')) {
    if(Get-ShishiHelperRole $taskLine){throw 'A desktop, unrelated mode or quoted path was treated as an MCP helper'}
}
foreach($taskMode in @('--data-transfer','--installer-confirm','--native-dialog-smoke','--live-ai-regression','--desktop-bridge-check')) {
    if(Get-ShishiHelperRole ('"D:\Fixture App\shishi.exe" '+$taskMode+' value --context-mcp-root "D:\Fixture"')){throw 'A higher-priority main entry point was treated as an MCP helper'}
}
Write-Output 'PASS helper argv classification: exact modes, quoted arguments, desktop, unknown mode, case and flag-like path text.'
