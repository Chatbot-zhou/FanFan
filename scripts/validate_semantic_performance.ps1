[CmdletBinding()]
param(
    [uint32]$P95LimitMs = 2000,
    [string]$RepositoryRoot
)

$ErrorActionPreference = 'Stop'
if ([string]::IsNullOrWhiteSpace($RepositoryRoot)) {
    $RepositoryRoot = Split-Path -Parent $PSScriptRoot
}
$env:PATH = (Join-Path $env:USERPROFILE '.cargo\bin') + ';' + $env:PATH
$env:CARGO_TARGET_DIR = Join-Path $RepositoryRoot '.artifacts\cargo-target'
$env:REMIN_SEMANTIC_P95_MS = $P95LimitMs.ToString()
Push-Location $RepositoryRoot
try {
    & cargo test -p remin-core --locked --release semantic_search_twenty_thousand_file_gate -- --ignored --nocapture
    if ($LASTEXITCODE -ne 0) { throw "20,000文件语义检索性能门禁失败：exit=$LASTEXITCODE" }
}
finally {
    Pop-Location
}
