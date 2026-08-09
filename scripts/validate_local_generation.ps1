[CmdletBinding()]
param(
    [string]$RepositoryRoot,
    [string]$ModelPath
)

$ErrorActionPreference = 'Stop'
$OutputEncoding = [Console]::OutputEncoding = [Text.Encoding]::UTF8
if ([string]::IsNullOrWhiteSpace($RepositoryRoot)) {
    $RepositoryRoot = Split-Path -Parent $PSScriptRoot
}
if ([string]::IsNullOrWhiteSpace($ModelPath)) {
    $ModelPath = Join-Path $RepositoryRoot '.artifacts\test-models\Qwen3-0.6B-Q8_0.gguf'
}
$server = Join-Path $RepositoryRoot '.artifacts\runtime\llama\llama-server.exe'
if (-not (Test-Path -LiteralPath $server -PathType Leaf)) {
    throw '缺少llama-server.exe；请先执行 scripts/prepare_llama_runtime.ps1'
}
if (-not (Test-Path -LiteralPath $ModelPath -PathType Leaf)) {
    throw "缺少轻量验收模型：$ModelPath"
}
$model = Get-Item -LiteralPath $ModelPath
$modelHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $ModelPath).Hash
if ($model.Length -ne 639446688 -or $modelHash -ne '9465E63A22ADD5354D9BB4B99E90117043C7124007664907259BD16D043BB031') {
    throw "轻量验收模型大小或SHA-256不匹配：size=$($model.Length) sha256=$modelHash"
}
$env:PATH = (Join-Path $env:USERPROFILE '.cargo\bin') + ';' + $env:PATH
$env:CARGO_TARGET_DIR = Join-Path $RepositoryRoot '.artifacts\cargo-target'
$env:REMIN_LLAMA_SERVER = $server
$env:REMIN_TEST_GGUF = $ModelPath
Push-Location $RepositoryRoot
try {
    & cargo test -p remin-core --locked real_llama_cpp_runtime_generates_locally -- --ignored --nocapture
    if ($LASTEXITCODE -ne 0) { throw "本地生成E2E失败：exit=$LASTEXITCODE" }
}
finally {
    Pop-Location
}
