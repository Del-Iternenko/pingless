# Builds dist\OpenLag-Setup-<version>.exe.
# Needs vendor\xray\xray.exe and vendor\WinDivert-2.2.2-A (see README).
# Optional: vendor\setup\music.mp3 (installer background music, not in git).
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
Set-Location $root
$env:CARGO_TARGET_DIR = Join-Path $root 'target\dist'
$release = Join-Path $env:CARGO_TARGET_DIR 'release'

# Windows PowerShell turns cargo's progress on stderr into errors under 'Stop'.
function Invoke-Cargo {
    $ErrorActionPreference = 'Continue'
    & cargo @args 2>&1 | ForEach-Object { "$_" }
    if ($LASTEXITCODE) { throw "cargo $args failed" }
}

Write-Host '== building OpenLag'
Invoke-Cargo build --release -p openlag-gui

Write-Host '== staging payload'
$payload = Join-Path $env:CARGO_TARGET_DIR 'payload'
Remove-Item $payload -Recurse -Force -ErrorAction SilentlyContinue
$core = New-Item -ItemType Directory -Force (Join-Path $payload 'core')
$extras = New-Item -ItemType Directory -Force (Join-Path $payload 'extras')
New-Item -ItemType Directory -Force (Join-Path $core 'licenses'), (Join-Path $extras 'licenses') | Out-Null

Copy-Item (Join-Path $release 'openlag-gui.exe') (Join-Path $core 'OpenLag.exe')
Copy-Item LICENSE-MIT (Join-Path $core 'licenses\OpenLag-LICENSE.txt')

$wd = 'vendor\WinDivert-2.2.2-A'
Copy-Item 'vendor\xray\xray.exe' (Join-Path $extras 'openlag-xray.exe')
Copy-Item 'vendor\xray\LICENSE' (Join-Path $extras 'licenses\xray-LICENSE.txt')
Copy-Item "$wd\x64\WinDivert.dll", "$wd\x64\WinDivert64.sys" $extras
Copy-Item "$wd\LICENSE" (Join-Path $extras 'licenses\WinDivert-LICENSE.txt')

# WebView2 Evergreen bootstrapper; the installer runs it only if the runtime is missing.
$wv2 = 'vendor\setup\MicrosoftEdgeWebview2Setup.exe'
if (-not (Test-Path $wv2)) {
    New-Item -ItemType Directory -Force 'vendor\setup' | Out-Null
    Invoke-WebRequest 'https://go.microsoft.com/fwlink/p/?LinkId=2124703' -OutFile $wv2
}
Copy-Item $wv2 $extras

Write-Host '== building installer'
$env:OPENLAG_PAYLOAD = $payload
$env:OPENLAG_PAYLOAD_STAMP = (Get-Date).Ticks
Invoke-Cargo build --release -p openlag-setup

$version = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
New-Item -ItemType Directory -Force dist | Out-Null
$out = "dist\OpenLag-Setup-$version.exe"
Copy-Item (Join-Path $release 'openlag-setup.exe') $out -Force
Write-Host "== done: $out ($([math]::Round((Get-Item $out).Length / 1MB, 1)) MB)"
