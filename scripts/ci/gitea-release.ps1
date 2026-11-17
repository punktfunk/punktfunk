# Shared Gitea Release helpers for the punktfunk Windows CI workflows (pwsh / PowerShell 7).
#
# Dot-source it, then call Ensure-GiteaRelease / Upsert-GiteaAsset:
#   . scripts/ci/gitea-release.ps1
# Mirrors scripts/ci/gitea-release.sh; parses JSON with Invoke-RestMethod (the Windows runner
# has no python). Same idempotent semantics: Upsert-GiteaAsset deletes an existing asset of
# the same name before uploading, so re-runs / rolling canary uploads don't 409 — and attaches
# a `<asset>.sha256` sidecar alongside every asset, exactly like the bash twin.
#
# Env (Gitea Actions sets the first two automatically):
#   GITHUB_SERVER_URL   e.g. https://git.unom.io
#   GITHUB_REPOSITORY   e.g. unom/punktfunk
#   GITEA_TOKEN         a PAT with repository (release) write scope — set from secrets.REGISTRY_TOKEN
#                       (must carry write:repository, not only write:package)

$ErrorActionPreference = 'Stop'

function _GiteaApi {
  if (-not $env:GITHUB_SERVER_URL) { throw 'GITHUB_SERVER_URL unset' }
  if (-not $env:GITHUB_REPOSITORY) { throw 'GITHUB_REPOSITORY unset' }
  "$($env:GITHUB_SERVER_URL)/api/v1/repos/$($env:GITHUB_REPOSITORY)"
}

function _GiteaHeaders {
  if (-not $env:GITEA_TOKEN) { throw 'GITEA_TOKEN unset' }
  @{ Authorization = "token $($env:GITEA_TOKEN)" }
}

# Ensure-GiteaRelease TAG NAME PRERELEASE [TARGETCOMMITISH] -> release id
#   Idempotently create or fetch the release for TAG. Prerelease is 'true', 'false', or 'auto'
#   (auto marks it a prerelease iff TAG carries a `-` pre-release suffix, e.g. v0.2.0-rc1, so an
#   rc never becomes "Latest"). TargetCommitish (optional) creates the tag if missing.
function Ensure-GiteaRelease {
  param(
    [Parameter(Mandatory)] [string]$Tag,
    [Parameter(Mandatory)] [string]$Name,
    [Parameter(Mandatory)] [string]$Prerelease,
    [string]$TargetCommitish
  )
  $pre = if ($Prerelease -eq 'auto') { [bool]($Tag -match '-') } else { [System.Convert]::ToBoolean($Prerelease) }
  $api = _GiteaApi; $h = _GiteaHeaders
  $payload = @{ tag_name = $Tag; name = $Name; prerelease = $pre }
  if ($TargetCommitish) { $payload.target_commitish = $TargetCommitish }
  # Seed the body from docs/releases/<Tag>.md (the source of truth) so a Windows job winning the
  # create race is born WITH its notes, exactly like the bash twin. Missing file (canary/rc) -> no
  # body key. $PSScriptRoot is scripts/ci; walk up two levels to the repo root.
  $notes = Join-Path (Split-Path (Split-Path $PSScriptRoot -Parent) -Parent) "docs/releases/$Tag.md"
  if (Test-Path $notes) { $payload.body = Get-Content -Raw -Encoding utf8 $notes }
  try {
    $r = Invoke-RestMethod -Method Post -Uri "$api/releases" -Headers $h `
           -ContentType 'application/json' -Body ($payload | ConvertTo-Json -Compress)
    return $r.id
  } catch {
    # Almost always: the release already exists. Fetch it by tag; error if that fails too.
    $r = Invoke-RestMethod -Method Get -Uri "$api/releases/tags/$Tag" -Headers $h
    if (-not $r.id) { throw "gitea-release: could not create or find a release for tag '$Tag'" }
    return $r.id
  }
}

# _PutGiteaAsset RELEASEID FILE NAME
#   The raw attach: delete any existing asset of the same name, then POST FILE under NAME.
function _PutGiteaAsset {
  param([string]$ReleaseId, [string]$File, [string]$Name)
  $api = _GiteaApi; $h = _GiteaHeaders
  $assets = Invoke-RestMethod -Method Get -Uri "$api/releases/$ReleaseId/assets" -Headers $h
  $existing = $assets | Where-Object { $_.name -eq $Name } | Select-Object -First 1
  if ($existing) {
    Invoke-RestMethod -Method Delete -Uri "$api/releases/$ReleaseId/assets/$($existing.id)" -Headers $h | Out-Null
  }
  $enc = [uri]::EscapeDataString($Name)
  # curl.exe for the multipart upload — matches the rest of the windows workflows.
  curl.exe -fsS -H "Authorization: token $($env:GITEA_TOKEN)" -o NUL `
    -X POST "$api/releases/$ReleaseId/assets?name=$enc" -F "attachment=@$File"
  if ($LASTEXITCODE -ne 0) { throw "gitea-release: asset upload failed ($LASTEXITCODE): $Name" }
  Write-Output "gitea-release: uploaded '$Name' -> release $ReleaseId"
}

# Upsert-GiteaAsset RELEASEID FILE [NAME]
#   Attach FILE, replacing any existing asset of the same name first (idempotent), plus a
#   `<NAME>.sha256` checksum sidecar so the download is verifiable. Sidecars rather than one
#   shared SHA256SUMS because every platform's workflow attaches to the same release object
#   concurrently — see the bash twin's comment for the full reasoning.
function Upsert-GiteaAsset {
  param(
    [Parameter(Mandatory)] [string]$ReleaseId,
    [Parameter(Mandatory)] [string]$File,
    [string]$Name
  )
  if (-not (Test-Path $File)) { throw "gitea-release: asset file not found: $File" }
  if (-not $Name) { $Name = Split-Path $File -Leaf }
  _PutGiteaAsset -ReleaseId $ReleaseId -File $File -Name $Name
  if ($Name -like '*.sha256') { return }
  # "<digest>  <name>", LF-terminated and written byte-exactly: GNU sha256sum -c treats a trailing
  # CR as part of the filename, so PowerShell's default CRLF output would make every check fail on
  # the Linux/macOS box doing the verifying. The name is the ASSET name, not the local basename.
  $digest = (Get-FileHash -Algorithm SHA256 -LiteralPath $File).Hash.ToLowerInvariant()
  $sums = Join-Path ([IO.Path]::GetTempPath()) ([IO.Path]::GetRandomFileName())
  [IO.File]::WriteAllText($sums, "$digest  $Name`n", (New-Object Text.UTF8Encoding $false))
  try { _PutGiteaAsset -ReleaseId $ReleaseId -File $sums -Name "$Name.sha256" }
  finally { Remove-Item -LiteralPath $sums -Force -ErrorAction SilentlyContinue }
}
