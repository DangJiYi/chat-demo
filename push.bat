@echo off
rem Commit + push, which triggers the rust-build GitHub Actions workflow.
rem
rem   push.bat                 commit everything and push
rem   push.bat fix voice input with a custom commit message
rem   push.bat --dry-run       show what would be committed, do not push
rem
rem The build is triggered by .github/workflows/rust-build.yml on push to main.
rem No token needed. The Actions link is printed at the end.
rem
rem Two hard-won notes, do not "simplify" these away:
rem   1. Never do `echo %CMDCMDLINE% ^| find /c` to detect a double-click.
rem      That variable expands to a quoted command line; combined with a pipe
rem      the quotes break and the whole script dies instantly (window flashes).
rem   2. Keep this file pure ASCII. cmd parses .bat as GBK/ANSI, so CJK bytes
rem      inside an `if (` block can corrupt the block and kill the script.
rem      Chinese notes live in BUILD.md instead.

chcp 437 >nul
setlocal
cd /d "%~dp0"
set RC=1

if not exist "%~dp0tools\push.py" (
  echo [error] tools\push.py not found. Keep this .bat next to tools\.
  goto :done
)

rem Prefer the py launcher: it bypasses the WindowsApps python.exe stub,
rem which opens the Microsoft Store instead of running anything.
set "PY="
where py >nul 2>nul && set "PY=py -3"
if not defined PY (
  where python >nul 2>nul && set "PY=python"
)
if not defined PY (
  echo [error] python not found. Install Python 3 with "Add to PATH".
  goto :done
)

rem Mirror output to push.log so a crashed run still leaves evidence.
set "LOG=%~dp0push.log"
%PY% "%~dp0tools\push.py" %* > "%LOG%" 2>&1
set RC=%ERRORLEVEL%

if exist "%LOG%" type "%LOG%"
echo.
if not "%RC%"=="0" (
  echo [error] exit code %RC%  ^(details in push.log^)
) else (
  echo [ok] done. Check the Actions link above for the build result.
)

:done
echo.
pause
exit /b %RC%
