@echo off
rem Launch the client in normal play mode (login UI + player character).
rem Uses the Godot binary in GODOT if set, else the local dev default.
rem The servers must be running first: ..\start_servers.ps1
setlocal
if not defined GODOT set "GODOT=D:\dev\Godot.exe"
if not exist "%GODOT%" (
    echo Godot not found at "%GODOT%" - set the GODOT environment variable to your Godot 4 executable.
    exit /b 1
)
rem "%~dp0." (trailing dot) dodges cmd's backslash-before-closing-quote escaping.
start "" "%GODOT%" --path "%~dp0."
