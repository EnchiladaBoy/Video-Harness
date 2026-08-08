[CmdletBinding()]
param(
    [Parameter()]
    [string] $TargetDirectory = (Join-Path $PSScriptRoot "../desktop/src-tauri/target")
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

function Get-SingleWebViewInstaller {
    param(
        [Parameter(Mandatory)]
        [string] $Label,
        [Parameter(Mandatory)]
        [string] $Root
    )

    if (-not (Test-Path -LiteralPath $Root -PathType Container)) {
        throw "$Label WebView2 source directory is missing: $Root"
    }

    $rootItem = Get-Item -LiteralPath $Root
    $installers = @(Get-ChildItem -LiteralPath $rootItem.FullName -Recurse -File |
        Where-Object { $_.Name -ceq "MicrosoftEdgeWebView2RuntimeInstallerX64.exe" })
    if ($installers.Count -ne 1) {
        throw "expected one $Label WebView2 offline installer below $($rootItem.FullName), found $($installers.Count)"
    }

    $installer = $installers[0]
    if ($null -eq $installer.Directory.Parent -or
        $installer.Directory.Parent.FullName -ne $rootItem.FullName) {
        throw "$Label WebView2 installer is not in the expected <GUID>/<FILENAME> layout: $($installer.FullName)"
    }
    if ($installer.Directory.Name -notmatch '^[0-9a-fA-F-]+$') {
        throw "$Label WebView2 installer has an unexpected release identifier: $($installer.Directory.Name)"
    }

    return $installer
}

function Assert-MicrosoftCodeSignature {
    param(
        [Parameter(Mandatory)]
        [System.IO.FileInfo] $Installer
    )

    $signature = Get-AuthenticodeSignature -LiteralPath $Installer.FullName
    if ($signature.Status -ne "Valid" -or $null -eq $signature.SignerCertificate) {
        throw "$($Installer.FullName) has invalid Authenticode status: $($signature.Status) $($signature.StatusMessage)"
    }
    if ($null -eq $signature.TimeStamperCertificate) {
        throw "$($Installer.FullName) has no trusted Authenticode timestamp"
    }

    $signer = $signature.SignerCertificate
    $publisher = $signer.GetNameInfo(
        [System.Security.Cryptography.X509Certificates.X509NameType]::SimpleName,
        $false
    )
    if ($publisher -cne "Microsoft Corporation" -or
        $signer.Subject -notmatch '(^|,\s*)O=Microsoft Corporation(,|$)') {
        throw "unexpected WebView2 publisher: $($signer.Subject)"
    }
    $ekuExtension = @($signer.Extensions |
        Where-Object { $_.Oid.Value -eq "2.5.29.37" })
    if ($ekuExtension.Count -ne 1) {
        throw "$($Installer.FullName) signer has no unambiguous enhanced-key-usage extension"
    }
    $enhancedKeyUsage = [System.Security.Cryptography.X509Certificates.X509EnhancedKeyUsageExtension]::new(
        $ekuExtension[0],
        $ekuExtension[0].Critical
    )
    if ($enhancedKeyUsage.EnhancedKeyUsages.Value -notcontains "1.3.6.1.5.5.7.3.3") {
        throw "$($Installer.FullName) signer is not valid for code signing"
    }
}

$resolvedTarget = (Resolve-Path -LiteralPath $TargetDirectory).Path
$nsisInstaller = Get-SingleWebViewInstaller `
    -Label "NSIS" `
    -Root (Join-Path $resolvedTarget ".tauri/x64")

$nsisHash = (Get-FileHash -LiteralPath $nsisInstaller.FullName -Algorithm SHA256).Hash
Assert-MicrosoftCodeSignature -Installer $nsisInstaller

$version = $nsisInstaller.VersionInfo.ProductVersion
Write-Host "Verified Microsoft WebView2 offline installer: version=$version sha256=$($nsisHash.ToLowerInvariant())"
