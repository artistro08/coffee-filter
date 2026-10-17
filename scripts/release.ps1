# Publishes a GitHub release, only after every test, UI tests included, has passed.
#
# Output: the release URL, with the signed installer and the public signing certificate attached.
# Used by: releasing a version the owner asked for (see CLAUDE.md, "Releasing").
# Needs: GitHub CLI signed in (https://cli.github.com/), the signing certificate in
#        CurrentUser\My, a clean working tree pushed to main, and notes for the release.
#
#   scripts\release.ps1 -Notes path\to\notes.md
#
# The tag and name use three numbers (v0.1.1, "Coffee Filter 0.1.1"); the fourth number in
# the package version is never shown.

param([Parameter(Mandatory)][string]$Notes)

$ErrorActionPreference = 'Stop'
$root       = Split-Path $PSScriptRoot -Parent
$thumbprint = '731870AC454C0A65DBFBF7C72B6A28CA10FB346B'

# Release Gate: nothing ships unless everything passes
& "$PSScriptRoot\test.ps1"
if ($LASTEXITCODE -ne 0) { throw 'tests failed; no release' }

# What's released must be what's on GitHub
Push-Location $root
try {
    if (git status --porcelain) { throw 'uncommitted changes; commit and push first' }
    git fetch -q origin
    if ((git rev-parse HEAD) -ne (git rev-parse origin/main)) { throw 'main is not pushed; push first' }
}
finally { Pop-Location }

$full    = ([xml](Get-Content "$root\app\Package.appxmanifest")).Package.Identity.Version
$version = ($full -split '\.')[0..2] -join '.'
$msix    = "$root\app\AppPackages\CoffeeFilter_${full}_x64_Test\CoffeeFilter_${full}_x64.msix"
if ((Get-AuthenticodeSignature $msix).Status -ne 'Valid') { throw "$msix isn't validly signed" }

# Release files, named without the fourth number
$out = Join-Path ([IO.Path]::GetTempPath()) "coffee-filter-release-$version"
New-Item -ItemType Directory -Force $out | Out-Null
Copy-Item $msix "$out\CoffeeFilter_${version}_x64.msix" -Force
Export-Certificate -Cert "Cert:\CurrentUser\My\$thumbprint" -FilePath "$out\CoffeeFilter-signing.cer" -Type CERT | Out-Null

gh release create "v$version" --repo artistro08/coffee-filter --target main --title "Coffee Filter $version" --notes-file $Notes "$out\CoffeeFilter_${version}_x64.msix" "$out\CoffeeFilter-signing.cer"
if ($LASTEXITCODE -ne 0) { throw 'gh release create failed' }
