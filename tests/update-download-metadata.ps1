# Load only the pure classifier AST; this test never evaluates the downloader.
$ErrorActionPreference='Stop'
Add-Type -AssemblyName System.Net.Http
$taskTokens=$null;$taskErrors=$null
$taskAst=[Management.Automation.Language.Parser]::ParseFile((Join-Path $PSScriptRoot '../assets/update-download.ps1'),[ref]$taskTokens,[ref]$taskErrors)
if($taskErrors.Count){throw 'Downloader PowerShell syntax errors'}
$taskFunction=$taskAst.Find({param($node)$node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq 'Get-UpdateMetadataFailure'},$true)
if(!$taskFunction){throw 'Metadata classifier missing'}
# Redirects must pass through the existing host/hop checks before classification.
$taskRedirect=$taskAst.Extent.Text.IndexOf('if ($code -ge 300 -and $code -lt 400 -and $response.Headers.Location)')
$taskClassification=$taskAst.Extent.Text.IndexOf('$failure = Get-UpdateMetadataFailure $response')
if($taskRedirect -lt 0 -or $taskClassification -le $taskRedirect){throw 'Metadata classification bypasses redirect handling'}
. ([ScriptBlock]::Create($taskFunction.Extent.Text))
function Read-FixtureFailure([int]$Status,[string]$Remaining='',[string]$Reset='',[string]$RetryAfter='') {
 # Windows PowerShell's .NET enum may predate HTTP 429, although responses support it.
 $taskCode=[Enum]::ToObject([Net.HttpStatusCode],$Status)
 $taskResponse=[Net.Http.HttpResponseMessage]::new($taskCode)
 try {
  if($Remaining){$taskResponse.Headers.TryAddWithoutValidation('X-RateLimit-Remaining',$Remaining) | Out-Null}
  if($Reset){$taskResponse.Headers.TryAddWithoutValidation('X-RateLimit-Reset',$Reset) | Out-Null}
  if($RetryAfter){$taskResponse.Headers.TryAddWithoutValidation('Retry-After',$RetryAfter) | Out-Null}
  Get-UpdateMetadataFailure $taskResponse
 }finally{$taskResponse.Dispose()}
}
$taskLimited=Read-FixtureFailure 403 '0' '1700000000'
if($taskLimited.error -ne 'rate_limited' -or $taskLimited.retry_at -ne 1700000000000){throw 'Rate limit reset was lost'}
if((Read-FixtureFailure 403 '1').error -ne 'http_error'){throw 'Unrelated forbidden response mislabeled as quota'}
if((Read-FixtureFailure 429).error -ne 'rate_limited'){throw 'HTTP 429 was not classified'}
$taskNow=[DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds();$taskSecondary=Read-FixtureFailure 403 '1' '' '90'
if($taskSecondary.error -ne 'rate_limited' -or $taskSecondary.retry_at -lt $taskNow+90000 -or $taskSecondary.retry_at -gt $taskNow+91000){throw 'Secondary limit retry deadline was lost'}
if((Read-FixtureFailure 503).status -ne 503){throw 'HTTP failure status was lost'}
if((Read-FixtureFailure 200) -or (Read-FixtureFailure 404)){throw 'Success or not-found became a failure'}
foreach($taskReset in @('-1','9999999999999999999999999','9999999999','invalid')){if((Read-FixtureFailure 403 '0' $taskReset).retry_at -ne 0){throw 'Invalid reset timestamp accepted'}}
Write-Output 'PASS offline metadata classification: quota, reset time, forbidden, unavailable, not-found and invalid headers.'
