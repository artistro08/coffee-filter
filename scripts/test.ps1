# Runs every test suite, stopping at the first failure. A release is only allowed when this
# passes from start to finish (see CLAUDE.md, "Release Gate").
#
# Output: each suite's result; exits 1 on the first failure, 0 when everything passes.
# Used by: release.ps1, and before every commit that changes behavior.
# Needs: build.ps1 and install.ps1 run first for the current code, because the UI tests drive
#        the installed app. Chrome for the end-to-end test. The e2e test briefly changes the
#        system proxy and restores it; one UI test excludes a Store app and deletes it again.
#
# -SkipUi skips the UI tests (for quick checks only; never for a release).

param([switch]$SkipUi)

$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent

function Step($name, [scriptblock]$run) {
    "== $name"
    & $run
    if ($LASTEXITCODE -ne 0) {
        "FAILED: $name"
        exit 1
    }
}

Push-Location "$root\core"
try {
    Step 'clippy (no warnings allowed)' { cargo clippy --release --all-targets -- -D warnings }
    Step 'core unit tests' { cargo test --release }
    Step 'end-to-end (real proxy, real Chrome)' { cargo test --release -- --ignored }
}
finally { Pop-Location }

Push-Location "$root\tests\CoffeeFilter.Tests"
try { Step 'app-to-core tests' { dotnet test } }
finally { Pop-Location }

if (-not $SkipUi) {
    # The installed app must be the one just built, or the UI tests test old code
    $built     = ([xml](Get-Content "$root\app\Package.appxmanifest")).Package.Identity.Version
    $installed = (Get-AppxPackage Artistro08.CoffeeFilter).Version
    if ($installed -ne $built) {
        "FAILED: installed app is $installed but the code is $built; run scripts\build.ps1 and scripts\install.ps1"
        exit 1
    }

    Push-Location "$root\tests\CoffeeFilter.UITests"
    try { Step 'UI tests (installed app)' { dotnet test } }
    finally { Pop-Location }
}

'ALL PASSED'
