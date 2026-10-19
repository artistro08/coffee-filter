# Points the core's build at the tools BoringSSL needs (see core\src\upstream.rs).
#
# Output: sets CMAKE, ASM_NASM, and LIBCLANG_PATH for this PowerShell session.
# Used by: build.ps1 and test.ps1 (dot-sourced), or by hand before running cargo yourself:
#          . scripts\tools.ps1
# Needs: CMake from Visual Studio's "C++ CMake tools for Windows" (found with vswhere.exe),
#        NASM (https://www.nasm.us/, or: winget install NASM.NASM), and
#        LLVM for bindgen's libclang (https://llvm.org/, or: winget install LLVM.LLVM).

# Visual Studio's own CMake, newest install first (previews too)
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
if (-not $env:CMAKE -and (Test-Path $vswhere)) {
    $cmake = & $vswhere -all -prerelease -sort -products '*' -property installationPath |
        ForEach-Object { Join-Path $_ 'Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin\cmake.exe' } |
        Where-Object { Test-Path $_ } |
        Select-Object -First 1
    if ($cmake) { $env:CMAKE = $cmake }
}
if (-not $env:CMAKE) { $env:CMAKE = (Get-Command cmake.exe -ErrorAction SilentlyContinue).Source }

# NASM, from a user or machine install
if (-not $env:ASM_NASM) {
    $nasm = @("$env:LOCALAPPDATA\bin\NASM\nasm.exe", "$env:ProgramFiles\NASM\nasm.exe") | Where-Object { Test-Path $_ } | Select-Object -First 1
    if (-not $nasm) { $nasm = (Get-Command nasm.exe -ErrorAction SilentlyContinue).Source }
    if ($nasm) { $env:ASM_NASM = $nasm }
}

# libclang for the BoringSSL bindings
if (-not $env:LIBCLANG_PATH) {
    $clang = (Get-Command clang.exe -ErrorAction SilentlyContinue).Source
    $env:LIBCLANG_PATH = @("$env:ProgramFiles\LLVM\bin", $(if ($clang) { Split-Path $clang })) |
        Where-Object { $_ -and (Test-Path "$_\libclang.dll") } |
        Select-Object -First 1
}

foreach ($need in 'CMAKE', 'ASM_NASM', 'LIBCLANG_PATH') {
    if (-not (Get-Item "env:$need" -ErrorAction SilentlyContinue)) { throw "$need not found; see scripts\tools.ps1 for what to install" }
}
