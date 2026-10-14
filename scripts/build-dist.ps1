# aipicdl distribution build (ASCII only).
#
# Produces dist\aipicdl-v<version>-windows-x64.zip containing the signed exe,
# README.md, LICENSE and THIRD-PARTY-NOTICES.txt, plus a .sha256 file.
#
# Steps:
#   1. cargo fmt --check / clippy -D warnings / tests
#   2. cargo clean -p aipicdl --release, then cargo build --release
#      (always recompile the app from current source)
#   3. sign + verify the exe (skip with -NoSign; unsigned zips are for testing only)
#   4. generate THIRD-PARTY-NOTICES.txt with cargo-about
#   5. zip + SHA-256
#
# Usage (from anywhere):
#   PS> scripts\build-dist.ps1
#   PS> scripts\build-dist.ps1 -NoSign
#
# Requirements: Rust toolchain, Windows SDK (rc.exe / signtool.exe),
# cargo-about (`cargo install --locked cargo-about --features cli`),
# and SimplySign Desktop logged in for signing. See docs/release.md.

[CmdletBinding()]
param(
    [switch] $NoSign,
    # Use only when the identical source tree already passed the checks.
    [switch] $SkipChecks
)

$ErrorActionPreference = 'Stop'
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
Set-Location $repoRoot
. (Join-Path $PSScriptRoot 'sign-files.ps1')

function Invoke-Step {
    param([string] $Name, [scriptblock] $Block)
    Write-Host ""
    Write-Host "==> $Name" -ForegroundColor Cyan
    # cargo writes progress to stderr; under 'Stop' PowerShell 5.1 would treat
    # that as a terminating error, so judge success by the exit code instead.
    $prev = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try { & $Block } finally { $ErrorActionPreference = $prev }
    if ($LASTEXITCODE -ne 0) { throw "[build-dist] step failed: $Name (exit $LASTEXITCODE)" }
}

$cargoToml = Get-Content -Raw -LiteralPath (Join-Path $repoRoot 'Cargo.toml')
if ($cargoToml -notmatch '(?m)^version\s*=\s*"([^"]+)"') { throw "[build-dist] version not found in Cargo.toml" }
$version = $Matches[1]
$name = "aipicdl-v$version-windows-x64"
Write-Host "[build-dist] version $version"

$dirty = git status --porcelain
if ($dirty) { Write-Warning "[build-dist] working tree has uncommitted changes" }

if (-not (Get-Command cargo-about -ErrorAction SilentlyContinue)) {
    throw "[build-dist] cargo-about not found. Install: cargo install --locked cargo-about --features cli"
}
if (-not $NoSign) { Assert-AipicdlSignReady }

if (-not $SkipChecks) {
    Invoke-Step 'cargo fmt --check' { cargo fmt --all --check }
    Invoke-Step 'cargo clippy' { cargo clippy --locked --all-targets -- -D warnings }
    Invoke-Step 'cargo test' { cargo test --locked }
}
Invoke-Step 'cargo clean (app only)' { cargo clean --release -p aipicdl }
Invoke-Step 'cargo build --release' { cargo build --locked --release }

$targetDir = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $repoRoot 'target' }
if (-not [System.IO.Path]::IsPathRooted($targetDir)) { $targetDir = Join-Path $repoRoot $targetDir }
$exe = Join-Path $targetDir 'release\aipicdl.exe'
if (-not $NoSign) {
    Write-Host ""
    Write-Host "==> sign" -ForegroundColor Cyan
    Invoke-AipicdlSign -Files @($exe) -Verify
} else {
    Write-Warning "[build-dist] -NoSign: the exe is NOT signed. Do not publish this zip."
}

$dist = Join-Path $repoRoot 'dist'
$stage = Join-Path $dist $name
if (Test-Path -LiteralPath $stage) { Remove-Item -LiteralPath $stage -Recurse -Force }
New-Item -ItemType Directory -Force -Path $stage | Out-Null

Invoke-Step 'cargo about generate' {
    cargo about generate --locked about.hbs -o (Join-Path $stage 'THIRD-PARTY-NOTICES.txt')
}
Copy-Item -LiteralPath $exe -Destination $stage
Copy-Item -LiteralPath (Join-Path $repoRoot 'README.md') -Destination $stage
Copy-Item -LiteralPath (Join-Path $repoRoot 'LICENSE') -Destination $stage

$zip = Join-Path $dist "$name.zip"
if (Test-Path -LiteralPath $zip) { Remove-Item -LiteralPath $zip -Force }
Compress-Archive -Path (Join-Path $stage '*') -DestinationPath $zip
$hash = (Get-FileHash -Algorithm SHA256 -LiteralPath $zip).Hash.ToLowerInvariant()
# LF line ending so that `sha256sum -c` also works on non-Windows systems.
[System.IO.File]::WriteAllText("$zip.sha256", "$hash  $name.zip`n", (New-Object System.Text.ASCIIEncoding))

Write-Host ""
Write-Host "[build-dist] done: $zip" -ForegroundColor Green
Write-Host "[build-dist] sha256: $hash"
if (-not $NoSign) {
    Write-Host "[build-dist] next: git tag v$version; git push origin v$version"
    Write-Host "[build-dist]       gh release create v$version `"$zip`" `"$zip.sha256`" --title v$version --notes-file <notes.md>"
}
