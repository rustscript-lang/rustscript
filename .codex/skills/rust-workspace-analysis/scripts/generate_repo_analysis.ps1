[CmdletBinding()]
param(
    [string]$RepoRoot = ".",
    [string]$OutputPath = "repo-analysis.md"
)

$ErrorActionPreference = "Stop"

function Normalize-Path([string]$PathText) {
    return ((Resolve-Path $PathText).Path).Replace("/", "\").TrimEnd("\")
}

function Get-RelPath([string]$BasePath, [string]$TargetPath) {
    $base = Normalize-Path $BasePath
    $target = Normalize-Path $TargetPath
    if ($target.Equals($base, [System.StringComparison]::OrdinalIgnoreCase)) {
        return "."
    }
    $prefix = $base + "\"
    if ($target.StartsWith($prefix, [System.StringComparison]::OrdinalIgnoreCase)) {
        return $target.Substring($prefix.Length)
    }
    return $target
}

function Get-LineCount([string]$PathText) {
    if (-not (Test-Path $PathText)) {
        return 0
    }
    return (Get-Content $PathText | Measure-Object -Line).Lines
}

function Get-Bucket([string]$CrateDir, [string]$FilePath) {
    $rel = Get-RelPath $CrateDir $FilePath
    if ($rel -eq "build.rs") {
        return "build"
    }
    if (-not $rel.StartsWith("src\")) {
        return "other"
    }

    $parts = $rel.Substring(4).Split("\")
    if ($parts[0] -in @("lib.rs", "main.rs")) {
        return "root"
    }
    if ($parts[0] -eq "bin") {
        return "bin/" + [IO.Path]::GetFileNameWithoutExtension($parts[1])
    }
    if ($parts.Length -eq 1) {
        return [IO.Path]::GetFileNameWithoutExtension($parts[0])
    }

    $second = [IO.Path]::GetFileNameWithoutExtension($parts[1])
    if ($second -eq "mod") {
        return $parts[0]
    }
    return $parts[0] + "/" + $second
}

function Count-Tests([string]$PathText) {
    $state = $false
    $count = 0
    foreach ($line in Get-Content $PathText) {
        if ($line -match "#\[(tokio::)?test\b") {
            $state = $true
            continue
        }
        if ($state) {
            if ($line -match "^\s*#\[" -or $line.Trim() -eq "") {
                continue
            }
            if ($line -match "^\s*(pub\s+)?(async\s+)?fn\s+([A-Za-z0-9_]+)") {
                $count += 1
            }
            $state = $false
        }
    }
    return $count
}

$resolvedRoot = Normalize-Path $RepoRoot
if (-not (Test-Path (Join-Path $resolvedRoot "Cargo.toml"))) {
    throw "No Cargo.toml found at repo root: $resolvedRoot"
}

Push-Location $resolvedRoot
try {
    $metadata = cargo metadata --no-deps --format-version 1 | ConvertFrom-Json
}
finally {
    Pop-Location
}

$members = @{}
foreach ($member in $metadata.workspace_members) {
    $members[$member] = $true
}

$packages = @(
    $metadata.packages |
    Where-Object { $members.ContainsKey($_.id) } |
    Sort-Object manifest_path
)

$crateRows = foreach ($pkg in $packages) {
    $crateDir = Split-Path $pkg.manifest_path -Parent

    $codeFiles = @()
    if (Test-Path (Join-Path $crateDir "src")) {
        $codeFiles += Get-ChildItem (Join-Path $crateDir "src") -Recurse -File -Filter *.rs
    }
    if (Test-Path (Join-Path $crateDir "build.rs")) {
        $codeFiles += Get-Item (Join-Path $crateDir "build.rs")
    }

    $featureAreas = @(
        $codeFiles |
        Group-Object { Get-Bucket $crateDir $_.FullName } |
        ForEach-Object {
            [pscustomobject]@{
                feature = $_.Name
                lines = [int](($_.Group | ForEach-Object { Get-LineCount $_.FullName } | Measure-Object -Sum).Sum)
            }
        } |
        Sort-Object @{Expression = "lines"; Descending = $true}, feature
    )

    $featureCum = 0
    $featureAreas = @(
        $featureAreas | ForEach-Object {
            $featureCum += $_.lines
            [pscustomobject]@{
                feature = $_.feature
                lines = $_.lines
                cumulative = $featureCum
            }
        }
    )

    $testFiles = @()
    if (Test-Path (Join-Path $crateDir "tests")) {
        $testFiles += Get-ChildItem (Join-Path $crateDir "tests") -Recurse -File -Filter *.rs
    }
    if (Test-Path (Join-Path $crateDir "src")) {
        $testFiles += Get-ChildItem (Join-Path $crateDir "src") -Recurse -File -Filter *.rs
    }
    if (Test-Path (Join-Path $crateDir "build.rs")) {
        $testFiles += Get-Item (Join-Path $crateDir "build.rs")
    }

    $testSuites = @(
        $testFiles |
        Sort-Object FullName -Unique |
        ForEach-Object {
            $count = Count-Tests $_.FullName
            if ($count -gt 0) {
                $rel = (Get-RelPath $crateDir $_.FullName).Replace("\", "/")
                $kind = "unit"
                if ($rel.StartsWith("tests/")) {
                    $kind = "integration"
                }
                [pscustomobject]@{
                    path = $rel
                    kind = $kind
                    tests = $count
                }
            }
        } |
        Sort-Object @{Expression = "tests"; Descending = $true}, path
    )

    [pscustomobject]@{
        crate = $pkg.name
        manifest = (Get-RelPath $resolvedRoot $pkg.manifest_path).Replace("\", "/")
        cargo_features = @($pkg.features.PSObject.Properties.Name | Sort-Object)
        code_lines = [int](($featureAreas | Measure-Object -Property lines -Sum).Sum)
        feature_areas = $featureAreas
        test_total = [int](($testSuites | Measure-Object -Property tests -Sum).Sum)
        test_suites = $testSuites
    }
}

$workspaceCum = 0
$crateRows = @(
    $crateRows | ForEach-Object {
        $workspaceCum += $_.code_lines
        [pscustomobject]@{
            crate = $_.crate
            manifest = $_.manifest
            cargo_features = $_.cargo_features
            code_lines = $_.code_lines
            cumulative_code_lines = $workspaceCum
            feature_areas = $_.feature_areas
            test_total = $_.test_total
            test_suites = $_.test_suites
        }
    }
)

$totalCode = ($crateRows | Measure-Object -Property code_lines -Sum).Sum
$totalTests = ($crateRows | Measure-Object -Property test_total -Sum).Sum

$lines = New-Object System.Collections.Generic.List[string]
$lines.Add("# Repo Analysis")
$lines.Add("")
$lines.Add("- Count basis: physical Rust lines in `src/**/*.rs` plus `build.rs` for each crate.")
$lines.Add("- Excluded from LOC totals: `tests/`, `examples/`, `docs/`, `target/`, and web assets.")
$lines.Add("- Feature buckets are source/module areas, not Cargo feature flags. Cargo feature flags are listed separately because they overlap.")
$lines.Add("- Test counts come from detected `#[test]` and `#[tokio::test]` functions in `src/**/*.rs`, `tests/**/*.rs`, and `build.rs` when present.")
$lines.Add("")
$lines.Add(("Workspace production LOC: **{0}**" -f $totalCode))
$lines.Add(("Detected tests: **{0}**" -f $totalTests))
$lines.Add("")
$lines.Add("## Crate Summary")
$lines.Add("")
$lines.Add("| Crate | LOC | Cumulative LOC | Tests | Cargo features |")
$lines.Add("| --- | ---: | ---: | ---: | --- |")
foreach ($crate in $crateRows) {
    $featureText = "-"
    if ($crate.cargo_features.Count -gt 0) {
        $featureText = ($crate.cargo_features -join ", ")
    }
    $lines.Add(("| {0} | {1} | {2} | {3} | {4} |" -f $crate.crate, $crate.code_lines, $crate.cumulative_code_lines, $crate.test_total, $featureText))
}

$lines.Add("")
$lines.Add("## Crate Feature Matrix")
$lines.Add("")
$lines.Add("| Crate | Crate LOC | Crate Cum LOC | Feature / Functionality | Feature LOC | Feature Cum LOC | Tests | Cargo features |")
$lines.Add("| --- | ---: | ---: | --- | ---: | ---: | ---: | --- |")
foreach ($crate in $crateRows) {
    $featureText = "-"
    if ($crate.cargo_features.Count -gt 0) {
        $featureText = ($crate.cargo_features -join ", ")
    }
    foreach ($area in $crate.feature_areas) {
        $lines.Add(("| {0} | {1} | {2} | {3} | {4} | {5} | {6} | {7} |" -f $crate.crate, $crate.code_lines, $crate.cumulative_code_lines, $area.feature, $area.lines, $area.cumulative, $crate.test_total, $featureText))
    }
}

$lines.Add("")
$lines.Add("## Crate Test Matrix")
$lines.Add("")
$lines.Add("| Crate | Suite | Kind | Tests |")
$lines.Add("| --- | --- | --- | ---: |")
foreach ($crate in $crateRows) {
    if ($crate.test_total -eq 0) {
        $lines.Add(("| {0} | _none_ | - | 0 |" -f $crate.crate))
        continue
    }
    foreach ($suite in $crate.test_suites) {
        $lines.Add(("| {0} | {1} | {2} | {3} |" -f $crate.crate, $suite.path, $suite.kind, $suite.tests))
    }
}

foreach ($crate in $crateRows) {
    $lines.Add("")
    $lines.Add(("## {0}" -f $crate.crate))
    $lines.Add("")
    $line = '- Manifest: `{0}`' -f $crate.manifest
    $lines.Add($line)
    $line = '- LOC: **{0}**' -f $crate.code_lines
    $lines.Add($line)
    $line = '- Cumulative workspace LOC at this crate: **{0}**' -f $crate.cumulative_code_lines
    $lines.Add($line)
    $featureText = "-"
    if ($crate.cargo_features.Count -gt 0) {
        $featureText = ($crate.cargo_features -join ", ")
    }
    $line = '- Cargo features: {0}' -f $featureText
    $lines.Add($line)
    $line = '- Tests: **{0}**' -f $crate.test_total
    $lines.Add($line)
    $lines.Add("")
    $lines.Add("### Feature Areas")
    $lines.Add("")
    $lines.Add("| Feature area | LOC | Cumulative in crate |")
    $lines.Add("| --- | ---: | ---: |")
    foreach ($area in $crate.feature_areas) {
        $lines.Add(("| {0} | {1} | {2} |" -f $area.feature, $area.lines, $area.cumulative))
    }
    $lines.Add("")
    $lines.Add("### Test Suites")
    $lines.Add("")
    if ($crate.test_total -eq 0) {
        $lines.Add("_No detected tests._")
    } else {
        $lines.Add("| Suite | Kind | Tests |")
        $lines.Add("| --- | --- | ---: |")
        foreach ($suite in $crate.test_suites) {
            $lines.Add(("| {0} | {1} | {2} |" -f $suite.path, $suite.kind, $suite.tests))
        }
    }
}

$outPath = $OutputPath
if (-not [System.IO.Path]::IsPathRooted($outPath)) {
    $outPath = Join-Path $resolvedRoot $OutputPath
}

$outDir = Split-Path $outPath -Parent
if ($outDir -and -not (Test-Path $outDir)) {
    New-Item -ItemType Directory -Path $outDir | Out-Null
}

$lines -join "`r`n" | Set-Content -Path $outPath -Encoding UTF8
Write-Output $outPath
