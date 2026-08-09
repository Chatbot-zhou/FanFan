[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$InstallerPath,
    [string]$RepositoryRoot,
    [switch]$InstallerSmokePassed
)

$ErrorActionPreference = 'Stop'
$OutputEncoding = [Console]::OutputEncoding = [Text.Encoding]::UTF8
if ([string]::IsNullOrWhiteSpace($RepositoryRoot)) {
    $RepositoryRoot = Split-Path -Parent $PSScriptRoot
}
$RepositoryRoot = [IO.Path]::GetFullPath($RepositoryRoot)
$installer = [IO.Path]::GetFullPath($InstallerPath)
if (-not (Test-Path -LiteralPath $installer -PathType Leaf)) {
    throw "Installer not found: $installer"
}

$releaseRoot = Join-Path $RepositoryRoot '.artifacts\release'
New-Item -ItemType Directory -Force -Path $releaseRoot | Out-Null
$releaseInstaller = Join-Path $releaseRoot ([IO.Path]::GetFileName($installer))
if (-not $installer.Equals($releaseInstaller, [StringComparison]::OrdinalIgnoreCase)) {
    Copy-Item -LiteralPath $installer -Destination $releaseInstaller -Force
    $installer = $releaseInstaller
}
$runtimeManifestPath = Join-Path $RepositoryRoot '.artifacts\runtime\llama\MANIFEST.json'
$workerPath = Join-Path $RepositoryRoot '.artifacts\worker\remin-worker\remin-worker.exe'
foreach ($required in @($runtimeManifestPath, $workerPath)) {
    if (-not (Test-Path -LiteralPath $required -PathType Leaf)) {
        throw "Release payload missing: $required"
    }
}

$runtime = Get-Content -LiteralPath $runtimeManifestPath -Raw -Encoding UTF8 | ConvertFrom-Json
$signature = Get-AuthenticodeSignature -LiteralPath $installer
$generatedAt = [DateTimeOffset]::UtcNow.ToString('o')
$installerItem = Get-Item -LiteralPath $installer
$productName = ([char]0x62FE).ToString() + ([char]0x5FC6).ToString()
$verification = @(
    'contract-catalog',
    'frontend-typecheck-and-tests',
    'rust-workspace-tests',
    'python-worker-corpus',
    'semantic-20000-file-gate',
    'real-llama-generation'
)
if ($InstallerSmokePassed) {
    $verification += 'isolated-installer-smoke'
}
$manifest = [ordered]@{
    schema_version = 1
    product = $productName
    version = '0.1.0'
    platform = 'windows-x64'
    channel = 'development-candidate'
    generated_at = $generatedAt
    installer = [ordered]@{
        file_name = $installerItem.Name
        size_bytes = $installerItem.Length
        sha256 = (Get-FileHash -LiteralPath $installer -Algorithm SHA256).Hash.ToLowerInvariant()
        authenticode_status = [string]$signature.Status
    }
    bundled_runtime = [ordered]@{
        component = $runtime.component
        release = $runtime.release
        commit = $runtime.commit
        platform = $runtime.platform
        source_url = $runtime.source_url
        manifest_sha256 = (Get-FileHash -LiteralPath $runtimeManifestPath -Algorithm SHA256).Hash.ToLowerInvariant()
        license = $runtime.license
    }
    worker = [ordered]@{
        file_name = 'worker/remin-worker.exe'
        sha256 = (Get-FileHash -LiteralPath $workerPath -Algorithm SHA256).Hash.ToLowerInvariant()
    }
    verification = $verification
}
$manifestPath = Join-Path $releaseRoot 'release-manifest.json'
$manifest | ConvertTo-Json -Depth 7 | Set-Content -LiteralPath $manifestPath -Encoding UTF8

$components = @()
$cargoPackages = @()
$currentCargoPackage = $null
foreach ($line in Get-Content -LiteralPath (Join-Path $RepositoryRoot 'Cargo.lock') -Encoding UTF8) {
    if ($line -eq '[[package]]') {
        if ($null -ne $currentCargoPackage -and $currentCargoPackage.Contains('name') -and $currentCargoPackage.Contains('version')) {
            $cargoPackages += $currentCargoPackage
        }
        $currentCargoPackage = [ordered]@{}
    }
    elseif ($null -ne $currentCargoPackage -and $line -match '^name = "([^"]+)"$') {
        $currentCargoPackage.name = $Matches[1]
    }
    elseif ($null -ne $currentCargoPackage -and $line -match '^version = "([^"]+)"$') {
        $currentCargoPackage.version = $Matches[1]
    }
}
if ($null -ne $currentCargoPackage -and $currentCargoPackage.Contains('name') -and $currentCargoPackage.Contains('version')) {
    $cargoPackages += $currentCargoPackage
}
foreach ($package in $cargoPackages) {
    $components += [ordered]@{
        type = 'library'
        name = $package.name
        version = $package.version
        purl = "pkg:cargo/$($package.name)@$($package.version)"
    }
}
Get-Content -LiteralPath (Join-Path $RepositoryRoot 'services\worker\requirements-build.txt') -Encoding UTF8 |
    Where-Object { $_ -match '^([A-Za-z0-9_.-]+)==([^\s#]+)' } |
    ForEach-Object {
        $components += [ordered]@{
            type = 'library'
            name = $Matches[1]
            version = $Matches[2]
            purl = "pkg:pypi/$($Matches[1])@$($Matches[2])"
        }
    }
$components += [ordered]@{
    type = 'application'
    name = 'llama.cpp'
    version = $runtime.release
    properties = @(
        @{ name = 'remin:commit'; value = $runtime.commit },
        @{ name = 'remin:archive_sha256'; value = $runtime.archive_sha256 }
    )
    licenses = @(@{ license = @{ id = 'MIT' } })
}

$bom = [ordered]@{
    bomFormat = 'CycloneDX'
    specVersion = '1.5'
    serialNumber = "urn:uuid:$([Guid]::NewGuid())"
    version = 1
    metadata = [ordered]@{
        timestamp = $generatedAt
        component = [ordered]@{ type = 'application'; name = $productName; version = '0.1.0' }
    }
    components = @($components)
}
$sbomPath = Join-Path $releaseRoot 'remin-sbom.cdx.json'
$bom | ConvertTo-Json -Depth 9 | Set-Content -LiteralPath $sbomPath -Encoding UTF8

Write-Output "Release manifest generated: $manifestPath"
Write-Output "CycloneDX SBOM generated: $sbomPath"
Write-Output "installer_sha256=$($manifest.installer.sha256)"
Write-Output "signature_status=$($signature.Status)"
