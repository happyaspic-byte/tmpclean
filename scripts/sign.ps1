<#
.SYNOPSIS
    Documentation-only: how to Authenticode-sign the tmpclean release executables.

.DESCRIPTION
    This script does NOT sign anything and is NOT run in CI. It prints the
    signtool command lines for the two supported signing routes so a
    maintainer with a certificate can run them by hand. No certificate,
    password, or credential is stored in this repository.

    Prerequisites: Windows SDK signtool.exe (run from a "Developer PowerShell
    for VS" or pass -SignTool), and a release build (cargo build --release).
    Always sign AFTER scripts/check-release.ps1 passes; signing does not change
    the checked PE flags. Never UPX-pack before or after signing.

    Route 1 - PFX file (-Mode Pfx)
      Code-signing certificate exported as .pfx. Supply the password via the
      environment variable named by -PasswordEnv (default TMPCLEAN_PFX_PASSWORD)
      so it never appears in shell history or this script.

    Route 2 - Azure Trusted Signing (-Mode Azure)
      Requires an Azure Trusted Signing account + certificate profile, the
      "Microsoft.Trusted.Signing.Client" package (provides
      Azure.CodeSigning.Dlib.dll), and an Azure identity the
      DefaultAzureCredential chain can use (az login, managed identity, or
      AZURE_CLIENT_ID/AZURE_TENANT_ID/AZURE_CLIENT_SECRET in the environment).
      metadata.json looks like:
        {
          "Endpoint": "https://<region>.codesigning.azure.net",
          "CodeSigningAccountName": "<account>",
          "CertificateProfileName": "<profile>"
        }

    Verify afterwards:  signtool verify /pa /v target\release\tmpclean.exe

.PARAMETER Mode
    Pfx or Azure.
.PARAMETER ReleaseDir
    Directory with tmpclean.exe and tmpclean-gui.exe (default target\release).
.PARAMETER PfxPath
    Path to the .pfx file (Pfx mode).
.PARAMETER PasswordEnv
    Name of the environment variable holding the PFX password (Pfx mode).
.PARAMETER DlibPath
    Path to Azure.CodeSigning.Dlib.dll (Azure mode).
.PARAMETER MetadataPath
    Path to metadata.json (Azure mode).
.PARAMETER TimestampUrl
    RFC 3161 timestamp server. Defaults: DigiCert for Pfx, Microsoft for Azure.
.PARAMETER SignTool
    signtool executable (default: signtool.exe on PATH).

.EXAMPLE
    pwsh scripts/sign.ps1 -Mode Pfx -PfxPath C:\keys\codesign.pfx
.EXAMPLE
    pwsh scripts/sign.ps1 -Mode Azure -DlibPath C:\tools\Azure.CodeSigning.Dlib.dll -MetadataPath C:\tools\metadata.json
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][ValidateSet('Pfx', 'Azure')][string]$Mode,
    [string]$ReleaseDir = 'target\release',
    [string]$PfxPath = '<path\to\codesign.pfx>',
    [string]$PasswordEnv = 'TMPCLEAN_PFX_PASSWORD',
    [string]$DlibPath = '<path\to\Azure.CodeSigning.Dlib.dll>',
    [string]$MetadataPath = '<path\to\metadata.json>',
    [string]$TimestampUrl = '',
    [string]$SignTool = 'signtool.exe'
)

Set-StrictMode -Version 3

$files = @('tmpclean.exe', 'tmpclean-gui.exe') | ForEach-Object { '"{0}"' -f (Join-Path $ReleaseDir $_) }

if ($Mode -eq 'Pfx') {
    if (-not $TimestampUrl) { $TimestampUrl = 'http://timestamp.digicert.com' }
    $cmd = '{0} sign /v /fd SHA256 /f "{1}" /p $env:{2} /tr {3} /td SHA256 {4}' -f `
        $SignTool, $PfxPath, $PasswordEnv, $TimestampUrl, ($files -join ' ')
} else {
    if (-not $TimestampUrl) { $TimestampUrl = 'http://timestamp.acs.microsoft.com' }
    $cmd = '{0} sign /v /debug /fd SHA256 /tr {1} /td SHA256 /dlib "{2}" /dmdf "{3}" {4}' -f `
        $SignTool, $TimestampUrl, $DlibPath, $MetadataPath, ($files -join ' ')
}

Write-Host 'Documentation only - nothing is executed. Run this by hand:'
Write-Host ''
Write-Host "  $cmd"
Write-Host ''
Write-Host ('Then verify:  {0} verify /pa /v {1}' -f $SignTool, ($files -join ' '))
