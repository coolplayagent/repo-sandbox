param(
    [Parameter(Mandatory = $true)]
    [string]$ResultDirectory
)

$ErrorActionPreference = 'Stop'
$distro = $env:REPO_SANDBOX_WSL_DISTRO
if ([string]::IsNullOrWhiteSpace($distro)) {
    throw 'REPO_SANDBOX_WSL_DISTRO must name an installed, disposable EulerOS WSL distribution'
}
$root = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
$linuxRoot = (& wsl.exe --distribution $distro -- wslpath -a -u $root).Trim()
if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($linuxRoot)) {
    throw "could not resolve repository path in WSL distribution $distro"
}
& wsl.exe --distribution $distro -- bash "$linuxRoot/scripts/wsl/smoke-euleros.sh" $linuxRoot
if ($LASTEXITCODE -ne 0) {
    throw "WSL dogfood failed in distribution $distro"
}
New-Item -ItemType Directory -Force -Path $ResultDirectory | Out-Null
Set-Content -LiteralPath (Join-Path $ResultDirectory 'wsl-dogfood.passed') -Value 'passed'
Write-Output 'wsl_dogfood=passed'
