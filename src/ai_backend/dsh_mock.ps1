# Synthetic ACP runtime for offline tests. No network, model, or user profile.
$ErrorActionPreference = 'Stop'
[Console]::InputEncoding = [Text.UTF8Encoding]::new($false)
[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)
$fixtureHome = $env:DSH_HOME
$scenarioPath = Join-Path $fixtureHome 'scenario.txt'
$scenario = [IO.File]::ReadAllText($scenarioPath)
$log = Join-Path $fixtureHome 'runtime.log'
function Record([string] $value) { [IO.File]::AppendAllText($log, $value + "`n") }
function Reply($id, $value) {
    [Console]::WriteLine((@{jsonrpc='2.0'; id=$id; result=$value} | ConvertTo-Json -Compress -Depth 64))
}
Record "pid|$PID"
$sessions = @{}
$sequence = 0
$options = @(@{id='model'; name='Model'; type='select'; currentValue='["fixture","model"]'; options=@(@{value='["fixture","model"]'; name='Synthetic model'})})
while ($null -ne ($line = [Console]::ReadLine())) {
    $request = $line | ConvertFrom-Json
    if ($null -eq $request.id) { continue }
    switch ($request.method) {
        'initialize' {
            Record 'initialize'
            $capabilities = @{}
            if ($scenario -ne 'no-close') { $capabilities.close = @{} }
            Reply $request.id @{protocolVersion=1; agentInfo=@{name='synthetic'; version='0'}; agentCapabilities=@{sessionCapabilities=$capabilities}}
        }
        'session/new' {
            $sequence++
            $sessionId = "fixture-$sequence"
            $submit = $request.params.mcpServers | Where-Object { $_.name -eq 'shishi_submit' }
            $sessions[$sessionId] = [string] $submit.args[1]
            Record "new|$sessionId|$($sessions[$sessionId])"
            Reply $request.id @{sessionId=$sessionId; configOptions=$options}
        }
        'session/set_config_option' { Reply $request.id @{configOptions=$options} }
        'session/prompt' {
            $sessionId = $request.params.sessionId
            Record "prompt|$sessionId"
            if ($scenario -eq 'wait') { Start-Sleep -Seconds 30 }
            $ticketPath = $sessions[$sessionId]
            $ticket = [IO.File]::ReadAllText($ticketPath) | ConvertFrom-Json
            $value = @{answer='synthetic'}
            if ($scenario -eq 'invalid') { $value.answer = 7 }
            $accepted = @{version=1; request_id=$ticket.request_id; kind=$ticket.kind; schema_hash=$ticket.schema_hash; value=$value}
            $resultPath = Join-Path ([IO.Path]::GetDirectoryName($ticketPath)) 'result.json'
            [IO.File]::WriteAllText($resultPath, ($accepted | ConvertTo-Json -Compress -Depth 64))

            Reply $request.id @{stopReason='end_turn'; usage=@{inputTokens=10; outputTokens=5; totalTokens=15}}
        }
        'session/close' {
            Record "close|$($request.params.sessionId)"
            if ($scenario -eq 'close-failed') {
                [Console]::WriteLine((@{jsonrpc='2.0'; id=$request.id; error=@{code=-32603; message='synthetic close failure'}} | ConvertTo-Json -Compress))
            } else {
                $sessions.Remove($request.params.sessionId)
                Reply $request.id @{}
            }
        }
        default { throw "Unexpected synthetic request: $($request.method)" }
    }
}
