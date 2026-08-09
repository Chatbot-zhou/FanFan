[CmdletBinding()]
param(
    [string]$RepositoryRoot
)

$ErrorActionPreference = 'Stop'
$OutputEncoding = [Console]::OutputEncoding = [Text.Encoding]::UTF8
if ([string]::IsNullOrWhiteSpace($RepositoryRoot)) {
    $RepositoryRoot = Split-Path -Parent $PSScriptRoot
}
$release = 'b10326'
$commit = '3653e6d6d'
$archiveName = "llama-$release-bin-win-cpu-x64.zip"
$archiveSha256 = 'DABFF645E0948FEAE41EE6C8E46F2C12DFFEE96B0CB050E850DA7D6B3932F56D'
$licenseSha256 = '94F29BBED6A22C35B992C5C6EBF0E7C92F13B836B90F36F461C9CF2F0F1D010D'
$sourceUrl = "https://github.com/ggml-org/llama.cpp/releases/download/$release/$archiveName"
$licenseUrl = "https://raw.githubusercontent.com/ggml-org/llama.cpp/$release/LICENSE"
$downloadRoot = Join-Path $RepositoryRoot '.artifacts\downloads'
$archivePath = Join-Path $downloadRoot $archiveName
$extractRoot = Join-Path $downloadRoot ([IO.Path]::GetFileNameWithoutExtension($archiveName))
$runtimeRoot = Join-Path $RepositoryRoot '.artifacts\runtime\llama'
$licenseDownload = Join-Path $downloadRoot "llama.cpp-LICENSE-$release"

New-Item -ItemType Directory -Force -Path $downloadRoot, $runtimeRoot | Out-Null

function Assert-Sha256([string]$Path, [string]$Expected) {
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        throw "缺少文件：$Path"
    }
    $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $Path).Hash
    if ($actual -ne $Expected) {
        throw "SHA-256不匹配：$Path；期望=$Expected；实际=$actual"
    }
}

function Get-LlamaVersion([string]$Executable) {
    $startInfo = [Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = $Executable
    $startInfo.Arguments = '--version'
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

if (-not (Test-Path -LiteralPath $archivePath -PathType Leaf)) {
    & curl.exe --fail --location --proto '=https' --tlsv1.2 --retry 3 --continue-at - --output $archivePath $sourceUrl
    if ($LASTEXITCODE -ne 0) { throw "llama.cpp运行时下载失败：exit=$LASTEXITCODE" }
}
Assert-Sha256 $archivePath $archiveSha256

if (-not (Test-Path -LiteralPath $licenseDownload -PathType Leaf)) {
    & curl.exe --fail --location --proto '=https' --tlsv1.2 --retry 3 --output $licenseDownload $licenseUrl
    if ($LASTEXITCODE -ne 0) { throw "llama.cpp许可证下载失败：exit=$LASTEXITCODE" }
}
Assert-Sha256 $licenseDownload $licenseSha256

if (-not (Test-Path -LiteralPath (Join-Path $extractRoot 'llama-server.exe') -PathType Leaf)) {
    Expand-Archive -LiteralPath $archivePath -DestinationPath $extractRoot -Force
}
Get-ChildItem -LiteralPath $extractRoot -File | Copy-Item -Destination $runtimeRoot -Force
Copy-Item -LiteralPath $licenseDownload -Destination (Join-Path $runtimeRoot 'LICENSE-llama.cpp.txt') -Force

$server = Join-Path $runtimeRoot 'llama-server.exe'
$version = Get-LlamaVersion $server
if ($version.ExitCode -ne 0 -or $version.Text -notmatch [regex]::Escape($commit)) {
    throw "llama-server版本自检失败：$($version.Text)"
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
    platform = 'windows-x64-cpu'
    source_url = $sourceUrl
    archive_sha256 = $archiveSha256
    license = 'MIT'
    license_sha256 = $licenseSha256
    generated_at = [DateTimeOffset]::UtcNow.ToString('o')
    files = @($files)
}
$manifest | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $runtimeRoot 'MANIFEST.json') -Encoding UTF8

Write-Output "llama.cpp运行时已准备：release=$release commit=$commit files=$($files.Count)"
Write-Output $version.Text
