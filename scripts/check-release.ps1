<#
.SYNOPSIS
    Verify tmpclean release executables: PE32+ x64, hardening flags, no packer, size budget.

.DESCRIPTION
    Reads the PE headers directly (no dumpbin/llvm-readobj dependency) of
    tmpclean.exe and tmpclean-gui.exe in <Dir> and checks:
      - PE32+ for x64 (COFF Machine 0x8664, optional-header magic 0x20B)
      - DllCharacteristics: HIGH_ENTROPY_VA (0x20), DYNAMIC_BASE (0x40),
        NX_COMPAT (0x100), GUARD_CF (0x4000)
      - no UPX section names (UPX0/UPX1/...) and no "UPX!" marker in the file
      - size: tmpclean.exe <= 1.5 MiB, tmpclean-gui.exe <= 2 MiB
    Prints one PASS/FAIL line per check and exits 1 if any check failed.
    Missing, non-PE or truncated files are reported as FAIL, never as an
    unhandled error.

.EXAMPLE
    pwsh scripts/check-release.ps1 target/release
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true, Position = 0)]
    [string]$Dir
)

Set-StrictMode -Version 3
$ErrorActionPreference = 'Stop'

$Limits = [ordered]@{
    'tmpclean.exe'     = 1572864   # 1.5 MiB
    'tmpclean-gui.exe' = 2097152   # 2 MiB
}

$RequiredDllCharacteristics = [ordered]@{
    'HIGH_ENTROPY_VA' = 0x0020
    'DYNAMIC_BASE'    = 0x0040
    'NX_COMPAT'       = 0x0100
    'GUARD_CF'        = 0x4000
}

$script:Failures = 0

function Write-Check([string]$Exe, [string]$Check, [bool]$Ok, [string]$Detail) {
    if ($Ok) { $status = 'PASS' } else { $status = 'FAIL'; $script:Failures++ }
    Write-Host ('{0} {1}: {2} ({3})' -f $status, $Exe, $Check, $Detail)
}

function Read-U16([byte[]]$Bytes, [long]$Offset) {
    if ($Offset -lt 0 -or $Offset + 2 -gt $Bytes.Length) {
        throw ('truncated file: no u16 at 0x{0:X}' -f $Offset)
    }
    return [BitConverter]::ToUInt16($Bytes, [int]$Offset)
}

function Read-U32([byte[]]$Bytes, [long]$Offset) {
    if ($Offset -lt 0 -or $Offset + 4 -gt $Bytes.Length) {
        throw ('truncated file: no u32 at 0x{0:X}' -f $Offset)
    }
    return [BitConverter]::ToUInt32($Bytes, [int]$Offset)
}

# Returns @{ Machine; Magic; DllCharacteristics; Sections } or throws on a
# malformed/truncated image.
function Read-PeHeaders([byte[]]$Bytes) {
    if ((Read-U16 $Bytes 0) -ne 0x5A4D) { throw 'not a PE file: missing MZ signature' }
    $pe = [long](Read-U32 $Bytes 0x3C)
    if ((Read-U32 $Bytes $pe) -ne 0x00004550) { throw ('not a PE file: missing PE signature at 0x{0:X}' -f $pe) }
    $coff = $pe + 4
    $machine = Read-U16 $Bytes $coff
    $sectionCount = Read-U16 $Bytes ($coff + 2)
    $optSize = Read-U16 $Bytes ($coff + 16)
    $opt = $coff + 20
    $magic = Read-U16 $Bytes $opt
    if ($optSize -lt 72) { throw ('optional header too small: {0} bytes' -f $optSize) }
    # DllCharacteristics sits at offset 70 in both PE32 and PE32+.
    $dllChars = Read-U16 $Bytes ($opt + 70)
    if ($sectionCount -gt 96) { throw ('implausible section count {0}' -f $sectionCount) }
    $table = $opt + $optSize
    if ($table + 40 * $sectionCount -gt $Bytes.Length) { throw 'truncated file: section table out of range' }
    $sections = @()
    for ($i = 0; $i -lt $sectionCount; $i++) {
        $name = [Text.Encoding]::ASCII.GetString($Bytes, [int]($table + 40 * $i), 8).TrimEnd([char]0)
        $sections += $name
    }
    return @{ Machine = $machine; Magic = $magic; DllCharacteristics = $dllChars; Sections = $sections }
}

foreach ($exe in $Limits.Keys) {
    $path = Join-Path $Dir $exe
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        Write-Check $exe 'exists' $false "not found: $path"
        continue
    }
    Write-Check $exe 'exists' $true $path

    $bytes = [IO.File]::ReadAllBytes((Resolve-Path -LiteralPath $path).ProviderPath)
    $limit = $Limits[$exe]
    Write-Check $exe 'size' ($bytes.Length -le $limit) ('{0} bytes, limit {1}' -f $bytes.Length, $limit)

    # Latin-1 maps every byte 1:1 to a char, so an ordinal search is a byte search.
    $text = [Text.Encoding]::GetEncoding(28591).GetString($bytes)
    $marker = $text.IndexOf('UPX!', [StringComparison]::Ordinal)
    Write-Check $exe 'no-upx-marker' ($marker -lt 0) $(if ($marker -lt 0) { 'absent' } else { 'UPX! at 0x{0:X}' -f $marker })

    try {
        $h = Read-PeHeaders $bytes
    } catch {
        Write-Check $exe 'pe-parse' $false $_.Exception.Message
        continue
    }
    Write-Check $exe 'pe-parse' $true ('{0} sections' -f $h.Sections.Count)
    Write-Check $exe 'machine-x64' ($h.Machine -eq 0x8664) ('Machine 0x{0:X4}' -f $h.Machine)
    Write-Check $exe 'pe32plus' ($h.Magic -eq 0x20B) ('Magic 0x{0:X3}' -f $h.Magic)
    foreach ($flag in $RequiredDllCharacteristics.Keys) {
        $bit = $RequiredDllCharacteristics[$flag]
        Write-Check $exe $flag (($h.DllCharacteristics -band $bit) -eq $bit) ('DllCharacteristics 0x{0:X4}' -f $h.DllCharacteristics)
    }
    $packed = @($h.Sections | Where-Object { $_ -match '^UPX' })
    Write-Check $exe 'section-names' ($packed.Count -eq 0) ('[{0}]' -f ($h.Sections -join ','))
}

if ($script:Failures -gt 0) {
    Write-Host "check-release: $($script:Failures) check(s) FAILED"
    exit 1
}
Write-Host 'check-release: all checks PASSED'
exit 0
