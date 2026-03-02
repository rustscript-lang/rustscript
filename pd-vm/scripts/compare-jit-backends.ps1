#!/usr/bin/env pwsh
Set-StrictMode -Version Latest

function Show-Usage {
    @'
Usage:
  pd-vm/scripts/compare-jit-backends.ps1 [perf_test_name]

Defaults:
  perf_test_name = perf_jit_native_reduces_tight_loop_latency

Runs the same ignored perf test twice:
  1) PD_VM_JIT_CODEGEN=handwritten
  2) PD_VM_JIT_CODEGEN=cranelift

Requires:
  - pd-vm feature: cranelift-jit
'@
}

$testName = 'perf_jit_native_reduces_tight_loop_latency'

if ($args.Count -gt 1) {
    Show-Usage
    exit 1
}

if ($args.Count -eq 1) {
    switch ($args[0]) {
        '-h' {
            Show-Usage
            exit 0
        }
        '--help' {
            Show-Usage
            exit 0
        }
        default {
            $testName = $args[0]
        }
    }
}

$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$repoRoot = (Resolve-Path (Join-Path $scriptDir '..\..')).Path

function Invoke-Backend([string]$backend, [string]$perfTestName, [string]$repoPath) {
    $suffix = [Guid]::NewGuid().ToString('N').Substring(0, 8)
    $logFile = Join-Path ([System.IO.Path]::GetTempPath()) ("pd-vm-{0}-{1}.log" -f $backend, $suffix)
    $status = 'ok'

    $previousCodegen = [Environment]::GetEnvironmentVariable('PD_VM_JIT_CODEGEN', 'Process')

    Push-Location $repoPath
    try {
        [Environment]::SetEnvironmentVariable('PD_VM_JIT_CODEGEN', $backend, 'Process')
        & cargo test -p pd-vm --features cranelift-jit --test perf_tests -- --ignored --exact $perfTestName --nocapture *> $logFile
        if ($LASTEXITCODE -ne 0) {
            $status = 'fail'
        }
    } finally {
        [Environment]::SetEnvironmentVariable('PD_VM_JIT_CODEGEN', $previousCodegen, 'Process')
        Pop-Location
    }

    $line = ''
    if (Test-Path $logFile) {
        $line = Get-Content -Path $logFile | Select-String -Pattern 'latency median:' | Select-Object -Last 1 | ForEach-Object { $_.Line }
    }

    $interpreter = '-'
    $jit = '-'
    $unit = '-'
    $speedup = '-'

    if ($line -and $line -match 'interpreter=([0-9]+)([a-z]+)\s+jit=([0-9]+)([a-z]+)\s+speedup=([0-9]+(\.[0-9]+)?)x') {
        $interpreter = $Matches[1]
        $jit = $Matches[3]
        $speedup = $Matches[5]
        $unit = $Matches[2]
        if ($Matches[2] -ne $Matches[4]) {
            $unit = '{0}/{1}' -f $Matches[2], $Matches[4]
        }
    }

    if ($status -ne 'ok') {
        [Console]::Error.WriteLine("backend '$backend' failed. Last 40 log lines:")
        if (Test-Path $logFile) {
            Get-Content -Path $logFile -Tail 40 | ForEach-Object { [Console]::Error.WriteLine($_) }
        }
    }

    [Console]::Error.WriteLine("log[$backend]: $logFile")

    [PSCustomObject]@{
        Backend     = $backend
        Status      = $status
        Interpreter = $interpreter
        Jit         = $jit
        Unit        = $unit
        Speedup     = $speedup
    }
}

$handwritten = Invoke-Backend -backend 'handwritten' -perfTestName $testName -repoPath $repoRoot
$cranelift = Invoke-Backend -backend 'cranelift' -perfTestName $testName -repoPath $repoRoot

('{0,-12} {1,-8} {2,-12} {3,-12} {4,-8} {5,-10}' -f 'backend', 'status', 'interpreter', 'jit', 'unit', 'speedup')
('{0,-12} {1,-8} {2,-12} {3,-12} {4,-8} {5,-10}' -f '------------', '--------', '------------', '------------', '--------', '----------')
('{0,-12} {1,-8} {2,-12} {3,-12} {4,-8} {5,-10}' -f $handwritten.Backend, $handwritten.Status, $handwritten.Interpreter, $handwritten.Jit, $handwritten.Unit, $handwritten.Speedup)
('{0,-12} {1,-8} {2,-12} {3,-12} {4,-8} {5,-10}' -f $cranelift.Backend, $cranelift.Status, $cranelift.Interpreter, $cranelift.Jit, $cranelift.Unit, $cranelift.Speedup)

if (
    $handwritten.Status -eq 'ok' -and
    $cranelift.Status -eq 'ok' -and
    $handwritten.Jit -match '^[0-9]+$' -and
    $cranelift.Jit -match '^[0-9]+$' -and
    $handwritten.Unit -ne '-' -and
    $handwritten.Unit -eq $cranelift.Unit
) {
    $ratio = 'n/a'
    $a = [double]$handwritten.Jit
    $b = [double]$cranelift.Jit
    if ($a -ne 0.0) {
        $ratio = ($b / $a).ToString('0.000', [Globalization.CultureInfo]::InvariantCulture)
    }

    ''
    "cranelift/handwritten jit ratio: $ratio ($($handwritten.Unit))"
}
