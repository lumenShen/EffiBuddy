# Remove EffiBuddy from the Windows taskbar on uninstall (best effort).
#
# Tries the shell "taskbarunpin" verb first, then deletes the pinned .lnk
# from "User Pinned\TaskBar". Always exits 0: an uninstall must never be
# blocked by a stubborn shortcut.
#
# Every step is individually guarded. In particular the deletion step must
# not depend on COM being available - if WScript.Shell cannot be created
# (locked-down machine, broken COM registration) the name-based cleanup
# still has to run, otherwise a stale pinned icon survives the uninstall.
#
# NOTE: keep this file ASCII-only (see pin-taskbar.ps1).

[CmdletBinding()]
param(
    [string]$TargetName = 'EffiBuddy'
)

$ErrorActionPreference = 'Continue'

function Write-Log([string]$Message) {
    Write-Output $Message
}

function Get-WscriptShell {
    # Returns $null when COM is unavailable instead of throwing.
    try {
        return New-Object -ComObject WScript.Shell
    } catch {
        Write-Log "com: WScript.Shell unavailable ($($_.Exception.Message))"
        return $null
    }
}

$installDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$exePath = Join-Path $installDir "$TargetName.exe"
$pinnedDir = Join-Path $env:APPDATA 'Microsoft\Internet Explorer\Quick Launch\User Pinned\TaskBar'
$removed = $false

# 1) shell verb, for the case where the item is still a live pinned entry.
try {
    if (Test-Path -LiteralPath $exePath) {
        $shell = New-Object -ComObject Shell.Application
        $item = $shell.Namespace($installDir).ParseName("$TargetName.exe")
        if ($null -ne $item) {
            foreach ($verbName in @('taskbarunpin', 'Unpin from taskbar')) {
                try {
                    $item.InvokeVerb($verbName)
                    Write-Log "verb: InvokeVerb('$verbName')"
                } catch {
                    Write-Log "verb: InvokeVerb('$verbName') threw: $($_.Exception.Message)"
                }
            }
            Start-Sleep -Milliseconds 600
        }
    }
} catch {
    Write-Log "verb: $($_.Exception.Message)"
}

# 2) delete the pinned .lnk, matched by name first, then by target path.
try {
    if (Test-Path -LiteralPath $pinnedDir) {
        $wsh = Get-WscriptShell
        foreach ($item in Get-ChildItem -LiteralPath $pinnedDir -Filter '*.lnk' -ErrorAction SilentlyContinue) {
            # Name match needs no COM, so it always works.
            $hit = ($item.BaseName -eq $TargetName)
            if (-not $hit -and $null -ne $wsh) {
                try {
                    $target = $wsh.CreateShortcut($item.FullName).TargetPath
                    $hit = ($target -and $exePath -and ($target -eq $exePath))
                } catch {
                    $hit = $false
                }
            }
            if ($hit) {
                Remove-Item -LiteralPath $item.FullName -Force -ErrorAction SilentlyContinue
                if (-not (Test-Path -LiteralPath $item.FullName)) {
                    Write-Log "removed: $($item.Name)"
                    $removed = $true
                } else {
                    Write-Log "failed to remove: $($item.Name)"
                }
            }
        }
    } else {
        Write-Log "no pinned folder at $pinnedDir"
    }
} catch {
    Write-Log "remove: $($_.Exception.Message)"
}

if (-not $removed) { Write-Log 'nothing to unpin' }
exit 0
