#
# COKACCTL Installer for Windows
# Usage: irm https://raw.githubusercontent.com/kstost/cokacctl/refs/heads/main/manage.ps1 | iex
#

$ErrorActionPreference = "Stop"

$BINARY_NAME = "cokacctl"
$BASE_URL = "https://raw.githubusercontent.com/kstost/cokacctl/refs/heads/main/dist_beta"

function Info($msg) { Write-Host "→ $msg" -ForegroundColor Blue }
function Success($msg) { Write-Host "✓ $msg" -ForegroundColor Green }
function Warn($msg) { Write-Host "! $msg" -ForegroundColor Yellow }
function Error($msg) { Write-Host "✗ $msg" -ForegroundColor Red; throw $msg }

function Test-PortableExecutable($path) {
    $reader = $null
    try {
        $reader = [IO.BinaryReader]::new([IO.File]::OpenRead($path))
        if ($reader.BaseStream.Length -lt 64 -or $reader.ReadUInt16() -ne 0x5A4D) {
            return $false
        }
        $reader.BaseStream.Seek(0x3C, [IO.SeekOrigin]::Begin) | Out-Null
        $peOffset = $reader.ReadInt32()
        if ($peOffset -lt 64 -or $peOffset -gt ($reader.BaseStream.Length - 4)) {
            return $false
        }
        $reader.BaseStream.Seek($peOffset, [IO.SeekOrigin]::Begin) | Out-Null
        return ($reader.ReadUInt32() -eq 0x00004550)
    } catch {
        return $false
    } finally {
        if ($null -ne $reader) { $reader.Dispose() }
    }
}

# Detect architecture
function Detect-Arch {
    # A 32-bit PowerShell process on 64-bit Windows reports its process
    # architecture in PROCESSOR_ARCHITECTURE. PROCESSOR_ARCHITEW6432 carries
    # the native OS architecture in that case.
    $arch = if ($env:PROCESSOR_ARCHITEW6432) {
        $env:PROCESSOR_ARCHITEW6432
    } else {
        $env:PROCESSOR_ARCHITECTURE
    }
    switch ($arch) {
        "AMD64" { return "x86_64" }
        "ARM64" { return "aarch64" }
        default { Error "Unsupported architecture: $arch" }
    }
}

# Get install directory
function Get-InstallDir {
    $dir = Join-Path $env:LOCALAPPDATA "cokacctl"
    if (-not (Test-Path -LiteralPath $dir)) {
        New-Item -ItemType Directory -Path $dir -Force | Out-Null
    }
    return $dir
}

# Add directory to user PATH
function Add-ToPath($dir) {
    $currentPath = [Environment]::GetEnvironmentVariable("Path", "User")
    $normalizedDir = [IO.Path]::GetFullPath($dir).TrimEnd('\', '/')
    $alreadyPresent = @($currentPath -split ';') | Where-Object {
        if ([String]::IsNullOrWhiteSpace($_)) { return $false }
        try {
            $entry = [IO.Path]::GetFullPath($_.Trim()).TrimEnd('\', '/')
            return [StringComparer]::OrdinalIgnoreCase.Equals($entry, $normalizedDir)
        } catch {
            return $false
        }
    }
    if (-not $alreadyPresent) {
        $newPath = if ([String]::IsNullOrWhiteSpace($currentPath)) { $dir } else { "$dir;$currentPath" }
        [Environment]::SetEnvironmentVariable("Path", $newPath, "User")
        $env:Path = "$dir;$env:Path"
        Success "Added $dir to PATH"
    }
}

function Main {
    $arch = Detect-Arch

    Info "Downloading cokacctl (windows-$arch)..."

    $filename = "${BINARY_NAME}-windows-${arch}.exe"
    $url = "${BASE_URL}/${filename}"

    $installDir = Get-InstallDir
    $installPath = Join-Path $installDir "${BINARY_NAME}.exe"
    $tempPath = Join-Path $installDir ".${BINARY_NAME}.$([Guid]::NewGuid().ToString('N')).tmp.exe"
    $backupPath = Join-Path $installDir ".${BINARY_NAME}.$([Guid]::NewGuid().ToString('N')).backup.exe"
    $published = $false
    $verified = $false

    if (Test-Path -LiteralPath $installPath) {
        $existingInstall = Get-Item -LiteralPath $installPath -Force
        if ($existingInstall.PSIsContainer -or
            (($existingInstall.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0)) {
            throw "Refusing to replace a directory or reparse-point install path: $installPath"
        }
    }

    try {
        # Download beside the installed binary so publication stays on one
        # filesystem. A failed or interrupted download never touches the old exe.
        Invoke-WebRequest -Uri $url -OutFile $tempPath -UseBasicParsing
        if (-not (Test-Path -LiteralPath $tempPath -PathType Leaf) -or
            (Get-Item -LiteralPath $tempPath).Length -le 0) {
            throw "Downloaded file is empty"
        }
        if (-not (Test-PortableExecutable $tempPath)) {
            throw "Downloaded file is not a valid PE executable"
        }

        # Stop only the installed instance. Do not terminate unrelated
        # cokacctl processes that happen to have the same executable name.
        $installFullPath = [IO.Path]::GetFullPath($installPath)
        $stopped = $false
        Get-Process -Name $BINARY_NAME -ErrorAction SilentlyContinue | ForEach-Object {
            try {
                if ($_.Path -and [StringComparer]::OrdinalIgnoreCase.Equals(
                    [IO.Path]::GetFullPath($_.Path), $installFullPath)) {
                    # Keep the Process object selected by the path check rather
                    # than looking it up again by PID, which could target a
                    # newly reused PID if the original process exits here.
                    Stop-Process -InputObject $_ -Force -ErrorAction Stop
                    $stopped = $true
                }
            } catch {
                # A process can exit between enumeration and inspection.
            }
        }
        if ($stopped) { Start-Sleep -Seconds 1 }

        if (Test-Path -LiteralPath $installPath -PathType Leaf) {
            try {
                [IO.File]::Replace($tempPath, $installPath, $backupPath, $true)
                $published = $true
            } catch {
                if (-not (Test-Path -LiteralPath $installPath) -and
                    (Test-Path -LiteralPath $backupPath)) {
                    Move-Item -LiteralPath $backupPath -Destination $installPath -Force
                }
                throw
            }
        } else {
            [IO.File]::Move($tempPath, $installPath)
            $published = $true
        }

        # Verify
        if (-not (Test-Path -LiteralPath $installPath -PathType Leaf) -or
            (Get-Item -LiteralPath $installPath).Length -le 0) {
            throw "Installation verification failed"
        }
        Add-ToPath $installDir
        # PATH publication is part of the transaction. If it fails, finally
        # restores the previous executable instead of reporting failure while
        # silently discarding the recovery backup.
        $verified = $true
        Success "Installed!"
        Success "Run 'cokacctl' to start."
    } finally {
        Remove-Item -LiteralPath $tempPath -Force -ErrorAction SilentlyContinue
        if ($verified) {
            Remove-Item -LiteralPath $backupPath -Force -ErrorAction SilentlyContinue
        } elseif (Test-Path -LiteralPath $backupPath -PathType Leaf) {
            try {
                if (Test-Path -LiteralPath $installPath -PathType Leaf) {
                    [IO.File]::Replace($backupPath, $installPath, $null, $true)
                } else {
                    [IO.File]::Move($backupPath, $installPath)
                }
            } catch {
                Write-Warning "Could not restore the previous executable. Backup preserved at: $backupPath"
            }
        } elseif ($published -and (Test-Path -LiteralPath $installPath)) {
            # There was no previous executable to restore.
            Remove-Item -LiteralPath $installPath -Force -ErrorAction SilentlyContinue
        }
    }
}

try {
    Main
} catch {
    Write-Error "Installation failed: $($_.Exception.Message)"
    exit 1
}
