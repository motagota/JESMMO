@echo off
rem Launch the client in terrain editing mode (#78): free-fly camera + height
rem brush. Everything after Godot's "--" separator reaches Main.gd as a user arg.
rem
rem Logs in as EDITOR_EMAIL/EDITOR_PASS if set (any editor-role account works,
rem and edits are attributed to it), else the server-seeded dev editor account.
rem Example:
rem   set EDITOR_EMAIL=test@test.com
rem   set EDITOR_PASS=yourpassword
rem   terrain_editor.bat
rem
rem Uses the Godot binary in GODOT if set, else the local dev default.
rem The servers must be running first: ..\start_servers.ps1
setlocal
if not defined GODOT set "GODOT=D:\dev\Godot.exe"
if not exist "%GODOT%" (
    echo Godot not found at "%GODOT%" - set the GODOT environment variable to your Godot 4 executable.
    exit /b 1
)
set "LOGIN_ARGS="
if defined EDITOR_EMAIL set "LOGIN_ARGS=--editor-email=%EDITOR_EMAIL% --editor-pass=%EDITOR_PASS%"
rem "%~dp0." (trailing dot) dodges cmd's backslash-before-closing-quote escaping.
start "" "%GODOT%" --path "%~dp0." -- --editor-mode %LOGIN_ARGS%
