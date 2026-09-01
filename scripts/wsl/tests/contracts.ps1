$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '..\..\..')).Path
$bootstrap = Get-Content -Raw (Join-Path $root 'scripts\wsl\bootstrap-euleros.ps1')
$installer = Get-Content -Raw (Join-Path $root 'scripts\wsl\install-euleros.sh')
$smoke = Get-Content -Raw (Join-Path $root 'scripts\wsl\smoke-euleros.sh')

function Assert-Match([string]$Text, [string]$Pattern, [string]$Message) {
    if ($Text -notmatch $Pattern) { throw $Message }
}

Assert-Match $bootstrap "ValidatePattern\('\^\[A-Fa-f0-9\]\{64\}\$'\)" 'rootfs checksum must be mandatory and syntactically validated'
Assert-Match $bootstrap "Scheme -eq 'https'" 'remote rootfs must require HTTPS'
Assert-Match $bootstrap 'Refusing to replace or modify it' 'existing distros must be protected by default'
Assert-Match $bootstrap '--import.+--version 2' 'bootstrap must import as WSL2'
Assert-Match $installer 'expected exactly \$EXPECTED_VERSION' 'installer must reject a different EulerOS baseline'
Assert-Match $installer '\$\(uname -m\) == x86_64' 'installer must reject non-x86_64 hosts'
Assert-Match $installer 'WSL_INTEROP' 'installer must reject non-WSL hosts'
Assert-Match $installer 'microsoft-standard-WSL2' 'installer must reject WSL1 hosts'
Assert-Match $installer "curl --proto '=https' --tlsv1\.2" 'downloads must enforce HTTPS/TLS'
Assert-Match $installer 'sha256sum' 'downloaded executables must be checksum verified'
Assert-Match $installer 'cmp -s' 'systemd configuration must avoid rewriting unchanged content'
Assert-Match $installer 'systemctl is-active.+\|\| systemctl start' 'Docker start must be state-aware'
Assert-Match $installer 'rpm -q qemu-user-static' 'QEMU installation must be state-aware'
Assert-Match $smoke '--platform linux/amd64' 'acceptance must exercise native amd64'
Assert-Match $smoke '--platform linux/arm64' 'acceptance must exercise emulated arm64'
Assert-Match $smoke '(?s)amd64\).+uname -m.+x86_64.+arm64\).+uname -m.+aarch64' 'Dockerfile must map each target architecture to its correct kernel architecture'
Assert-Match $smoke '(?s)docker run.+--platform linux/amd64.+uname -m.+x86_64' 'runtime smoke must assert amd64 is x86_64'
Assert-Match $smoke '(?s)docker run.+--platform linux/arm64.+uname -m.+aarch64' 'runtime smoke must assert arm64 is aarch64'
Assert-Match $smoke 'io\.repo-sandbox\.smoke\.issue-14' 'temporary images must carry a smoke ownership label'
Assert-Match $smoke 'if \[\[ \$owner == "\$run_id" \]\]' 'cleanup must verify image ownership before removal'
Assert-Match $smoke 'docker image rm -- "\$tag"' 'cleanup must remove only an exact generated tag'
if ($installer -match '(?m)^\s*(curl|wget).+\|\s*(sh|bash)') { throw 'pipe-to-shell downloads are forbidden' }
if ($installer -match 'daemon\.json') { throw 'installer must not overwrite Docker daemon configuration' }

Write-Host 'EulerOS WSL2 static contracts passed.'
