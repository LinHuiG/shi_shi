# Built-in Windows ZIP reader. Paths arrive as data, never interpolated into code.
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)
Add-Type -AssemblyName System.IO.Compression
$spec = $env:SHISHI_PLUGIN_PACKAGE | ConvertFrom-Json
# Rust canonical paths use the Win32 extended prefix; Windows PowerShell's .NET
# Framework file APIs may reject it under legacy path handling (including Win10).
function Native-Path([string]$value) {
    if ($value.StartsWith('\\?\UNC\')) { return '\\'+$value.Substring(8) }
    if ($value.StartsWith('\\?\')) { return $value.Substring(4) }
    return $value
}
$destination = [IO.Path]::GetFullPath((Native-Path $spec.destination))
$limit = 256MB
$archive = $null
$stream = $null
try {
    if (![IO.Directory]::Exists($destination) -or [IO.Directory]::GetFileSystemEntries($destination).Length -ne 0) { throw 'Extraction directory must be new and empty' }
    $stream = [IO.File]::Open((Native-Path $spec.path), [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
    if ($stream.Length -gt $limit) { throw 'Plugin ZIP exceeds 256 MiB' }
    $archive = [IO.Compression.ZipArchive]::new($stream, [IO.Compression.ZipArchiveMode]::Read)
    if ($archive.Entries.Count -gt 256) { throw 'Plugin ZIP has more than 256 entries' }
    $seen = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    $manifests = [Collections.Generic.List[string]]::new()
    $manifestNames = [Collections.Generic.List[string]]::new()
    [long]$total = 0
    foreach ($entry in $archive.Entries) {
        $name = $entry.FullName.TrimEnd('/')
        if (!$name -or $name.Contains('\') -or $name.StartsWith('/') -or $name.Length -gt 512) { throw 'Invalid ZIP path' }
        foreach ($part in $name.Split('/')) {
            if (!$part -or $part -eq '.' -or $part -eq '..' -or $part.TrimEnd(' ','.') -ne $part -or $part.IndexOfAny([IO.Path]::GetInvalidFileNameChars()) -ge 0 -or $part -match '^(CON|PRN|AUX|NUL|COM[0-9]|LPT[0-9])(?:\.|$)') { throw 'Unsafe ZIP path' }
        }
        if (!$seen.Add($name)) { throw 'Duplicate ZIP path' }
        if (($entry.ExternalAttributes -shr 16 -band 0xF000) -eq 0xA000) { throw 'ZIP links are not supported' }
        $target = [IO.Path]::GetFullPath([IO.Path]::Combine($destination,$name))
        if (!$target.StartsWith($destination.TrimEnd('\')+'\',[StringComparison]::OrdinalIgnoreCase)) { throw 'ZIP path escapes destination' }
        $total += $entry.Length
        if ($entry.Length -gt $limit -or $total -gt $limit) { throw 'Expanded plugin exceeds 256 MiB' }
        if (!$entry.FullName.EndsWith('/') -and $entry.Name -ieq 'plugin.json') {
            if ($entry.Length -gt 128KB) { throw 'Plugin manifest exceeds 128 KiB' }
            $manifests.Add($target)
            $manifestNames.Add($name)
        }
    }
    if ($manifests.Count -ne 1) { throw 'ZIP 中需要且只能包含一个 plugin.json。请安装编译后的插件 ZIP 包，源码 ZIP 不能直接安装。' }
    if ($manifestNames[0].Split('/').Length -gt 2) { throw '该 ZIP 不是可安装的插件包：plugin.json 需要位于 ZIP 根目录或第一层插件目录。若选择了源码 ZIP，请改选包含 DLL 和依赖文件的插件安装包。' }
    [long]$written = 0
    $buffer = New-Object byte[] 65536
    foreach ($entry in $archive.Entries) {
        $target = [IO.Path]::Combine($destination,$entry.FullName.TrimEnd('/'))
        if ($entry.FullName.EndsWith('/')) { [IO.Directory]::CreateDirectory($target) | Out-Null; continue }
        [IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($target)) | Out-Null
        $inputStream = $null; $outputStream = $null
        try {
            $inputStream = $entry.Open()
            $outputStream = [IO.File]::Open($target,[IO.FileMode]::CreateNew,[IO.FileAccess]::Write,[IO.FileShare]::None)
            [long]$entryWritten = 0
            while (($count = $inputStream.Read($buffer,0,$buffer.Length)) -gt 0) {
                $entryWritten += $count; $written += $count
                if ($written -gt $limit -or $entryWritten -gt $entry.Length) { throw 'Invalid expanded ZIP size' }
                $outputStream.Write($buffer,0,$count)
            }
            if ($entryWritten -ne $entry.Length) { throw 'Truncated ZIP entry' }
        } finally {
            if ($null -ne $outputStream) { $outputStream.Dispose() }
            if ($null -ne $inputStream) { $inputStream.Dispose() }
        }
    }
    @{manifest=$manifests[0]} | ConvertTo-Json -Compress
} catch {
    [Console]::Error.WriteLine($_.Exception.Message)
    exit 1
} finally {
    if ($null -ne $archive) { $archive.Dispose() }
    if ($null -ne $stream) { $stream.Dispose() }
}
