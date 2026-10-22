# This script only downloads bytes. Rust validates metadata and SHA-256 before
# exposing installation; no downloaded command or script is evaluated here.
$ErrorActionPreference = 'Stop'
[Console]::InputEncoding = [Text.UTF8Encoding]::new($false)
[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)
Add-Type -AssemblyName System.Net.Http
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
function Get-UpdateMetadataFailure($Response) {
    $code = [int]$Response.StatusCode
    if ($code -ge 200 -and $code -lt 300 -or $code -eq 404) { return $null }
    $remaining = if ($Response.Headers.Contains('X-RateLimit-Remaining')) { @($Response.Headers.GetValues('X-RateLimit-Remaining'))[0] } else { '' }
    if ($code -eq 429 -or ($code -eq 403 -and ($remaining -eq '0' -or $Response.Headers.Contains('Retry-After')))) {
        $reset = 0L
        if ($Response.Headers.Contains('X-RateLimit-Reset')) {
            [long]::TryParse(@($Response.Headers.GetValues('X-RateLimit-Reset'))[0], [ref]$reset) | Out-Null
        }
        if ($reset -le 0 -or $reset -gt 4102444800) { $reset = 0L }
        $retryAt = $reset * 1000
        $retrySeconds = 0L
        if ($Response.Headers.Contains('Retry-After')) {
            [long]::TryParse(@($Response.Headers.GetValues('Retry-After'))[0], [ref]$retrySeconds) | Out-Null
            if ($retrySeconds -gt 0 -and $retrySeconds -le 86400) {
                $retryAt = [Math]::Max($retryAt, [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds() + $retrySeconds * 1000)
            }
        }
        return @{ error = 'rate_limited'; retry_at = $retryAt }
    }
    return @{ error = 'http_error'; status = $code }
}
$plan = [Console]::In.ReadToEnd() | ConvertFrom-Json
$handler = [System.Net.Http.HttpClientHandler]::new()
$handler.AllowAutoRedirect = $false
$client = [System.Net.Http.HttpClient]::new($handler)
$client.Timeout = [TimeSpan]::FromSeconds(600)
$client.DefaultRequestHeaders.UserAgent.ParseAdd('ShiShi-Updater/1.0')
$uri = [Uri]$plan.url
$allowed = @('github.com', 'release-assets.githubusercontent.com', 'objects.githubusercontent.com')
if ($plan.mode -eq 'metadata') {
    $uri = [Uri]'https://api.github.com/repos/LinHuiG/shi_shi/releases/latest'
    $allowed = @('api.github.com')
    $client.Timeout = [TimeSpan]::FromSeconds(30)
    $client.DefaultRequestHeaders.Accept.ParseAdd('application/vnd.github+json')
}
$stream = $null
$file = $null
$response = $null
try {
    for ($hop = 0; $hop -le 5; $hop++) {
        if ($uri.Scheme -ne 'https' -or $uri.Port -ne 443 -or $uri.UserInfo -or $uri.Host -notin $allowed) { throw 'Download location rejected' }
        $response = $client.GetAsync($uri, [System.Net.Http.HttpCompletionOption]::ResponseHeadersRead).GetAwaiter().GetResult()
        $code = [int]$response.StatusCode
        if ($plan.mode -eq 'metadata' -and $code -eq 404) { [Console]::WriteLine('{"not_found":true}'); exit 0 }
        if ($code -ge 300 -and $code -lt 400 -and $response.Headers.Location) {
            $uri = [Uri]::new($uri, $response.Headers.Location)
            $response.Dispose(); $response = $null
            continue
        }
        if ($plan.mode -eq 'metadata') {
            $failure = Get-UpdateMetadataFailure $response
            if ($failure) { [Console]::WriteLine(($failure | ConvertTo-Json -Compress)); exit 0 }
        }
        $response.EnsureSuccessStatusCode() | Out-Null
        if ($plan.mode -eq 'metadata') {
            $stream = $response.Content.ReadAsStreamAsync().GetAwaiter().GetResult()
            $body = [IO.MemoryStream]::new()
            try {
                $buffer = [byte[]]::new(16384)
                while (($count = $stream.ReadAsync($buffer, 0, $buffer.Length).GetAwaiter().GetResult()) -gt 0) {
                    if ($body.Length + $count -gt 1048576) { throw 'Metadata size exceeded' }
                    $body.Write($buffer, 0, $count)
                }
                [Console]::WriteLine([Text.Encoding]::UTF8.GetString($body.ToArray()))
            } finally { $body.Dispose() }
            exit 0
        }
        if ($response.Content.Headers.ContentLength -and $response.Content.Headers.ContentLength -ne [long]$plan.size) { throw 'Unexpected download size' }
        $stream = $response.Content.ReadAsStreamAsync().GetAwaiter().GetResult()
        $file = [IO.File]::Open($plan.path, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::Read)
        $buffer = [byte[]]::new(65536)
        $total = 0L; $last = -1
        $deadline = [DateTime]::UtcNow.AddSeconds(600)
        while (($count = $stream.ReadAsync($buffer, 0, $buffer.Length).GetAwaiter().GetResult()) -gt 0) {
            $total += $count
            if ($total -gt [long]$plan.size -or [DateTime]::UtcNow -gt $deadline) { throw 'Download limit exceeded' }
            $file.Write($buffer, 0, $count)
            $percent = [int][Math]::Min(99, [Math]::Floor($total * 100 / [long]$plan.size))
            if ($percent -ne $last) { [Console]::WriteLine("progress:$percent"); $last = $percent }
        }
        $file.Flush($true)
        if ($total -ne [long]$plan.size) { throw 'Incomplete download' }
        exit 0
    }
    throw 'Too many download redirects'
} catch {
    # Never print redirect query strings or upstream response bodies.
    [Console]::Error.WriteLine('Update download failed')
    exit 1
} finally {
    if ($file) { $file.Dispose() }
    if ($stream) { $stream.Dispose() }
    if ($response) { $response.Dispose() }
    $client.Dispose(); $handler.Dispose()
}
