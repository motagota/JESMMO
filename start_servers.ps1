# Starts the proxy and two zone servers, each in its own console window.
#
# Each process gets its own window so its logs are visible and the proxy keeps a
# live stdin for migration commands (migrate phase1/2/3/auto ...).
#
# Usage:
#   .\start_servers.ps1            # build (debug) then start
#   .\start_servers.ps1 -NoBuild   # skip the build, just start
#   .\start_servers.ps1 -Release   # build + run the release binaries

param(
    [switch]$NoBuild,
    [switch]$Release,
    [int]$Bots = 6
)

$ErrorActionPreference = 'Stop'
$root = $PSScriptRoot
$server = Join-Path $root 'rust_server'

$profile = if ($Release) { 'release' } else { 'debug' }

if (-not $NoBuild) {
    Write-Host 'Building binaries...' -ForegroundColor Cyan
    Push-Location $server
    try {
        if ($Release) { cargo build --release --bins } else { cargo build --bins }
        if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }
    } finally {
        Pop-Location
    }
}

# Binaries land in the repo-root workspace target dir (see the root Cargo.toml),
# not rust_server\target — pointing there would launch stale pre-workspace builds.
$bin = Join-Path $root "target\$profile"
$proxyExe = Join-Path $bin 'proxy.exe'
$zoneExe = Join-Path $bin 'zone_server.exe'
$botsExe = Join-Path $bin 'bots.exe'

foreach ($exe in @($proxyExe, $zoneExe, $botsExe)) {
    if (-not (Test-Path $exe)) { throw "Missing binary: $exe (build first, or drop -NoBuild)" }
}

# Proxy first so the zone has something to register with.
Write-Host 'Starting proxy (ports 8766 client / 8764 registration / 8767 admin)...' -ForegroundColor Green
Start-Process -FilePath $proxyExe -WorkingDirectory $server
Start-Sleep -Milliseconds 800

# A single zone owning the whole 1200x1200 world; the gateway auto-splits it
# into shards as the population grows.
Write-Host 'Starting zone_a on 9001 (owns the whole world; auto-splits on load)...' -ForegroundColor Green
Start-Process -FilePath $zoneExe -WorkingDirectory $server `
    -ArgumentList 'zone_a', '9001', 'ws://127.0.0.1:8764'
Start-Sleep -Milliseconds 400

if ($Bots -gt 0) {
    Start-Sleep -Milliseconds 600
    Write-Host "Starting $Bots simulated player bots..." -ForegroundColor Green
    Start-Process -FilePath $botsExe -WorkingDirectory $server `
        -ArgumentList 'ws://127.0.0.1:8766', "$Bots"
}

Write-Host ''
Write-Host 'All processes started in separate windows.' -ForegroundColor Cyan
Write-Host 'Open client\client.html and client\admin.html in a browser.'
Write-Host 'Stop everything with: .\stop_servers.ps1'
