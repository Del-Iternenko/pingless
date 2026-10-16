# Builds the installers into dist\:
#   Pingless-Setup-<version>.exe        public, no music inside (plays music.mp3 found
#                                      next to the exe, if any)
#   Pingless-Setup-<version>-Music.exe  with vendor\setup\music.mp3 inside; personal use
#                                      only, never publish it (the track is not ours)
# Needs vendor\xray\xray.exe and vendor\WinDivert-2.2.2-A (see README).
param([ValidateSet('both', 'public', 'music')] [string] $Variant = 'both')
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

Write-Host '== building Pingless'
Invoke-Cargo build --release -p openlag-gui

# Static Linux builds of the relay, uploaded by "install relay on my server".
# rust-lld links musl without a cross toolchain; blake3 is built without C (pure).
Write-Host '== building Linux relays'
$env:CARGO_FEATURE_PURE = '1'
foreach ($t in 'x86_64', 'aarch64') {
    $triple = "$t-unknown-linux-musl"
    Set-Item "env:CARGO_TARGET_$($triple.ToUpper().Replace('-', '_'))_LINKER" 'rust-lld'
    Invoke-Cargo build --release -p openlag-relay --target $triple
}
Remove-Item env:CARGO_FEATURE_PURE

Write-Host '== staging payload'
$payload = Join-Path $env:CARGO_TARGET_DIR 'payload'
Remove-Item $payload -Recurse -Force -ErrorAction SilentlyContinue
$core = New-Item -ItemType Directory -Force (Join-Path $payload 'core')
$extras = New-Item -ItemType Directory -Force (Join-Path $payload 'extras')
New-Item -ItemType Directory -Force (Join-Path $core 'licenses'), (Join-Path $extras 'licenses') | Out-Null

Copy-Item (Join-Path $release 'openlag-gui.exe') (Join-Path $core 'Pingless.exe')
Copy-Item LICENSE-MIT (Join-Path $core 'licenses\Pingless-LICENSE.txt')
Copy-Item LICENSE-APACHE (Join-Path $core 'licenses\Pingless-LICENSE-APACHE.txt')
$relayDir = New-Item -ItemType Directory -Force (Join-Path $core 'relay')
foreach ($t in 'x86_64', 'aarch64') {
    Copy-Item (Join-Path $env:CARGO_TARGET_DIR "$t-unknown-linux-musl\release\openlag-relay") (Join-Path $relayDir "openlag-relay-$t")
}

$wd = 'vendor\WinDivert-2.2.2-A'
Copy-Item 'vendor\xray\xray.exe' (Join-Path $extras 'pingless-xray.exe')
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

$version = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
New-Item -ItemType Directory -Force dist | Out-Null
$env:OPENLAG_PAYLOAD = $payload
$env:OPENLAG_PAYLOAD_STAMP = (Get-Date).Ticks

function Build-Setup([string] $music, [string] $name) {
    Write-Host "== building $name"
    $env:OPENLAG_SETUP_MUSIC = $music
    Invoke-Cargo build --release -p openlag-setup
    $out = "dist\$name"
    Copy-Item (Join-Path $release 'openlag-setup.exe') $out -Force
    Write-Host "== done: $out ($([math]::Round((Get-Item $out).Length / 1MB, 1)) MB)"
}

if ($Variant -in 'both', 'public') {
    Build-Setup 'none' "Pingless-Setup-$version.exe"
}
if ($Variant -in 'both', 'music') {
    $track = Join-Path $root 'vendor\setup\music.mp3'
    if (Test-Path $track) { Build-Setup $track "Pingless-Setup-$version-Music.exe" }
    else { Write-Host "== skipping music build: $track not found" }
}
