# Platforms

## Linux

Ubuntu 24.04 or newer is what it is tested on; the release binary is built
against glibc 2.31, so older distributions run it too. `ideae --install`
(which the install script runs) lists it with the applications, where it can
be pinned to the dock.

## macOS

Apple Silicon. The build is signed ad hoc, without an Apple Developer ID:
installed with the script it opens as it is, but an `.app` downloaded with a
browser needs right click → Open the first time (or
`xattr -d com.apple.quarantine ideae.app`). Shortcuts use Cmd where Linux uses
Ctrl.

macOS gives permissions (Screen Recording, Accessibility) to a signature, and
an ad-hoc one changes with every build, so they are asked for again after an
update.

Not there yet: clicks on notifications, the Kotlin language server install.

## Windows

Windows 10 1903 or newer, x64. The `.exe` needs only system DLLs. Terminals
run PowerShell (`pwsh` when installed). Git must be on PATH; Claude Code on
Windows brings Git Bash, which also runs the agent hooks. The headless
browser tab uses Chrome, or Edge when there is no Chrome. State and language
servers live in `%LOCALAPPDATA%\ideae`.

Not there yet: the Kotlin language server install.

## Updates

Release builds ask GitHub for this repository's latest release at start and
every six hours (Settings → General → Updates turns that off). A newer one
shows "↑ update" in the title bar: it downloads that release's build next to
the running one, puts it in its place and restarts. On Linux the agents
keep running through the restart.
