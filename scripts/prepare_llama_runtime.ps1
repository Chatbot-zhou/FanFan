[CmdletBinding()]
param(
    [string]$RepositoryRoot,
    [ValidateSet('Auto', 'Cpu', 'Cuda', 'Vulkan')]
    [string]$Backend = 'Auto'
)

$ErrorActionPreference = 'Stop'
$OutputEncoding = [Console]::OutputEncoding = [Text.Encoding]::UTF8
if ([string]::IsNullOrWhiteSpace($RepositoryRoot)) {
    $RepositoryRoot = Split-Path -Parent $PSScriptRoot
}

$release = 'b10326'
$commit = '3653e6d6d'
$licenseSha256 = '94F29BBED6A22C35B992C5C6EBF0E7C92F13B836B90F36F461C9CF2F0F1D010D'
$licenseUrl = "https://raw.githubusercontent.com/ggml-org/llama.cpp/$release/LICENSE"
$downloadRoot = Join-Path $RepositoryRoot '.artifacts\downloads'
$licenseDownload = Join-Path $downloadRoot "llama.cpp-LICENSE-$release"

function Assert-Sha256([string]$Path, [string]$Expected) {
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        throw "Missing file: $Path"
    }
    $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $Path).Hash
    if ($actual -ne $Expected) {
        throw "SHA-256 mismatch: $Path; expected=$Expected; actual=$actual"
    }
}

function Get-VerifiedDownload([pscustomobject]$Package) {
    $archivePath = Join-Path $downloadRoot $Package.Name
    if (Test-Path -LiteralPath $archivePath -PathType Leaf) {
        $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $archivePath).Hash
        if ($actual -ne $Package.Sha256) {
            $quarantine = "$archivePath.invalid.$([DateTimeOffset]::UtcNow.ToString('yyyyMMddHHmmss'))"
            Move-Item -LiteralPath $archivePath -Destination $quarantine
            Write-Warning "Quarantined an invalid partial archive: $([IO.Path]::GetFileName($quarantine))"
        }
    }
    if (-not (Test-Path -LiteralPath $archivePath -PathType Leaf)) {
        & curl.exe --fail --location --proto '=https' --tlsv1.2 --retry 3 --continue-at - --output $archivePath $Package.Url
        if ($LASTEXITCODE -ne 0) { throw "llama.cpp runtime download failed: $($Package.Name); exit=$LASTEXITCODE" }
    }
    Assert-Sha256 $archivePath $Package.Sha256
    $archivePath
}

function Get-LlamaOutput([string]$Executable, [string]$Arguments) {
    $startInfo = [Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = $Executable
    $startInfo.Arguments = $Arguments
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    $process = [Diagnostics.Process]::Start($startInfo)
    $stdout = $process.StandardOutput.ReadToEnd()
    $stderr = $process.StandardError.ReadToEnd()
    $process.WaitForExit()
    [pscustomobject]@{ ExitCode = $process.ExitCode; Text = ($stdout + $stderr).Trim() }
}

function Resolve-Backend([string]$Requested) {
    if ($Requested -ne 'Auto') { return $Requested }
    $nvidiaSmi = Get-Command nvidia-smi.exe -ErrorAction SilentlyContinue
    if ($null -ne $nvidiaSmi) {
        $nvidiaNames = & $nvidiaSmi.Source --query-gpu=name --format=csv,noheader 2>$null
        if ($LASTEXITCODE -eq 0 -and -not [string]::IsNullOrWhiteSpace(($nvidiaNames -join ''))) {
            return 'Cuda'
        }
    }
    try {
        $gpuNames = (Get-CimInstance Win32_VideoController -ErrorAction Stop | Select-Object -ExpandProperty Name) -join ' '
        if ($gpuNames -match 'AMD|Radeon|Intel|Arc') { return 'Vulkan' }
    } catch {
        Write-Verbose "WMI graphics detection failed; using the CPU runtime: $($_.Exception.Message)"
    }
    'Cpu'
}

$selectedBackend = Resolve-Backend $Backend
$baseUrl = "https://github.com/ggml-org/llama.cpp/releases/download/$release"
$systemCudaBin = if ($env:CUDA_PATH) { Join-Path $env:CUDA_PATH 'bin' } else { $null }
$systemCudaAvailable = $selectedBackend -eq 'Cuda' -and $systemCudaBin -and
    (Test-Path -LiteralPath (Join-Path $systemCudaBin 'cudart64_12.dll') -PathType Leaf) -and
    (Test-Path -LiteralPath (Join-Path $systemCudaBin 'cublas64_12.dll') -PathType Leaf) -and
    (Test-Path -LiteralPath (Join-Path $systemCudaBin 'cublasLt64_12.dll') -PathType Leaf)
$packages = switch ($selectedBackend) {
    'Cuda' {
        $cudaPackages = @([pscustomobject]@{ Name = "llama-$release-bin-win-cuda-12.4-x64.zip"; Sha256 = '62B4CB0D65CE95561F23D662BD7E5A568F462E24254B24B5B4F8C9E71EA692A7'; Url = "$baseUrl/llama-$release-bin-win-cuda-12.4-x64.zip" })
        if (-not $systemCudaAvailable) {
            $cudaPackages += [pscustomobject]@{ Name = 'cudart-llama-bin-win-cuda-12.4-x64.zip'; Sha256 = '8C79A9B226DE4B3CACFD1F83D24F962D0773BE79F1E7B75C6AF4DED7E32AE1D6'; Url = "$baseUrl/cudart-llama-bin-win-cuda-12.4-x64.zip" }
        }
        $cudaPackages
    }
    'Vulkan' {
        @([pscustomobject]@{ Name = "llama-$release-bin-win-vulkan-x64.zip"; Sha256 = '3066BDBC866E424E6F6DF0AC1883CEE67B435A08CD1879353ABD70083A2EA939'; Url = "$baseUrl/llama-$release-bin-win-vulkan-x64.zip" })
    }
    default {
        @([pscustomobject]@{ Name = "llama-$release-bin-win-cpu-x64.zip"; Sha256 = 'DABFF645E0948FEAE41EE6C8E46F2C12DFFEE96B0CB050E850DA7D6B3932F56D'; Url = "$baseUrl/llama-$release-bin-win-cpu-x64.zip" })
    }
}
$platform = "windows-x64-$($selectedBackend.ToLowerInvariant())"
$runtimeFolder = if ($selectedBackend -eq 'Cpu') { 'llama' } else { "llama-$($selectedBackend.ToLowerInvariant())" }
$runtimeRoot = Join-Path $RepositoryRoot ".artifacts\runtime\$runtimeFolder"

New-Item -ItemType Directory -Force -Path $downloadRoot, $runtimeRoot | Out-Null
foreach ($package in $packages) {
    $archivePath = Get-VerifiedDownload $package
    $extractRoot = Join-Path $downloadRoot ([IO.Path]::GetFileNameWithoutExtension($package.Name))
    $marker = Join-Path $extractRoot ".verified-$($package.Sha256)"
    if (-not (Test-Path -LiteralPath $marker -PathType Leaf)) {
        New-Item -ItemType Directory -Force -Path $extractRoot | Out-Null
        Expand-Archive -LiteralPath $archivePath -DestinationPath $extractRoot -Force
        New-Item -ItemType File -Force -Path $marker | Out-Null
    }
    Get-ChildItem -LiteralPath $extractRoot -Recurse -File |
        Where-Object { $_.Name -notlike '.verified-*' } |
        Copy-Item -Destination $runtimeRoot -Force
}

if (-not (Test-Path -LiteralPath $licenseDownload -PathType Leaf)) {
    & curl.exe --fail --location --proto '=https' --tlsv1.2 --retry 3 --output $licenseDownload $licenseUrl
    if ($LASTEXITCODE -ne 0) { throw "llama.cpp license download failed: exit=$LASTEXITCODE" }
}
Assert-Sha256 $licenseDownload $licenseSha256
Copy-Item -LiteralPath $licenseDownload -Destination (Join-Path $runtimeRoot 'LICENSE-llama.cpp.txt') -Force

$server = Join-Path $runtimeRoot 'llama-server.exe'
$version = Get-LlamaOutput $server '--version'
if ($version.ExitCode -ne 0 -or $version.Text -notmatch [regex]::Escape($commit)) {
    throw "llama-server version self-test failed: $($version.Text)"
}
$devices = Get-LlamaOutput $server '--list-devices'
if ($devices.ExitCode -ne 0) {
    throw "llama-server device probe failed: $($devices.Text)"
}
if ($selectedBackend -ne 'Cpu' -and ($devices.Text -match '\(none\)' -or $devices.Text -notmatch 'CUDA|Vulkan|NVIDIA|AMD|Intel')) {
    throw "$selectedBackend runtime did not identify a usable GPU: $($devices.Text)"
}

$files = Get-ChildItem -LiteralPath $runtimeRoot -File |
    Where-Object { $_.Name -notin @('MANIFEST.json', 'README.txt') } |
    Sort-Object Name |
    ForEach-Object {
        [ordered]@{
            name = $_.Name
            size_bytes = $_.Length
            sha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $_.FullName).Hash
        }
    }
$manifest = [ordered]@{
    component = 'llama.cpp'
    release = $release
    commit = $commit
    platform = $platform
    backend = $selectedBackend.ToLowerInvariant()
    system_cuda_runtime = [bool]$systemCudaAvailable
    source_packages = @($packages | ForEach-Object { [ordered]@{ name = $_.Name; url = $_.Url; archive_sha256 = $_.Sha256 } })
    license = 'MIT'
    license_sha256 = $licenseSha256
    devices = @($devices.Text -split "`r?`n" | Where-Object { $_ -and $_ -notmatch '^Available devices:$' })
    generated_at = [DateTimeOffset]::UtcNow.ToString('o')
    files = @($files)
}
$manifest | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath (Join-Path $runtimeRoot 'MANIFEST.json') -Encoding UTF8
@(
    "This directory is managed by FanFan for the official llama.cpp Windows x64 $selectedBackend runtime.",
    "Prepare: powershell -ExecutionPolicy Bypass -File scripts/prepare_llama_runtime.ps1 -Backend $selectedBackend",
    'Validate: powershell -ExecutionPolicy Bypass -File scripts/validate_llama_runtime.ps1 -RuntimeRoot <this-directory>',
    '',
    'The pinned release, sources, archive SHA-256 values, license hash, and per-file hashes are recorded in MANIFEST.json.'
) | Set-Content -LiteralPath (Join-Path $runtimeRoot 'README.txt') -Encoding UTF8

Write-Output "llama.cpp runtime prepared: release=$release commit=$commit backend=$selectedBackend files=$($files.Count)"
Write-Output $devices.Text
