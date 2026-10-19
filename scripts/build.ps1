# Builds the Rust core (release) and the signed MSIX package.
#
# Output: core\target\release\coffee-filter-core.exe and
#         app\AppPackages\CoffeeFilter_<version>_x64_Test\CoffeeFilter_<version>_x64.msix
# Used by: install.ps1, test.ps1 (through install), release.ps1, and by hand.
# Needs: Rust (https://rustup.rs/), .NET 10 SDK (https://dotnet.microsoft.com/download),
#        the Windows SDK (rc.exe), the Visual Studio Installer (vswhere.exe, for the AOT linker),
#        CMake, NASM, and LLVM for BoringSSL (see tools.ps1), and the signing certificate whose thumbprint is in app\CoffeeFilter.csproj.

$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent

# The AOT linker finds the C++ tools through vswhere.exe
$env:PATH += ";${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer"

# CMake, NASM, and libclang for BoringSSL
. "$PSScriptRoot\tools.ps1"

# Both version numbers go up together (Windows refuses changed content under the same one)
$version = ([xml](Get-Content "$root\app\Package.appxmanifest")).Package.Identity.Version
$app     = @(([xml](Get-Content "$root\app\CoffeeFilter.csproj")).Project.PropertyGroup.Version | Where-Object { $_ })[0]
if ($app -ne $version) { throw "app\CoffeeFilter.csproj says $app but the manifest says $version; bump both" }

# Core
Push-Location "$root\core"
try {
    cargo build --release
    if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }
}
finally { Pop-Location }

# App and MSIX (bundles the core built above)
Push-Location "$root\app"
try {
    dotnet publish -c Release
    if ($LASTEXITCODE -ne 0) { throw 'dotnet publish failed' }
}
finally { Pop-Location }

$msix    ="$root\app\AppPackages\CoffeeFilter_${version}_x64_Test\CoffeeFilter_${version}_x64.msix"
if (-not (Test-Path $msix)) { throw "no package at $msix" }
"built $msix"
