# Packages the Windows client into two archives:
#   verbarelay-client-<label>-windows-x86_64.zip  executable + engine + CPU/Vulkan backends
#   verbarelay-cuda-<label>-windows-x86_64.zip    CUDA backend + CUDA runtime + cuBLAS
#
# Usage: scripts/package-client.ps1 -Label 1.2.3 -Binary target\release\verbarelay-client.exe `
#          -Engine dist\engine\bin -CudaBin "$env:CUDA_PATH\bin" -Out dist\release
param(
    [Parameter(Mandatory)] [string] $Label,
    [Parameter(Mandatory)] [string] $Binary,
    [Parameter(Mandatory)] [string] $Engine,
    [string] $CudaBin = "",
    [Parameter(Mandatory)] [string] $Out
)
$ErrorActionPreference = "Stop"

$core = Join-Path $env:RUNNER_TEMP "wm-core"
$cuda = Join-Path $env:RUNNER_TEMP "wm-cuda"
Remove-Item -Recurse -Force $core, $cuda -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force $core, $cuda, $Out | Out-Null

Copy-Item $Binary (Join-Path $core "verbarelay-client.exe")
Get-ChildItem $Engine -Filter *.dll | Where-Object { $_.Name -ne "ggml-cuda.dll" } | Copy-Item -Destination $core
Copy-Item LICENSE, THIRD-PARTY-NOTICES.md $core
# The native engine uses the shared Visual C++ runtime; ship it next to the
# executable (app-local deployment) so no redistributable has to be installed.
foreach ($dll in "msvcp140.dll", "vcruntime140.dll", "vcruntime140_1.dll") {
    $source = Join-Path $env:SystemRoot "System32\$dll"
    if (Test-Path $source) { Copy-Item $source $core }
}

$cudaModule = Join-Path $Engine "ggml-cuda.dll"
if (Test-Path $cudaModule) {
    Copy-Item $cudaModule $cuda
    foreach ($pattern in "cudart64_*.dll", "cublas64_*.dll", "cublasLt64_*.dll") {
        Get-ChildItem $CudaBin -Filter $pattern | Copy-Item -Destination $cuda
    }
}

$outDir = (Resolve-Path $Out).Path
Compress-Archive -Path (Join-Path $core "*") -DestinationPath (Join-Path $outDir "verbarelay-client-$Label-windows-x86_64.zip") -Force
if ((Get-ChildItem $cuda).Count -gt 0) {
    Compress-Archive -Path (Join-Path $cuda "*") -DestinationPath (Join-Path $outDir "verbarelay-cuda-$Label-windows-x86_64.zip") -Force
}
Get-ChildItem $outDir
