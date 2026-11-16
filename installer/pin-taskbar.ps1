# Pin a shortcut to the Windows taskbar (best effort).
#
# Windows has no supported API for this. The only semi-official route is the
# shell "taskbarpin" verb, and Microsoft has been progressively disabling it
# (Windows 11 22H2+ and many managed machines ignore it silently). There is
# also no reliable way to pre-seed the pinned state: since Windows 7 the
# taskbar layout lives in a binary registry blob, so merely dropping a .lnk
# into "User Pinned\TaskBar" does not actually pin anything - which is why
# this script never fakes success that way.
#
# Sequence: invoke the verb, then verify that the shell really created the
# pinned item. Exit 0 only on verified success, 1 otherwise. The installer
# treats exit 1 as "skip quietly": a failed pin must never abort an install.
#
# Known limitation: the installer runs elevated, so the verb executes in an
# elevated shell context. Some Windows builds then pin for the administrator
# profile or refuse outright; both cases end in exit 1 here.
#
# NOTE: keep this file ASCII-only. powershell.exe 5.1 decodes .ps1 files as
# ANSI unless they carry a BOM, so non-ASCII text would be corrupted.

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Shortcut
)

$ErrorActionPreference = 'Stop'

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

function Get-PinnedTaskbarDir {
    Join-Path $env:APPDATA 'Microsoft\Internet Explorer\Quick Launch\User Pinned\TaskBar'
}

# A pin counts as successful only if the shell resurfaced the item. The
# shell may rename it, so match on the shortcut target as well as the name.
function Test-Pinned([string]$PinnedDir, [string]$TargetPath, [string[]]$ExpectedNames) {
    if (-not (Test-Path -LiteralPath $PinnedDir)) { return $false }

    $wsh = Get-WscriptShell
    foreach ($item in Get-ChildItem -LiteralPath $PinnedDir -Filter '*.lnk' -ErrorAction SilentlyContinue) {
        # Name match needs no COM, so it always works. -eq is case-insensitive
        # for strings, so "effibuddy" also matches "EffiBuddy.lnk".
        foreach ($name in $ExpectedNames) {
            if ($name -and ($item.BaseName -eq $name)) { return $true }
        }
        if ($null -eq $wsh) { continue }
        try {
            $target = $wsh.CreateShortcut($item.FullName).TargetPath
            if ($target -and $TargetPath -and ($target -eq $TargetPath)) { return $true }
        } catch {
            # Unreadable .lnk: ignore and keep looking.
        }
    }
    return $false
}

try {
    if (-not (Test-Path -LiteralPath $Shortcut)) {
        Write-Log "error: shortcut not found: $Shortcut"
        exit 1
    }

    # Resolve the target for verification. Failure here is not fatal: the
    # verb below may still succeed, and verification falls back to the name.
    $target = ''
    $wsh = Get-WscriptShell
    if ($null -ne $wsh) {
        try {
            $target = $wsh.CreateShortcut($Shortcut).TargetPath
        } catch {
            Write-Log "resolve: $($_.Exception.Message)"
        }
    }
    # Accept either the shortcut's own name or the exe's name; the shell may
    # keep either one when it materialises the pinned entry.
    $expectedNames = @([System.IO.Path]::GetFileNameWithoutExtension($Shortcut))
    if ($target) {
        $exeName = [System.IO.Path]::GetFileNameWithoutExtension($target)
        if ($exeName -and ($expectedNames -notcontains $exeName)) {
            $expectedNames += $exeName
        }
    }
    Write-Log "expect pinned name in [$($expectedNames -join ', ')]"

    $pinnedDir = Get-PinnedTaskbarDir
    if (Test-Pinned $pinnedDir $target $expectedNames) {
        Write-Log 'already pinned'
        exit 0
    }

    # Ask the shell to pin. InvokeVerb accepts canonical verb strings and
    # silently ignores unknown ones, which is why the result is verified
    # afterwards instead of trusting a return value.
    try {
        $shell = New-Object -ComObject Shell.Application
        $item = $shell.Namespace((Split-Path -LiteralPath $Shortcut -Parent)).ParseName((Split-Path -LiteralPath $Shortcut -Leaf))
        if ($null -eq $item) {
            Write-Log 'error: shell namespace could not resolve the shortcut'
            exit 1
        }

        $captions = @()
        foreach ($verb in $item.Verbs()) { $captions += ($verb.Name -replace '&', '') }
        Write-Log "verb: available captions [$($captions -join ' | ')]"

        foreach ($verbName in @('taskbarpin', 'Pin to taskbar')) {
            try {
                $item.InvokeVerb($verbName)
                Write-Log "verb: InvokeVerb('$verbName')"
            } catch {
                Write-Log "verb: InvokeVerb('$verbName') threw: $($_.Exception.Message)"
            }
        }
    } catch {
        Write-Log "verb: $($_.Exception.Message)"
    }

    # The shell updates the taskbar asynchronously; give it a moment.
    for ($i = 0; $i -lt 10; $i++) {
        Start-Sleep -Milliseconds 400
        if (Test-Pinned $pinnedDir $target $expectedNames) {
            Write-Log 'pinned'
            exit 0
        }
    }

    Write-Log 'result: not pinned (blocked by policy or unsupported Windows build)'
    exit 1
} catch {
    Write-Log "error: $($_.Exception.Message)"
    exit 1
}
