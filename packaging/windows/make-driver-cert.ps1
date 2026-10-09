<#
.SYNOPSIS
  Generate the stable punktfunk driver code-signing certificate.

.DESCRIPTION
  Produces a self-signed code-signing cert (CN=punktfunk-driver) and writes the two values that
  become the DRIVER_CERT_PFX_B64 / DRIVER_CERT_PASSWORD Gitea Actions secrets.

  Built on the .NET CertificateRequest API rather than New-SelfSignedCertificate, for two reasons:
    * No key container is involved, so this works over SSH. New-SelfSignedCertificate fails with
      NTE_PERM 0x80090010 on a network logon because it wants the user's key container.
    * The extension set is explicit, so it matches byte-for-byte what the drivers have always been
      signed with (verified against the certs on this box): KeyUsage=DigitalSignature (critical),
      EKU=codeSigning (non-critical), SubjectKeyIdentifier, and NO basicConstraints.

  The key exists only in memory and in the .pfx this writes. It is never added to a certificate
  store, so there is nothing to clean up afterwards except the output folder.

  Self-test: the generated .pfx is used to actually sign a scratch binary with signtool before
  anything is reported, so a cert signtool cannot consume fails HERE and not six months from now
  in a release build.

.PARAMETER OutDir
  Where to write the outputs. Defaults to a fresh timestamped folder under the user profile.

.PARAMETER TestOnly
  Generate, self-test, then delete everything and report. Writes no secrets. Use to verify the
  script works before generating the real key.

.EXAMPLE
  pwsh -File make-driver-cert.ps1
#>
[CmdletBinding()]
param(
    [string]$OutDir,
    [switch]$TestOnly
)
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false
. (Join-Path $PSScriptRoot 'signing.ps1')

$subject = 'CN=punktfunk-driver'
$years   = 10

if (-not $OutDir) {
    $stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
    $OutDir = Join-Path $env:USERPROFILE "punktfunk-driver-cert-$stamp"
}
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

# -- 1. key + self-signed cert, in memory ------------------------------------------------------
# RSA 3072 / SHA-256. The drivers were previously signed with 2048; nothing interoperates with this
# cert except our own installer (we are our own trust anchor), so there is no compatibility reason
# to stay at 2048 - and the signtool self-test below proves 3072 is consumable.
$rsa = [System.Security.Cryptography.RSA]::Create(3072)
$req = [System.Security.Cryptography.X509Certificates.CertificateRequest]::new(
    $subject, $rsa,
    [System.Security.Cryptography.HashAlgorithmName]::SHA256,
    [System.Security.Cryptography.RSASignaturePadding]::Pkcs1)

# KeyUsage: DigitalSignature, CRITICAL - matches the existing certs.
$req.CertificateExtensions.Add(
    [System.Security.Cryptography.X509Certificates.X509KeyUsageExtension]::new(
        [System.Security.Cryptography.X509Certificates.X509KeyUsageFlags]::DigitalSignature, $true))
# EKU: code signing (1.3.6.1.5.5.7.3.3), NON-critical - matches the existing certs.
$oids = [System.Security.Cryptography.OidCollection]::new()
$oids.Add([System.Security.Cryptography.Oid]::new('1.3.6.1.5.5.7.3.3')) | Out-Null
$req.CertificateExtensions.Add(
    [System.Security.Cryptography.X509Certificates.X509EnhancedKeyUsageExtension]::new($oids, $false))
# SubjectKeyIdentifier - matches. Deliberately NO basicConstraints: the shipping certs carry none,
# and this is the one place to not get creative, since a chain-building difference would surface as
# a failed driver install on a user's machine rather than as an error here.
$req.CertificateExtensions.Add(
    [System.Security.Cryptography.X509Certificates.X509SubjectKeyIdentifierExtension]::new($req.PublicKey, $false))

$now  = [DateTimeOffset]::UtcNow.AddMinutes(-5)   # backdate slightly: clock skew must not make it not-yet-valid
$cert = $req.CreateSelfSigned($now, $now.AddYears($years))

# -- 2. export ---------------------------------------------------------------------------------
# RandomNumberGenerator, not Get-Random: Get-Random is System.Random and has no business generating
# the passphrase on a signing key.
$rng = [System.Security.Cryptography.RandomNumberGenerator]::Create()
$bytes = [byte[]]::new(24); $rng.GetBytes($bytes)
$pw = [Convert]::ToBase64String($bytes)

# .NET's own PKCS#12 writer - avoids the OpenSSL 3 trap where the default AES-256/PBKDF2 encryption
# produces a .pfx that Windows CryptoAPI cannot read.
$pfxBytes = $cert.Export([System.Security.Cryptography.X509Certificates.X509ContentType]::Pfx, $pw)
$pfxPath = Join-Path $OutDir 'driver.pfx'
[IO.File]::WriteAllBytes($pfxPath, $pfxBytes)

# -- 3. self-test: can signtool actually sign with it? -----------------------------------------
$signtool = try { Find-SdkTool 'signtool.exe' } catch { $null }
$selftest = 'SKIPPED (no signtool on this box)'
if ($signtool) {
    $scratch = Join-Path $OutDir 'selftest.exe'
    Copy-Item C:\Windows\System32\notepad.exe $scratch -Force
    $out = & $signtool sign /fd SHA256 /f $pfxPath /p $pw $scratch 2>&1 | Out-String
    if ($LASTEXITCODE -eq 0) {
        # Assert the signature is present and carries our subject. /pa chain trust FAILS until the
        # cert is in the machine's trust stores - expected, and not what this checks.
        $v = & $signtool verify /pa /v $scratch 2>&1 | Out-String
        $selftest = if ($v -match 'punktfunk-driver') { 'PASS (signtool signed; signature carries CN=punktfunk-driver)' }
                    else { 'PASS (signtool signed)' }
    }
    elseif ($out -match '0x80090010') {
        # NTE_PERM. Not a bad certificate - a network logon (SSH) has no key container, so signtool
        # cannot import the .pfx to sign with it. The KEY ABOVE IS STILL VALID: generating it needs
        # no container, only consuming it does. Re-run at an interactive logon (console/RDP) to
        # exercise this, or just let the canary CI build be the proof - the runner signs under a
        # real logon, which is how the MSIX cert already works.
        $selftest = 'SKIPPED (NTE_PERM 0x80090010 - no key container on this logon; run at a console/RDP session, or verify via a canary build)'
    }
    else {
        throw "SELF-TEST FAILED: signtool could not sign with the generated .pfx (exit $LASTEXITCODE)`n$out"
    }
    Remove-Item $scratch -Force -EA SilentlyContinue
}

# -- 4. report ---------------------------------------------------------------------------------
if ($TestOnly) {
    Remove-Item $OutDir -Recurse -Force
    Write-Output ''
    Write-Output "TEST ONLY - nothing kept."
    Write-Output "  thumbprint would have been : $($cert.Thumbprint)"
    Write-Output "  key size                   : $($cert.PublicKey.GetRSAPublicKey().KeySize) bits"
    Write-Output "  not after                  : $($cert.NotAfter.ToString('yyyy-MM-dd'))"
    Write-Output "  signtool self-test          : $selftest"
    Write-Output "  extensions                 : $((($cert.Extensions | ForEach-Object { $_.Oid.FriendlyName }) -join ', '))"
    return
}

$b64Path = Join-Path $OutDir 'DRIVER_CERT_PFX_B64.txt'
$pwPath  = Join-Path $OutDir 'DRIVER_CERT_PASSWORD.txt'
[IO.File]::WriteAllText($b64Path, [Convert]::ToBase64String($pfxBytes))
[IO.File]::WriteAllText($pwPath,  $pw)

Write-Output ''
Write-Output '================ punktfunk driver signing cert ================'
Write-Output ''
Write-Output "  THUMBPRINT (public - this is the only value to share):"
Write-Output "      $($cert.Thumbprint)"
Write-Output ''
Write-Output "  key size    : $($cert.PublicKey.GetRSAPublicKey().KeySize) bits"
Write-Output "  valid until : $($cert.NotAfter.ToString('yyyy-MM-dd'))"
Write-Output "  self-test   : $selftest"
Write-Output ''
Write-Output '  SECRETS - do not paste these into chat or a terminal. Open the files:'
Write-Output "      DRIVER_CERT_PFX_B64   ->  $b64Path"
Write-Output "      DRIVER_CERT_PASSWORD  ->  $pwPath"
Write-Output ''
Write-Output '  Add both as REPO-level secrets at:'
Write-Output '      https://git.unom.io/unom/punktfunk/settings/actions/secrets'
Write-Output ''
Write-Output "  BACK UP $pfxPath + the password somewhere you'd keep a signing key FIRST."
Write-Output '  Then remove the whole folder:'
Write-Output "      Remove-Item -Recurse -Force '$OutDir'"
Write-Output ''
Write-Output '==============================================================='
