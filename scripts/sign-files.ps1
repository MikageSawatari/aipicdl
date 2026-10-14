# Code-signing helper for aipicdl distribution builds (ASCII only).
#
# Dot-source this file to import the signing functions:
#     . (Join-Path $PSScriptRoot 'sign-files.ps1')
#     Assert-AipicdlSignReady
#     Invoke-AipicdlSign -Files @('target\release\aipicdl.exe') -Verify
#
# Signing uses the Certum Open Source Code Signing certificate exposed through
# SimplySign Desktop (the cloud key appears as a virtual smart card in the
# CurrentUser certificate store). SimplySign Desktop MUST be running and logged
# in for a signed build. See docs/release.md.
#
# Configuration via environment (all optional):
#   AIPICDL_SIGN_SHA1     - certificate thumbprint (overrides subject-name selection).
#   AIPICDL_SIGN_SUBJECT  - certificate subject-name substring (default below).
#   AIPICDL_SIGN_TS       - RFC3161 timestamp URL (default http://time.certum.pl).
#   AIPICDL_SIGNTOOL      - explicit path to signtool.exe (else auto-detected).
#
# ASCII-only: PowerShell 5.1 reads BOM-less .ps1 as the system ANSI codepage.

$script:AipicdlSignDefaultSubject = 'Open Source Developer Taku Sano'
$script:AipicdlSignDefaultTimestamp = 'http://time.certum.pl'

function Get-AipicdlSignTool {
    if ($env:AIPICDL_SIGNTOOL) {
        if (Test-Path $env:AIPICDL_SIGNTOOL) { return $env:AIPICDL_SIGNTOOL }
        throw "[sign] AIPICDL_SIGNTOOL is set but does not exist: $($env:AIPICDL_SIGNTOOL)"
    }
    $roots = @(
        'C:\Program Files (x86)\Windows Kits\10\bin',
        'C:\Program Files\Windows Kits\10\bin'
    )
    $found = @()
    foreach ($r in $roots) {
        if (-not (Test-Path $r)) { continue }
        $found += Get-ChildItem -Path $r -Recurse -Filter signtool.exe -ErrorAction SilentlyContinue |
            Where-Object { $_.FullName -like '*\x64\signtool.exe' }
    }
    if ($found.Count -eq 0) {
        throw "[sign] signtool.exe (x64) not found under Windows Kits. Install the Windows SDK, or set AIPICDL_SIGNTOOL."
    }
    # Highest SDK build wins (version directories sort lexically).
    return ($found | Sort-Object -Property FullName -Descending | Select-Object -First 1).FullName
}

function Get-AipicdlSignSelector {
    if ($env:AIPICDL_SIGN_SHA1) { return @('/sha1', $env:AIPICDL_SIGN_SHA1) }
    $subject = if ($env:AIPICDL_SIGN_SUBJECT) { $env:AIPICDL_SIGN_SUBJECT } else { $script:AipicdlSignDefaultSubject }
    return @('/n', $subject)
}

function Assert-AipicdlSignReady {
    $selector = Get-AipicdlSignSelector
    if ($selector[0] -eq '/sha1') {
        $cert = Get-ChildItem Cert:\CurrentUser\My -ErrorAction SilentlyContinue |
            Where-Object { $_.Thumbprint -eq $selector[1] }
    } else {
        $cert = Get-ChildItem Cert:\CurrentUser\My -CodeSigningCert -ErrorAction SilentlyContinue |
            Where-Object { $_.Subject -like ("*{0}*" -f $selector[1]) }
    }
    if (-not $cert) {
        throw ("[sign] signing certificate not found in CurrentUser\My (selector: {0}). Start SimplySign Desktop and log in." -f ($selector -join ' '))
    }
    Write-Host ("[sign] certificate ready: {0}" -f (@($cert)[0].Subject))
}

function Invoke-AipicdlSign {
    param(
        [Parameter(Mandatory = $true)] [string[]] $Files,
        [switch] $Verify
    )
    $signtool = Get-AipicdlSignTool
    $selector = Get-AipicdlSignSelector
    $ts = if ($env:AIPICDL_SIGN_TS) { $env:AIPICDL_SIGN_TS } else { $script:AipicdlSignDefaultTimestamp }

    foreach ($f in $Files) {
        if (-not (Test-Path -LiteralPath $f -PathType Leaf)) { throw "[sign] file to sign not found: $f" }
        $signArgs = @('sign') + $selector + @('/fd', 'sha256', '/tr', $ts, '/td', 'sha256', $f)
        $ok = $false
        # The timestamp server is remote; retry to ride out transient failures.
        for ($attempt = 1; $attempt -le 3; $attempt++) {
            $prev = $ErrorActionPreference
            $ErrorActionPreference = 'Continue'
            try { & $signtool @signArgs } finally { $ErrorActionPreference = $prev }
            if ($LASTEXITCODE -eq 0) { $ok = $true; break }
            Write-Warning ("[sign] attempt {0}/3 failed for {1} (exit {2}); retrying..." -f $attempt, $f, $LASTEXITCODE)
            Start-Sleep -Seconds 3
        }
        if (-not $ok) { throw "[sign] signing failed after 3 attempts: $f" }
        Write-Host "[sign] signed: $f"
    }

    if ($Verify) {
        foreach ($f in $Files) {
            $prev = $ErrorActionPreference
            $ErrorActionPreference = 'Continue'
            try { & $signtool verify /pa $f } finally { $ErrorActionPreference = $prev }
            if ($LASTEXITCODE -ne 0) { throw "[sign] verification failed: $f" }
        }
    }
}
