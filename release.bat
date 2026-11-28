@echo off
rem One command: commit -> push -> trigger GitHub Actions -> (optional) wait.
rem
rem   release.bat                          commit (if any) + push + trigger build
rem   release.bat "fix: something"         with a commit message
rem   release.bat "feat: x" -Tag v0.2.4    also tag and push (tag triggers build)
rem   release.bat -Wait                     wait for the run and print the result
rem
rem Auto-trigger needs a token with workflow scope:
rem   setx GH_TOKEN <your-token>
rem Without a token it still pushes; use -Tag, or click Run workflow on the site.
rem
rem All the real logic lives in tools\release.ps1 (UTF-8, easier to maintain).

setlocal
set "PS1=%~dp0tools\release.ps1"
if not exist "%PS1%" (
  echo [FAIL] Missing "%PS1%"
  echo        This script must sit next to tools\release.ps1
  pause
  exit /b 1
)

powershell -NoProfile -ExecutionPolicy Bypass -File "%PS1%" %*
set "RC=%ERRORLEVEL%"
if not "%RC%"=="0" (
  echo.
  echo [exit %RC%]
)
pause
exit /b %RC%
