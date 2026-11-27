# Installs the MSIX built for the current version and starts Coffee Filter.
#
# Output: the installed version, and whether the app and its core are running.
# Used by: test.ps1 (UI tests run against the installed app), and by hand after build.ps1.
# Needs: build.ps1 run first. Closes a running Coffee Filter to replace it.
#
# Windows only installs a changed package under a higher version: bump the fourth number in
# app\Package.appxmanifest before building (see CLAUDE.md).

$ErrorActionPreference = 'Stop'
$root    = Split-Path $PSScriptRoot -Parent
$version = ([xml](Get-Content "$root\app\Package.appxmanifest")).Package.Identity.Version
$msix    = "$root\app\AppPackages\CoffeeFilter_${version}_x64_Test\CoffeeFilter_${version}_x64.msix"
if (-not (Test-Path $msix)) { throw "no package for $version; run scripts\build.ps1 first" }

Add-AppxPackage $msix -ForceApplicationShutdown
$family = (Get-AppxPackage Artistro08.CoffeeFilter).PackageFamilyName
Start-Process "shell:AppsFolder\$family!App"

# Wait for the core to start (up to 30 seconds), then give it a moment to load its filter lists
$deadline = (Get-Date).AddSeconds(30)
while (-not (Get-Process coffee-filter-core -ErrorAction SilentlyContinue) -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 250 }
if (-not (Get-Process coffee-filter-core -ErrorAction SilentlyContinue)) { throw 'the filter core didn''t start within 30 seconds' }
Start-Sleep 2
"installed $((Get-AppxPackage Artistro08.CoffeeFilter).Version)"
Get-Process CoffeeFilter, coffee-filter-core -ErrorAction SilentlyContinue | Select-Object Name, Responding | Format-Table -AutoSize
