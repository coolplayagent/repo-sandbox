[CmdletBinding()]
param(
    [Parameter(Mandatory = $true, ParameterSetName = 'Local')]
    [ValidateScript({ Test-Path -LiteralPath $_ -PathType Leaf })]
    [string]$RootfsPath,

    [Parameter(Mandatory = $true, ParameterSetName = 'Download')]
    [ValidateScript({ $_.Scheme -eq 'https' })]
    [uri]$RootfsUri,

    [Parameter(Mandatory = $true)]
    [ValidatePattern('^[A-Fa-f0-9]{64}$')]
    [string]$RootfsSha256,

    [ValidatePattern('^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$')]
    [string]$DistroName = 'EulerOS-2.10.7',

    [string]$InstallLocation = (Join-Path $env:LOCALAPPDATA 'repo-sandbox\wsl\EulerOS-2.10.7'),
    [string]$RepositoryPath = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path,
    [switch]$UseExisting
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Invoke-Wsl {
    param([Parameter(Mandatory = $true)][string[]]$Arguments)
    & wsl.exe @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "wsl.exe failed with exit code $LASTEXITCODE"
    }
}

if (-not (Get-Command wsl.exe -ErrorAction SilentlyContinue)) {
    throw 'WSL is not installed. Enable the Windows Subsystem for Linux and Virtual Machine Platform features first.'
}

$existing = @(& wsl.exe --list --quiet) | ForEach-Object { $_.Trim([char]0).Trim() } | Where-Object { $_ }
if ($LASTEXITCODE -ne 0) { throw 'Unable to enumerate WSL distributions.' }
$alreadyExists = $existing -contains $DistroName
if ($alreadyExists -and -not $UseExisting) {
    throw "Distribution '$DistroName' already exists. Refusing to replace or modify it; pass -UseExisting after verifying it is the intended EulerOS instance."
}

$downloadedRootfs = $null
try {
    if (-not $alreadyExists) {
        if ($PSCmdlet.ParameterSetName -eq 'Download') {
            $downloadedRootfs = Join-Path ([System.IO.Path]::GetTempPath()) ("repo-sandbox-euleros-{0}.tar" -f [guid]::NewGuid())
            Invoke-WebRequest -Uri $RootfsUri -OutFile $downloadedRootfs -UseBasicParsing
            $resolvedRootfs = $downloadedRootfs
        } else {
            $resolvedRootfs = (Resolve-Path -LiteralPath $RootfsPath).Path
        }

        $actualHash = (Get-FileHash -LiteralPath $resolvedRootfs -Algorithm SHA256).Hash
        if ($actualHash -ine $RootfsSha256) {
            throw "EulerOS rootfs SHA-256 mismatch: expected $RootfsSha256, got $actualHash"
        }

        New-Item -ItemType Directory -Path $InstallLocation -Force | Out-Null
        & wsl.exe --import $DistroName $InstallLocation $resolvedRootfs --version 2
        if ($LASTEXITCODE -ne 0) { throw "Failed to import '$DistroName' as WSL2." }
    }

    $linuxRepositoryPath = (& wsl.exe --distribution $DistroName --user root --exec wslpath -a $RepositoryPath)
    if ($LASTEXITCODE -ne 0 -or $linuxRepositoryPath.Count -ne 1 -or -not $linuxRepositoryPath.StartsWith('/')) {
        throw 'Could not map the repository path into the selected WSL distribution.'
    }
    $installer = "$linuxRepositoryPath/scripts/wsl/install-euleros.sh"

    # Arguments are passed directly to wsl.exe/exec; none are evaluated by a shell.
    & wsl.exe --distribution $DistroName --user root --exec bash $installer --source $linuxRepositoryPath --configure-systemd-only
    if ($LASTEXITCODE -ne 0) { throw 'EulerOS identity/systemd preflight failed.' }

    # A WSL restart is required only when the installer changed /etc/wsl.conf.
    $restartMarker = (& wsl.exe --distribution $DistroName --user root --exec test -f /run/repo-sandbox-wsl-restart-required)
    $markerExit = $LASTEXITCODE
    if ($markerExit -eq 0) {
        Invoke-Wsl -Arguments @('--terminate', $DistroName)
    } elseif ($markerExit -ne 1) {
        throw 'Could not determine whether the WSL instance requires a restart.'
    }

    & wsl.exe --distribution $DistroName --user root --exec bash $installer --source $linuxRepositoryPath
    if ($LASTEXITCODE -ne 0) { throw 'EulerOS installation failed.' }
    Write-Host "Installed repo-sandbox in WSL2 distribution '$DistroName'. Run: wsl.exe -d $DistroName -- repo-sandbox doctor"
} finally {
    if ($downloadedRootfs -and (Test-Path -LiteralPath $downloadedRootfs)) {
        Remove-Item -LiteralPath $downloadedRootfs -Force
    }
}
