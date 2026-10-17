# Installs the MSIX built for the current version and starts Coffee Filter.
#
# Output: the installed version, and whether the app and its core are running.
# Used by: test.ps1 (UI tests run against the installed app), and by hand after build.ps1.
# Needs: build.ps1 run first. Closes a running Coffee Filter to replace it.
#
# Windows only installs a changed package under a higher version: bump the fourth number in
# app\CoffeeFilter.csproj and app\Package.appxmanifest before building (see CLAUDE.md).

$ErrorActionPreference = 'Stop'
$root    = Split-Path $PSScriptRoot -Parent
$version = ([xml](Get-Content "$root\app\Package.appxmanifest")).Package.Identity.Version
$msix    = "$root\app\AppPackages\CoffeeFilter_${version}_x64_Test\CoffeeFilter_${version}_x64.msix"
if (-not (Test-Path $msix)) { throw "no package for $version; run scripts\build.ps1 first" }

Add-AppxPackage $msix -ForceApplicationShutdown
$family = (Get-AppxPackage Artistro08.CoffeeFilter).PackageFamilyName
Start-Process "shell:AppsFolder\$family!App"

# The core loads its filter lists before the window shows them
Start-Sleep 10
"installed $((Get-AppxPackage Artistro08.CoffeeFilter).Version)"
Get-Process CoffeeFilter, coffee-filter-core -ErrorAction SilentlyContinue | Select-Object Name, Responding | Format-Table -AutoSize
