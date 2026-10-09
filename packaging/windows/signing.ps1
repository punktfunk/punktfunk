# Code-signing helpers shared by the Windows packers (pwsh / PowerShell 7).
#
# Dot-source it, then arm the shred in the caller's own script before any key is decoded:
#   . (Join-Path $PSScriptRoot 'signing.ps1')
#   trap { Remove-SigningPfx; break }
# A trap covers only the script it is written in, so it cannot live here. `break` re-throws
# after the shred, so the build still fails.
#
# The app packers call Resolve-SigningMode once, then Sign-File per artifact. The driver builders
# use only Find-SdkTool and the shred: their DRIVER_CERT key and catalog flow stay their own.

$script:ShredPfx = $null

# Deletes the decoded .pfx that $script:ShredPfx names. A decoded release key left on the runner
# is a standing credential, so the trap and the normal exit both call this.
function Remove-SigningPfx {
    if ($script:ShredPfx -and (Test-Path $script:ShredPfx)) {
        Remove-Item $script:ShredPfx -Force -ErrorAction SilentlyContinue
        $script:ShredPfx = $null
    }
}

# The newest copy of a Windows SDK or WDK tool under a versioned kit bin
# (...\10\bin\10.0.N.N\<Arch>\<Name>).
function Find-SdkTool([string]$Name, [string]$Arch = 'x64') {
    $root = 'C:\Program Files (x86)\Windows Kits\10\bin'
    $pattern = "\\(10\.0\.\d+\.\d+)\\$Arch\\"
    $hit = Get-ChildItem -Path $root -Recurse -Filter $Name -ErrorAction SilentlyContinue |
        Where-Object { $_.FullName -match $pattern } |
        Sort-Object { [version]([regex]::Match($_.FullName, $pattern).Groups[1].Value) } |
        Select-Object -Last 1
    if (-not $hit) { throw "$Name not found under $root - install the Windows 10/11 SDK (and the WDK for driver tools)." }
    $hit.FullName
}

# Azure.CodeSigning.Dlib.dll ships in the Microsoft.Trusted.Signing.Client NuGet package, which has
# no installer and no fixed location: an explicit path first, then the two paths the runner setup
# uses (packaging/windows/README.md). The newest wins, so a package update needs no edit here.
function Find-AzureDlib([string]$Explicit) {
    if ($Explicit) {
        if (-not (Test-Path $Explicit)) { throw "AZURE_CODESIGNING_DLIB points at a missing file: $Explicit" }
        return (Resolve-Path $Explicit).Path
    }
    $roots = @(
        (Join-Path $env:USERPROFILE '.nuget\packages\microsoft.trusted.signing.client'),
        'C:\trusted-signing\microsoft.trusted.signing.client'
    ) | Where-Object { $_ -and (Test-Path $_) }
    $hit = $roots | ForEach-Object { Get-ChildItem -Path $_ -Recurse -Filter 'Azure.CodeSigning.Dlib.dll' -ErrorAction SilentlyContinue } |
        Where-Object { $_.FullName -match '\\bin\\x64\\' } |
        Sort-Object LastWriteTime | Select-Object -Last 1
    if (-not $hit) {
        throw ("Azure.CodeSigning.Dlib.dll not found. Install the signing client on this box, e.g. " +
               "``nuget install Microsoft.Trusted.Signing.Client -OutputDirectory " +
               "`$env:USERPROFILE\.nuget\packages``, or set AZURE_CODESIGNING_DLIB to its full path.")
    }
    $hit.FullName
}

<#
  Picks the app signing backend, first match wins: Azure Artifact Signing (the endpoint, account
  and profile all set), the MSIX_CERT_PFX_B64 .pfx, then an ephemeral self-signed cert with
  subject -Publisher and store name -FriendlyName.

  Fails closed on a release: when -RequireSignedCert resolves true ('auto' means GITHUB_REF is
  refs/tags/v*), -NoSign or a missing real backend throws. A per-build throwaway cert cannot be
  pinned and is indistinguishable from an attacker's.

  Returns Mode (none, azure, pfx, selfsigned), Signtool, Pfx, Password, Dlib and Metadata, the
  object Sign-File takes. The .pfx is registered with Remove-SigningPfx before its bytes land.
  -CerPath exports the public .cer of a .pfx-backed mode for users to import; Azure has no .pfx,
  and its chain is publicly trusted.
#>
function Resolve-SigningMode {
    param(
        [Parameter(Mandatory = $true)][string]$OutDir,
        [Parameter(Mandatory = $true)][string]$Publisher,
        [Parameter(Mandatory = $true)][string]$FriendlyName,
        [string]$PfxBase64,
        [string]$PfxPassword,
        [string]$AzureEndpoint,
        [string]$AzureAccount,
        [string]$AzureProfile,
        [string]$AzureDlib,
        [ValidateSet('auto', 'true', 'false')][string]$RequireSignedCert = 'auto',
        [string]$CerPath,
        [switch]$NoSign
    )
    $requireCert = if ($RequireSignedCert -eq 'auto') { $env:GITHUB_REF -like 'refs/tags/v*' }
                   else { [Convert]::ToBoolean($RequireSignedCert) }
    if ($NoSign -and $requireCert) {
        throw "release build ($env:GITHUB_REF) with -NoSign - refusing to publish an unsigned installer."
    }
    $signing = [pscustomobject]@{
        Mode     = 'none'
        Signtool = $null
        Pfx      = (Join-Path $OutDir 'signing.pfx')
        Password = $PfxPassword
        Dlib     = $null
        Metadata = (Join-Path $OutDir 'azure-codesigning.json')
    }
    if ($NoSign) { return $signing }
    $signing.Signtool = Find-SdkTool 'signtool.exe'
    Write-Host "signtool: $($signing.Signtool)"

    if ($AzureEndpoint -and $AzureAccount -and $AzureProfile) {
        $signing.Mode = 'azure'
        $signing.Dlib = Find-AzureDlib $AzureDlib
        # signtool reads the account and profile from this file (/dmdf), not the command line.
        @{
            Endpoint               = $AzureEndpoint
            CodeSigningAccountName = $AzureAccount
            CertificateProfileName = $AzureProfile
        } | ConvertTo-Json | Set-Content -Path $signing.Metadata -Encoding utf8
        Write-Host "signing via Azure Artifact Signing: $AzureAccount/$AzureProfile at $AzureEndpoint"
        Write-Host "  dlib: $($signing.Dlib)"
        foreach ($v in 'AZURE_TENANT_ID', 'AZURE_CLIENT_ID', 'AZURE_CLIENT_SECRET') {
            if (-not [Environment]::GetEnvironmentVariable($v)) {
                throw ("Azure signing selected but $v is not set. The dlib authenticates with " +
                       "DefaultAzureCredential; without the service-principal trio it falls through to " +
                       "an interactive login that cannot complete on a runner and hangs the build.")
            }
        }
        return $signing
    }

    $script:ShredPfx = $signing.Pfx
    if ($PfxBase64) {
        $signing.Mode = 'pfx'
        Write-Host "signing with supplied code-signing cert (MSIX_CERT_PFX_B64)"
        [IO.File]::WriteAllBytes($signing.Pfx, [Convert]::FromBase64String($PfxBase64))
    }
    elseif ($requireCert) {
        throw ("release build ($env:GITHUB_REF) with neither AZURE_CODESIGNING_* nor MSIX_CERT_PFX_B64 - " +
               "refusing to fall back to an ephemeral self-signed cert. Restore the signing secrets " +
               "(packaging/windows/README.md), or pass -RequireSignedCert false if this really is a test build.")
    }
    else {
        $signing.Mode = 'selfsigned'
        Write-Host "no MSIX_CERT_PFX_B64 -> generating an ephemeral self-signed cert (subject $Publisher)"
        if (-not $signing.Password) { $signing.Password = 'punktfunk' }
        $tmp = New-SelfSignedCertificate -Type Custom -Subject $Publisher `
            -KeyUsage DigitalSignature -FriendlyName $FriendlyName `
            -CertStoreLocation 'Cert:\CurrentUser\My' `
            -TextExtension @('2.5.29.37={text}1.3.6.1.5.5.7.3.3', '2.5.29.19={text}')
        $sec = ConvertTo-SecureString -String $signing.Password -Force -AsPlainText
        Export-PfxCertificate -Cert "Cert:\CurrentUser\My\$($tmp.Thumbprint)" -FilePath $signing.Pfx -Password $sec | Out-Null
        Remove-Item "Cert:\CurrentUser\My\$($tmp.Thumbprint)" -Force
    }

    if ($CerPath) {
        $pwsec = if ($signing.Password) { ConvertTo-SecureString -String $signing.Password -Force -AsPlainText } else { $null }
        $pubCert = if ($pwsec) { Get-PfxCertificate -FilePath $signing.Pfx -Password $pwsec } else { Get-PfxCertificate -FilePath $signing.Pfx }
        Export-Certificate -Cert $pubCert -FilePath $CerPath | Out-Null
        Write-Host "signing cert subject=$($pubCert.Subject) thumbprint=$($pubCert.Thumbprint)"
    }
    $signing
}

<#
  Signs one file with the backend Resolve-SigningMode picked; Mode 'none' signs nothing.

  The timestamp is best-effort for a .pfx that outlives the release, but mandatory under Azure:
  its leaf certs are minted per request and expire in about three days, so an untimestamped
  signature stops verifying soon after shipping. Azure mode therefore never retries without one.

  -Package signs a container (the .msix): it skips the PE FileDescription /d and the /du URL.
#>
function Sign-File {
    param(
        [Parameter(Mandatory = $true)]$Signing,
        [Parameter(Mandatory = $true)][string]$Path,
        [switch]$Package
    )
    if ($Signing.Mode -eq 'none') { return }
    # The retry below reads $LASTEXITCODE, so a non-zero signtool exit must not throw first.
    $PSNativeCommandUseErrorActionPreference = $false
    if ($Signing.Mode -eq 'azure') {
        $signArgs = @('sign', '/fd', 'SHA256', '/dlib', $Signing.Dlib, '/dmdf', $Signing.Metadata)
        $ts = 'http://timestamp.acs.microsoft.com'
    }
    else {
        $signArgs = @('sign', '/fd', 'SHA256', '/f', $Signing.Pfx)
        if ($Signing.Password) { $signArgs += @('/p', $Signing.Password) }
        $ts = 'http://timestamp.digicert.com'
    }
    if (-not $Package) {
        # UAC names a signed file by /d. The exe's own FileDescription stays the one source.
        $desc = (Get-Item $Path).VersionInfo.FileDescription
        if ($desc) { $signArgs += @('/d', $desc) }
        $signArgs += @('/du', 'https://punktfunk.unom.io')
    }
    $signtool = $Signing.Signtool
    & $signtool ($signArgs + @('/tr', $ts, '/td', 'SHA256', $Path))
    if ($LASTEXITCODE -eq 0) { return }
    if ($Signing.Mode -eq 'azure') {
        throw ("timestamped sign failed for $Path ($LASTEXITCODE) - NOT retrying without a timestamp. " +
               "An Azure signing cert is valid for ~3 days; an untimestamped signature would go " +
               "untrusted within days of release.")
    }
    Write-Warning "timestamped sign failed for $Path - retrying without a timestamp"
    & $signtool ($signArgs + @($Path))
    if ($LASTEXITCODE -ne 0) { throw "signtool sign failed for $Path ($LASTEXITCODE)" }
}
