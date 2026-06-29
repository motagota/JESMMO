# Stops the proxy and zone servers started by start_servers.ps1.

$ErrorActionPreference = 'SilentlyContinue'

$stopped = 0
foreach ($name in @('proxy', 'zone_server', 'bots')) {
    Get-Process -Name $name | ForEach-Object {
        Stop-Process -Id $_.Id -Force
        $stopped++
    }
}

Write-Host "Stopped $stopped process(es)." -ForegroundColor Cyan
