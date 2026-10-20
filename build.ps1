param([string]$OutputDirectory="$PSScriptRoot\build",[string]$CargoHome=$env:CARGO_HOME,[string]$NsisPath)
$ErrorActionPreference='Stop'
$manifest=Get-Content -LiteralPath "$PSScriptRoot\Cargo.toml" -Raw
$version=[regex]::Match($manifest, '(?m)^version\s*=\s*"(\d+\.\d+\.\d+)"').Groups[1].Value
if(!$version){throw 'Missing package version'}
if($CargoHome){$env:CARGO_HOME=$CargoHome}
if(!$NsisPath){$NsisPath=(Get-Command makensis.exe -ErrorAction SilentlyContinue).Source}
if(!$NsisPath){throw 'Provide -NsisPath pointing to NSIS 3 makensis.exe'}
& cargo build --package shishi --release --locked --manifest-path "$PSScriptRoot\Cargo.toml"
if($LASTEXITCODE -ne 0){throw 'Rust build failed'}
$cache=if($CargoHome){$CargoHome}else{Join-Path $env:USERPROFILE '.cargo'}
$loader=Get-ChildItem -LiteralPath "$cache\registry\src" -Filter WebView2Loader.dll -Recurse | Where-Object {$_.FullName -match 'webview2-com-sys-[^\\]+\\x64\\'} | Select-Object -First 1
if(!$loader){throw 'x64 WebView2Loader.dll not found'}
Copy-Item -LiteralPath $loader.FullName -Destination "$PSScriptRoot\target\release\WebView2Loader.dll" -Force
New-Item -ItemType Directory -Force -Path $OutputDirectory | Out-Null
& $NsisPath /V2 /INPUTCHARSET UTF8 "/DVERSION=$version" "/DSOURCE=$PSScriptRoot" "/DOUTPUT=$OutputDirectory" "/DLOADER=$($loader.FullName)" "$PSScriptRoot\installer\setup.nsi"
if($LASTEXITCODE -ne 0){throw 'NSIS build failed'}
Get-FileHash -LiteralPath (Join-Path $OutputDirectory "Shishi-Setup-$version.exe") -Algorithm SHA256
