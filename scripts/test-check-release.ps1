<#
.SYNOPSIS
    Self-test for check-release.ps1: the real release dir must pass and
    every tampered/malformed fixture must fail with the expected FAIL line.

.EXAMPLE
    pwsh scripts/test-check-release.ps1            # uses target/release
    pwsh scripts/test-check-release.ps1 -ReleaseDir path\to\release
#>
[CmdletBinding()]
param(
    [string]$ReleaseDir = (Join-Path $PSScriptRoot '..' 'target' 'release')
)

Set-StrictMode -Version 3
$ErrorActionPreference = 'Stop'

$check = Join-Path $PSScriptRoot 'check-release.ps1'
$cli = 'tmpclean.exe'
$gui = 'tmpclean-gui.exe'
$script:Failures = 0

function Invoke-Expect([string]$Name, [string]$Dir, [bool]$ShouldPass, [string]$MustContain) {
    $out = & pwsh -NoProfile -NonInteractive -File $check $Dir 2>&1 | Out-String
    $code = $LASTEXITCODE
    Write-Host "----- fixture '$Name' (check-release exit $code) -----"
    Write-Host $out.TrimEnd()
    if ($ShouldPass) {
        $ok = ($code -eq 0) -and ($out -notmatch '(?m)^FAIL ')
        $why = 'expected exit 0 and no FAIL lines'
    } else {
        $ok = ($code -ne 0) -and ($out -match ('(?m)^' + [regex]::Escape($MustContain)))
        $why = "expected non-zero exit and a line starting '$MustContain'"
    }
    if ($ok) {
        Write-Host "TEST PASS: $Name ($why)"
    } else {
        Write-Host "TEST FAIL: $Name ($why)"
        $script:Failures++
    }
}

function New-Fixture([string]$Root, [string]$Name) {
    $dir = Join-Path $Root $Name
    New-Item -ItemType Directory -Path $dir | Out-Null
    Copy-Item -LiteralPath (Join-Path $ReleaseDir $cli) -Destination $dir
    Copy-Item -LiteralPath (Join-Path $ReleaseDir $gui) -Destination $dir
    return $dir
}

foreach ($exe in @($cli, $gui)) {
    if (-not (Test-Path -LiteralPath (Join-Path $ReleaseDir $exe) -PathType Leaf)) {
        Write-Host "TEST FAIL: release build missing $exe in $ReleaseDir"
        exit 1
    }
}

$root = Join-Path ([IO.Path]::GetTempPath()) ('check-release-test-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $root | Out-Null
try {
    # 1. The real release build passes.
    Invoke-Expect 'real-release' $ReleaseDir $true ''

    # 2. Packer fixture: first section renamed to UPX0.
    $dir = New-Fixture $root 'upx0-section'
    $path = Join-Path $dir $cli
    $b = [IO.File]::ReadAllBytes($path)
    $pe = [BitConverter]::ToInt32($b, 0x3C)
    $optSize = [BitConverter]::ToUInt16($b, $pe + 20)
    $table = $pe + 24 + $optSize
    $old = [Text.Encoding]::ASCII.GetString($b, $table, 8).TrimEnd([char]0)
    $name = [Text.Encoding]::ASCII.GetBytes('UPX0') + [byte[]](0, 0, 0, 0)
    [Array]::Copy([byte[]]$name, 0, $b, $table, 8)
    [IO.File]::WriteAllBytes($path, $b)
    Write-Host "fixture upx0-section: renamed first section '$old' -> 'UPX0' at 0x$('{0:X}' -f $table)"
    Invoke-Expect 'upx0-section' $dir $false "FAIL ${cli}: section-names"

    # 3. Packer marker fixture: "UPX!" appended to the GUI exe.
    $dir = New-Fixture $root 'upx-marker'
    $fs = [IO.File]::Open((Join-Path $dir $gui), [IO.FileMode]::Append)
    try { $m = [Text.Encoding]::ASCII.GetBytes('UPX!'); $fs.Write($m, 0, $m.Length) } finally { $fs.Dispose() }
    Invoke-Expect 'upx-marker' $dir $false "FAIL ${gui}: no-upx-marker"

    # 4. Oversized CLI.
    $dir = New-Fixture $root 'oversized'
    $fs = [IO.File]::Open((Join-Path $dir $cli), [IO.FileMode]::Open)
    try { $fs.SetLength(1572864 + 1) } finally { $fs.Dispose() }
    Invoke-Expect 'oversized' $dir $false "FAIL ${cli}: size"

    # 5. Malformed: not a PE file at all.
    $dir = New-Fixture $root 'non-pe'
    Set-Content -LiteralPath (Join-Path $dir $cli) -Value 'this is not a PE image' -NoNewline
    Invoke-Expect 'non-pe' $dir $false "FAIL ${cli}: pe-parse"

    # 6. Malformed: truncated after the DOS header.
    $dir = New-Fixture $root 'truncated'
    $fs = [IO.File]::Open((Join-Path $dir $cli), [IO.FileMode]::Open)
    try { $fs.SetLength(128) } finally { $fs.Dispose() }
    Invoke-Expect 'truncated' $dir $false "FAIL ${cli}: pe-parse"

    # 7. Missing executables.
    $dir = Join-Path $root 'empty'
    New-Item -ItemType Directory -Path $dir | Out-Null
    Invoke-Expect 'missing' $dir $false "FAIL ${cli}: exists"
} finally {
    Remove-Item -LiteralPath $root -Recurse -Force -ErrorAction SilentlyContinue
}

if ($script:Failures -gt 0) {
    Write-Host "test-check-release: $($script:Failures) test(s) FAILED"
    exit 1
}
Write-Host 'test-check-release: all tests PASSED'
exit 0
