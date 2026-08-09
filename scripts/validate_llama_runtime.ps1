[CmdletBinding()]
param(
    [string]$RepositoryRoot,
    [string]$RuntimeRoot
)

$ErrorActionPreference = 'Stop'
$OutputEncoding = [Console]::OutputEncoding = [Text.Encoding]::UTF8
if ([string]::IsNullOrWhiteSpace($RepositoryRoot)) {
    $RepositoryRoot = Split-Path -Parent $PSScriptRoot
}
if ([string]::IsNullOrWhiteSpace($RuntimeRoot)) {
    $RuntimeRoot = Join-Path $RepositoryRoot '.artifacts\runtime\llama'
}
$runtimeRoot = [System.IO.Path]::GetFullPath($RuntimeRoot)
$manifestPath = Join-Path $runtimeRoot 'MANIFEST.json'
if (-not (Test-Path -LiteralPath $manifestPath -PathType Leaf)) {
    throw '缺少llama.cpp运行时清单；请先执行 scripts/prepare_llama_runtime.ps1'
}
$manifest = Get-Content -LiteralPath $manifestPath -Raw -Encoding UTF8 | ConvertFrom-Json
if ($manifest.platform -ne 'windows-x64-cpu' -or $manifest.release -ne 'b10326') {
    throw "运行时清单版本不受支持：$($manifest.release) / $($manifest.platform)"
}
foreach ($file in $manifest.files) {
    $path = Join-Path $runtimeRoot $file.name
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "运行时文件缺失：$($file.name)"
    }
    $item = Get-Item -LiteralPath $path
    if ($item.Length -ne [int64]$file.size_bytes) {
        throw "运行时文件大小不匹配：$($file.name)"
    }
    $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $path).Hash
    if ($actual -ne $file.sha256) {
        throw "运行时文件SHA-256不匹配：$($file.name)"
    }
}
$server = Join-Path $runtimeRoot 'llama-server.exe'
$startInfo = [Diagnostics.ProcessStartInfo]::new()
$startInfo.FileName = $server
$startInfo.Arguments = '--version'
$startInfo.UseShellExecute = $false
$startInfo.CreateNoWindow = $true
$startInfo.RedirectStandardOutput = $true
$startInfo.RedirectStandardError = $true
$process = [Diagnostics.Process]::Start($startInfo)
$versionOutput = ($process.StandardOutput.ReadToEnd() + $process.StandardError.ReadToEnd()).Trim()
$process.WaitForExit()
if ($process.ExitCode -ne 0 -or $versionOutput -notmatch [regex]::Escape([string]$manifest.commit)) {
    throw "llama-server不可执行或版本错误：$versionOutput"
}
Write-Output "llama.cpp运行时验证通过：release=$($manifest.release) commit=$($manifest.commit) files=$($manifest.files.Count)"
