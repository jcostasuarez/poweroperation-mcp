<#
.SYNOPSIS
  Compila poweroperation-mcp para Windows x64 y x86, arma el zip de entrega y (opcional) publica el release en GitHub.

.DESCRIPTION
  1. Lee la versión de Cargo.toml.
  2. cargo test (salvo -SkipTests): unitarias + tests\offline.rs y tests\runtime.rs (CtApi.dll falso).
  3. cargo build --release (x64, host) y --target i686-pc-windows-msvc (x86). Si no hay vswhere, carga el
     entorno de Visual Studio desde vcvarsall.bat.
  4. Arma dist\poweroperation-mcp\ (exe x64, x86\exe, README.md, .env.example) y
     dist\poweroperation-mcp-<versión>.zip + .sha256.
  5. Con -Publish: exige árbol limpio y sincronizado con origin, crea el tag v<versión> y el release con gh.

.EXAMPLE
  .\scripts\release.ps1                 # solo compila y empaqueta
  .\scripts\release.ps1 -SkipTests      # sin cargo test
  .\scripts\release.ps1 -Publish        # empaqueta y publica el release v<versión>
  .\scripts\release.ps1 -Publish -Draft # lo deja como borrador
#>
[CmdletBinding()]
param(
    [switch]$SkipTests,
    [switch]$Publish,
    [switch]$Draft
)

$ErrorActionPreference = 'Stop'
$Root = Split-Path -Parent $PSScriptRoot
Set-Location $Root

$Name = 'poweroperation-mcp'
$X86 = 'i686-pc-windows-msvc'

function Step($msg) { Write-Host "==> $msg" -ForegroundColor Cyan }
function Invoke-Native {
    param([string]$Exe, [string[]]$Arguments)
    # cargo/rustup/gh escriben progreso en stderr: en PowerShell 5.1 con 'Stop' eso cortaría el script.
    $prev = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try { & $Exe @Arguments } finally { $ErrorActionPreference = $prev }
    if ($LASTEXITCODE -ne 0) { throw "Falló: $Exe $($Arguments -join ' ') (código $LASTEXITCODE)" }
}

foreach ($tool in 'cargo', 'rustup') {
    if (-not (Get-Command $tool -ErrorAction SilentlyContinue)) {
        throw "No se encontró '$tool' en el PATH. Instalar Rust desde https://rustup.rs (toolchain MSVC)."
    }
}

# --- Entorno de MSVC ---
# Rust encuentra solo el linker de Visual Studio con vswhere; si falta (o Visual Studio no está registrado), el
# entorno se carga desde vcvarsall.bat, uno por arquitectura (x64 y x64_x86), restaurando el original entre medio.
$OriginalEnv = @{}
Get-ChildItem env: | ForEach-Object { $OriginalEnv[$_.Name] = $_.Value }
$VcVars = $null
$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
$vsPath = if (Test-Path $vswhere) { & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath } else { $null }
$link = Get-Command link.exe -ErrorAction SilentlyContinue
# Con vswhere viendo el C++ de VS, rustc se arregla solo; si no (VS sin registrar, como en algunas VM), vcvarsall.
if (-not $vsPath -and -not ($link -and $link.Source -match '\\VC\\Tools\\MSVC\\')) {
    $VcVars = Get-ChildItem "$env:ProgramFiles\Microsoft Visual Studio\*\*\VC\Auxiliary\Build\vcvarsall.bat", "${env:ProgramFiles(x86)}\Microsoft Visual Studio\*\*\VC\Auxiliary\Build\vcvarsall.bat" -ErrorAction SilentlyContinue |
        Sort-Object FullName -Descending | Select-Object -First 1 -ExpandProperty FullName
    if (-not $VcVars) { throw 'No se encontró Visual Studio / Build Tools con C++ (vcvarsall.bat). Instalar "Desarrollo para el escritorio con C++".' }
    Write-Host "Usando $VcVars" -ForegroundColor DarkGray
}

function Use-Msvc([string]$Arch) {
    if (-not $VcVars) { return }
    Get-ChildItem env: | Where-Object { -not $OriginalEnv.ContainsKey($_.Name) } | ForEach-Object { Remove-Item "env:$($_.Name)" }
    foreach ($k in $OriginalEnv.Keys) { Set-Item "env:$k" $OriginalEnv[$k] }
    $lines = cmd /c "`"$VcVars`" $Arch >nul 2>&1 && set"
    if ($LASTEXITCODE -ne 0) { throw "Falló vcvarsall.bat $Arch" }
    foreach ($l in $lines) { if ($l -match '^([^=]+)=(.*)$') { Set-Item "env:$($matches[1])" $matches[2] } }
}

# --- Versión ---
$m = Select-String -Path (Join-Path $Root 'Cargo.toml') -Pattern '^\s*version\s*=\s*"([^"]+)"' | Select-Object -First 1
if (-not $m) { throw 'No se encontró la versión en Cargo.toml' }
$Version = $m.Matches[0].Groups[1].Value
$Tag = "v$Version"
Step "$Name $Version"

# --- Chequeos previos para publicar (antes de compilar, para fallar rápido) ---
if ($Publish) {
    if (-not (Get-Command gh -ErrorAction SilentlyContinue)) { throw "No se encontró 'gh' (GitHub CLI)." }
    $dirty = git status --porcelain
    if ($dirty) { throw "Hay cambios sin commitear; commitear antes de publicar:`n$dirty" }
    Invoke-Native git @('fetch', '--quiet', 'origin')
    $branch = (git rev-parse --abbrev-ref HEAD).Trim()
    $local = (git rev-parse HEAD).Trim()
    $remote = (git rev-parse "origin/$branch" 2>$null)
    if ($LASTEXITCODE -ne 0 -or $local -ne $remote.Trim()) { throw "La rama '$branch' no está sincronizada con origin (hacer push primero)." }
    gh release view $Tag *> $null
    if ($LASTEXITCODE -eq 0) { throw "El release $Tag ya existe. Subir la versión en Cargo.toml." }
    $global:LASTEXITCODE = 0
}

# --- Target x86 ---
if (-not ((rustup target list --installed) -contains $X86)) {
    Step "Instalando target $X86"
    Invoke-Native rustup @('target', 'add', $X86)
}

Use-Msvc 'x64'

# --- Pruebas (unitarias + tests\offline.rs y tests\runtime.rs con el CtApi.dll falso) ---
if (-not $SkipTests) {
    Step 'cargo test'
    Invoke-Native cargo @('test', '--quiet')
}

# --- Compilación ---
Step 'Compilando x64'
Invoke-Native cargo @('build', '--release')
Step 'Compilando x86'
Use-Msvc 'x64_x86'
Invoke-Native cargo @('build', '--release', '--target', $X86)

$Exe64 = Join-Path $Root "target\release\$Name.exe"
$Exe32 = Join-Path $Root "target\$X86\release\$Name.exe"
foreach ($e in $Exe64, $Exe32) { if (-not (Test-Path $e)) { throw "No se generó $e" } }

# --- Paquete ---
Step 'Armando dist\'
$Dist = Join-Path $Root 'dist'
$Stage = Join-Path $Dist $Name
if (Test-Path $Stage) { Remove-Item $Stage -Recurse -Force }
New-Item -ItemType Directory -Force (Join-Path $Stage 'x86') | Out-Null
Copy-Item $Exe64 $Stage
Copy-Item $Exe32 (Join-Path $Stage 'x86')
Copy-Item (Join-Path $Root 'README.md'), (Join-Path $Root '.env.example') $Stage

$Zip = Join-Path $Dist "$Name-$Version.zip"
if (Test-Path $Zip) { Remove-Item $Zip -Force }
Add-Type -AssemblyName System.IO.Compression.FileSystem
[IO.Compression.ZipFile]::CreateFromDirectory($Stage, $Zip, [IO.Compression.CompressionLevel]::Optimal, $true)

$Hash = (Get-FileHash $Zip -Algorithm SHA256).Hash.ToLower()
$ShaFile = "$Zip.sha256"
[IO.File]::WriteAllText($ShaFile, "$Hash  $(Split-Path $Zip -Leaf)`n")

Write-Host ''
Write-Host "Paquete: $Zip" -ForegroundColor Green
Write-Host "SHA256:  $Hash"
Get-ChildItem $Stage -Recurse -File | ForEach-Object {
    '  {0,-40} {1,10:N0} bytes' -f $_.FullName.Substring($Stage.Length + 1), $_.Length
}

# --- Release ---
if ($Publish) {
    Step "Publicando release $Tag"
    $notes = @"
Servidor MCP para EcoStruxure Power Operation, versión $Version.

- ``$Name.exe``: Windows x64 (recomendado).
- ``x86\$Name.exe``: Windows x86, para cuando ``CtApi.dll`` es de 32 bits.

Descomprimir en la VM, copiar ``.env.example`` como ``.env`` si hace falta y registrar el exe en el agente (ver README).

SHA256 (zip): ``$Hash``
"@
    $ghArgs = @('release', 'create', $Tag, $Zip, $ShaFile, '--title', "$Name $Version", '--notes', $notes, '--target', (git rev-parse HEAD).Trim())
    if ($Draft) { $ghArgs += '--draft' }
    Invoke-Native gh $ghArgs
    Write-Host "Release $Tag publicado." -ForegroundColor Green
}
