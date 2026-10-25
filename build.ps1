param([string]$OutputDirectory="$PSScriptRoot\build",[string]$CargoHome=$env:CARGO_HOME,[string]$NsisPath,[string]$PublishedAt)
$ErrorActionPreference='Stop'
$manifest=Get-Content -LiteralPath "$PSScriptRoot\Cargo.toml" -Raw
$version=[regex]::Match($manifest, '(?m)^version\s*=\s*"(\d+\.\d+\.\d+)"').Groups[1].Value
if(!$version){throw 'Missing package version'}
if($CargoHome){$env:CARGO_HOME=$CargoHome}
if(!$NsisPath){$NsisPath=(Get-Command makensis.exe -ErrorAction SilentlyContinue).Source}
if(!$NsisPath){throw 'Provide -NsisPath pointing to NSIS 3 makensis.exe'}
$previousBuildNonce=$env:SHISHI_PACKAGE_BUILD_NONCE
$previousPublishedAt=$env:SHISHI_PACKAGE_PUBLISHED_AT
$previousEncodedRustFlags=$env:CARGO_ENCODED_RUSTFLAGS
try {
 # Panic locations in shipped binaries must not disclose the build account or checkout.
 $buildRustFlags=@()
 if($previousEncodedRustFlags){$buildRustFlags=@($previousEncodedRustFlags -split [char]0x1f)}
 elseif($env:RUSTFLAGS){$buildRustFlags=@($env:RUSTFLAGS -split '\s+' | Where-Object {$_})}
 $buildRustFlags+="--remap-path-prefix=$PSScriptRoot=/shishi-source"
 if($env:USERPROFILE){$buildRustFlags+="--remap-path-prefix=$env:USERPROFILE=/build-user"}
 if($CargoHome){$buildRustFlags+="--remap-path-prefix=$CargoHome=/cargo-home"}
 $env:CARGO_ENCODED_RUSTFLAGS=$buildRustFlags -join [char]0x1f
 $env:SHISHI_PACKAGE_BUILD_NONCE=[guid]::NewGuid().ToString('N')
 if($PublishedAt){$env:SHISHI_PACKAGE_PUBLISHED_AT=([DateTimeOffset]::ParseExact($PublishedAt,"yyyy-MM-dd'T'HH:mm:sszzz",[Globalization.CultureInfo]::InvariantCulture).ToUnixTimeSeconds()).ToString()}
 else{Remove-Item Env:SHISHI_PACKAGE_PUBLISHED_AT -ErrorAction SilentlyContinue}
 & cargo build --package shishi --release --locked --manifest-path "$PSScriptRoot\Cargo.toml"
 if($LASTEXITCODE -ne 0){throw 'Rust build failed'}
} finally {
 if($null -eq $previousBuildNonce){Remove-Item Env:SHISHI_PACKAGE_BUILD_NONCE -ErrorAction SilentlyContinue}
 else{$env:SHISHI_PACKAGE_BUILD_NONCE=$previousBuildNonce}
 if($null -eq $previousPublishedAt){Remove-Item Env:SHISHI_PACKAGE_PUBLISHED_AT -ErrorAction SilentlyContinue}
 else{$env:SHISHI_PACKAGE_PUBLISHED_AT=$previousPublishedAt}
 if($null -eq $previousEncodedRustFlags){Remove-Item Env:CARGO_ENCODED_RUSTFLAGS -ErrorAction SilentlyContinue}
 else{$env:CARGO_ENCODED_RUSTFLAGS=$previousEncodedRustFlags}
}
$hostTriple=((& rustc -vV | Select-String '^host: ').Line).Substring(6)
$metadataText=& cargo metadata --locked --offline --format-version 1 --filter-platform $hostTriple --manifest-path "$PSScriptRoot\Cargo.toml"
if($LASTEXITCODE -ne 0){throw 'Cannot resolve locked build dependencies'}
$metadata=$metadataText | ConvertFrom-Json
$loaders=@($metadata.packages | Where-Object { $_.name -eq 'webview2-com-sys' })
if($loaders.Count -ne 1){throw 'Expected one resolved webview2-com-sys dependency'}
$loaderPackage=$loaders[0]
$loaderPath=Join-Path (Split-Path -LiteralPath $loaderPackage.manifest_path) 'x64/WebView2Loader.dll'
if(!(Test-Path -LiteralPath $loaderPath -PathType Leaf)){throw 'Resolved x64 WebView2Loader.dll not found'}
Copy-Item -LiteralPath $loaderPath -Destination "$PSScriptRoot\target\release\WebView2Loader.dll" -Force
New-Item -ItemType Directory -Force -Path $OutputDirectory | Out-Null
& $NsisPath /V2 /INPUTCHARSET UTF8 "/DVERSION=$version" "/DSOURCE=$PSScriptRoot" "/DOUTPUT=$OutputDirectory" "/DLOADER=$loaderPath" "$PSScriptRoot\installer\setup.nsi"
if($LASTEXITCODE -ne 0){throw 'NSIS build failed'}
$installer=Join-Path $OutputDirectory "Shishi-Setup-$version.exe"
$installerHash=Get-FileHash -LiteralPath $installer -Algorithm SHA256
$uiRoot=Join-Path $PSScriptRoot 'assets/ui'
$styles=[Text.StringBuilder]::new();$scripts=[Text.StringBuilder]::new()
foreach($line in Get-Content -LiteralPath (Join-Path $uiRoot 'manifest.txt')) {
 $line=$line.Trim()
 if(!$line -or $line.StartsWith('#')){continue}
 $kind,$relative=$line -split ' ',2
 $content=[IO.File]::ReadAllText((Join-Path $uiRoot $relative.Trim())).Replace("`r`n","`n")
 $builder=if($kind -eq 'style'){$styles}elseif($kind -eq 'script'){$scripts}else{throw 'Unknown UI manifest kind'}
 [void]$builder.Append($content)
 if(!$content.EndsWith("`n")){[void]$builder.Append("`n")}
}
$ui=[IO.File]::ReadAllText((Join-Path $uiRoot 'index.html')).Replace("`r`n","`n").Replace('<!--STYLES-->',"<style>`n$styles</style>").Replace('/*SCRIPTS*/',$scripts.ToString().TrimEnd())
[ordered]@{
 version=$version
 published_at=$PublishedAt
 installer_bytes=(Get-Item -LiteralPath $installer).Length
 executable_bytes=(Get-Item -LiteralPath "$PSScriptRoot/target/release/shishi.exe").Length
 embedded_ui_bytes=[Text.Encoding]::UTF8.GetByteCount($ui)
 loader_version=$loaderPackage.version
 loader_sha256=(Get-FileHash -LiteralPath $loaderPath -Algorithm SHA256).Hash
 installer_sha256=$installerHash.Hash
} | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $OutputDirectory "Shishi-Build-Metrics-$version.json") -Encoding UTF8
$installerHash
