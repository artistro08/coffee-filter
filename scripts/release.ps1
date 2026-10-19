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
$root = Split-Path $PSScriptRoot -Parent
if (-not (Test-Path $Notes)) { throw "no release notes at $Notes" }

# The quick checks first, so a mistake doesn't wait for the whole test suite
$csproj     = [xml](Get-Content "$root\app\CoffeeFilter.csproj")
$full       = ([xml](Get-Content "$root\app\Package.appxmanifest")).Package.Identity.Version
$version    = ($full -split '\.')[0..2] -join '.'
$thumbprint = @($csproj.Project.PropertyGroup.PackageCertificateThumbprint | Where-Object { $_ })[0]
$app        = @($csproj.Project.PropertyGroup.Version | Where-Object { $_ })[0]
$core       = (Select-String -Path "$root\core\Cargo.toml" -Pattern '^version = "(.+)"' | Select-Object -First 1).Matches.Groups[1].Value
if (-not $full.EndsWith('.0')) { throw "a release's fourth number is 0 (manifest says $full)" }
if ($app -ne $full) { throw "app\CoffeeFilter.csproj says $app, the manifest says $full" }
if ($core -ne $version) { throw "core\Cargo.toml says $core, the release is $version" }

# What's released must be what's on GitHub, under a tag that's still free
Push-Location $root
try {
    if (git status --porcelain) { throw 'uncommitted changes; commit and push first' }
    git fetch -q origin
    if ((git rev-parse HEAD) -ne (git rev-parse origin/main)) { throw 'main is not pushed; push first' }
    if (git ls-remote --tags origin "refs/tags/v$version") { throw "tag v$version already exists" }
}
finally { Pop-Location }

# Release Gate: nothing ships unless everything passes
& "$PSScriptRoot\test.ps1"
if ($LASTEXITCODE -ne 0) { throw 'tests failed; no release' }

$msix = "$root\app\AppPackages\CoffeeFilter_${full}_x64_Test\CoffeeFilter_${full}_x64.msix"
if ((Get-AuthenticodeSignature $msix).Status -ne 'Valid') { throw "$msix isn't validly signed" }

# Release files, named without the fourth number
$out = Join-Path ([IO.Path]::GetTempPath()) "coffee-filter-release-$version"
New-Item -ItemType Directory -Force $out | Out-Null
Copy-Item $msix "$out\CoffeeFilter_${version}_x64.msix" -Force
Export-Certificate -Cert "Cert:\CurrentUser\My\$thumbprint" -FilePath "$out\CoffeeFilter-signing.cer" -Type CERT | Out-Null

gh release create "v$version" --repo artistro08/coffee-filter --target main --title "Coffee Filter $version" --notes-file $Notes "$out\CoffeeFilter_${version}_x64.msix" "$out\CoffeeFilter-signing.cer"
if ($LASTEXITCODE -ne 0) { throw 'gh release create failed' }
