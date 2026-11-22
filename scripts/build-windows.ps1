# Build the Windows x64 app or NSIS installer using installed dependencies.
[CmdletBinding()]
param(
    [ValidateSet('app', 'installer')]
    [string]$Mode = 'app',
    [switch]$Check,
    [switch]$Help
)

$ErrorActionPreference = 'Stop'
if ($Help) {
    Write-Output 'Usage: powershell -File scripts/build-windows.ps1 [app|installer] [-Check]'
    Write-Output 'Defaults to app. -Check runs the full repository checks before building.'
    exit 0
}
if ($env:OS -ne 'Windows_NT') { throw 'This script requires Windows.' }

function Invoke-Checked {
    param([string]$Command, [string[]]$Arguments)
    & $Command @Arguments
    if ($LASTEXITCODE -ne 0) { throw "$Command failed with exit code $LASTEXITCODE." }
}

$previousToolchain = $env:RUSTUP_TOOLCHAIN
Push-Location (Join-Path $PSScriptRoot '..')
try {
    if (!(Test-Path node_modules/.bin/tauri.cmd) -or !(Test-Path node_modules/.bin/vite.cmd)) {
        throw 'Missing node_modules dependencies. Install the project dependencies first.'
    }
    if ($env:CARGO_TARGET_DIR -or $env:CARGO_BUILD_TARGET) {
        throw 'Unset CARGO_TARGET_DIR and CARGO_BUILD_TARGET for this x64 build.'
    }
    $target = 'x86_64-pc-windows-msvc'
    $releaseDirectory = Join-Path $PWD "src-tauri/target/$target/release"
    $appPath = Join-Path $releaseDirectory 'openquota.exe'
    $runningApp = Get-Process openquota -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -eq $appPath }
    if ($runningApp) {
        throw "Close the running application before rebuilding: $appPath"
    }
    $tauri = Join-Path $PWD 'node_modules/.bin/tauri.cmd'
    # Select the x64 compiler itself, including on Windows ARM64 hosts.
    $env:RUSTUP_TOOLCHAIN = 'stable-x86_64-pc-windows-msvc'
    Invoke-Checked rustc @('-vV')
    if ($Check) {
        $packageRunner = foreach ($candidate in @('npm.cmd', 'pnpm.exe', 'pnpm.cmd')) {
            $resolved = Get-Command $candidate -CommandType Application -ErrorAction SilentlyContinue |
                Select-Object -First 1
            if ($resolved) {
                $resolved.Source
                break
            }
        }
        if (-not $packageRunner) {
            throw 'Missing npm.cmd or pnpm on PATH.'
        }
        foreach ($script in @('verify:versions', 'verify:contracts', 'lint', 'check', 'test', 'build', 'verify:rust')) {
            Invoke-Checked $packageRunner @('run', $script)
        }
        Invoke-Checked git @('diff', '--check')
    }
    if ($Mode -eq 'installer') {
        Invoke-Checked $tauri @('build', '--bundles', 'nsis', '--target', $target)
    } else {
        Invoke-Checked $tauri @('build', '--no-bundle', '--target', $target)
    }
    $stream = [System.IO.File]::OpenRead($appPath)
    $reader = [System.IO.BinaryReader]::new($stream)
    try {
        $stream.Position = 0x3c
        $peOffset = $reader.ReadInt32()
        $stream.Position = $peOffset
        if ($reader.ReadUInt32() -ne 0x00004550 -or $reader.ReadUInt16() -ne 0x8664) {
            throw 'Built application is not a Windows x64 executable.'
        }
    } finally {
        $reader.Dispose()
    }
    Write-Output "App: $appPath"
    if ($Mode -eq 'installer') {
        $version = (Get-Content package.json -Raw | ConvertFrom-Json).version
        $installerPath = Join-Path $releaseDirectory "bundle/nsis/OpenQuota_${version}_x64-setup.exe"
        Get-FileHash -LiteralPath $installerPath -Algorithm SHA256
        Write-Output "Installer: $installerPath"
    }
} finally {
    $env:RUSTUP_TOOLCHAIN = $previousToolchain
    Pop-Location
}
