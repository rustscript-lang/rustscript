param(
    [string]$ExtensionDir = "rss-language-extension",
    [string]$OutputDir = "..\.vsix"
)

$ErrorActionPreference = "Stop"

$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$extensionPath = Resolve-Path (Join-Path $scriptDir $ExtensionDir)

if (-not (Test-Path $extensionPath)) {
    throw "Extension directory not found: $extensionPath"
}

$packageJsonPath = Join-Path $extensionPath "package.json"
if (-not (Test-Path $packageJsonPath)) {
    throw "package.json not found: $packageJsonPath"
}

$pkg = Get-Content $packageJsonPath | ConvertFrom-Json
$name = $pkg.name
$version = $pkg.version

if ([string]::IsNullOrWhiteSpace($name) -or [string]::IsNullOrWhiteSpace($version)) {
    throw "package.json must contain non-empty 'name' and 'version'."
}

$outputPath = Resolve-Path -Path (Join-Path $scriptDir ".") | Select-Object -ExpandProperty Path
$outputPath = Join-Path $outputPath $OutputDir
if (-not (Test-Path $outputPath)) {
    New-Item -ItemType Directory -Path $outputPath | Out-Null
}

$vsixPath = Join-Path $outputPath "$name-$version.vsix"

Push-Location $extensionPath
try {
    if (Test-Path (Join-Path $extensionPath "package-lock.json")) {
        npm ci
    }
    else {
        npm install
    }

    npm run copy-wasm
    npx @vscode/vsce package --out $vsixPath
}
finally {
    Pop-Location
}

Write-Output "Created VSIX: $vsixPath"
