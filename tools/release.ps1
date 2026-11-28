# One command: commit -> push -> trigger GitHub Actions -> (optional) wait.
#
# Usage (from the repo root or the workspace root; release.bat forwards args):
#
#   release.bat                          # commit (if any) + push + trigger build
#   release.bat "fix: something"         # with a custom commit message
#   release.bat "feat: x" -Tag v0.2.4    # also create+push a tag (tag triggers build)
#   release.bat -Wait                     # wait for the run, print the conclusion
#   release.bat -Workflow rust-build.yml  # pick a workflow (default rust-build.yml)
#
# Triggering workflow_dispatch needs a token with the "workflow" scope, in an
# environment variable:
#
#   setx GH_TOKEN <your-token>            # once; new shells pick it up
#
# Without a token the script still pushes. Then either pass -Tag (a v* tag push
# triggers the build), or click "Run workflow" on the Actions page.
#
# This script never stores the token and never writes it to disk; it only reads
# the environment variable.
#
# NOTE: keep this file pure ASCII. Windows PowerShell 5.1 reads .ps1 files as
# ANSI when there is no BOM, so non-ASCII here breaks string literals.

[CmdletBinding()]
param(
    # Commit message. When given, current changes are committed
    # (equivalent to: git add -A ; git commit -m).
    [Parameter(Position = 0)]
    [string]$Message,

    # Optional: create and push this tag (e.g. v0.2.4). A tag push triggers builds.
    [string]$Tag,

    # Optional: wait for the run to finish and print the conclusion.
    [switch]$Wait,

    # Workflow file name under .github/workflows/.
    [string]$Workflow = 'rust-build.yml',

    # Remote name.
    [string]$Remote = 'origin',

    # Branch.
    [string]$Branch = 'main'
)

$ErrorActionPreference = 'Stop'

# Only ever push to your own fork. A mistyped remote could aim elsewhere.
$AllowedUrls = @(
    'https://github.com/DangJiYi/chat-demo',
    'https://github.com/DangJiYi/chat-demo.git',
    'git@github.com:DangJiYi/chat-demo.git'
)
$RepoSlug = 'DangJiYi/chat-demo'

function Fail($msg) {
    Write-Host ""
    Write-Host "[FAIL] $msg" -ForegroundColor Red
    exit 1
}

function Step($msg) {
    Write-Host ""
    Write-Host "== $msg" -ForegroundColor Cyan
}

# ---------------------------------------------------------------- locate repo

Step 'Locate repository'
$here = if ($PSScriptRoot) { Split-Path -Parent $PSScriptRoot } else { (Get-Location).Path }
if (Test-Path (Join-Path $here 'chat\.git')) {
    $repo = Join-Path $here 'chat'
} elseif (Test-Path (Join-Path $here '.git')) {
    $repo = $here
} else {
    Fail "No git repository: neither .git nor chat\.git under $here"
}
Set-Location $repo
Write-Host "  repo: $repo"

# ---------------------------------------------------------------- check remotes

Step 'Check remotes'
$remotes = @(git remote)
if ($remotes.Count -eq 0) { Fail 'No git remote configured' }
foreach ($r in $remotes) {
    $url = (git remote get-url --push $r) -join ''
    $ok = $AllowedUrls -contains $url
    $mark = if ($ok) { 'ok' } else { 'NOT ALLOWED' }
    Write-Host ("  {0,-10} => {1}  [{2}]" -f $r, $url, $mark)
    if (-not $ok) {
        Fail "Remote '$r' points at $url, which is not your fork. Fix: git remote set-url $r https://github.com/$RepoSlug"
    }
}

# ---------------------------------------------------------------- commit

Step 'Commit changes'
git add -A
$staged = (git diff --cached --name-only) -join "`n"
if ([string]::IsNullOrWhiteSpace($staged)) {
    Write-Host '  nothing to commit'
} else {
    if ([string]::IsNullOrWhiteSpace($Message)) {
        $Message = 'chore: sync ' + (Get-Date -Format 'yyyy-MM-dd HH:mm')
        Write-Host "  no message given, using: $Message"
    }
    Write-Host '  staged:'
    git diff --cached --name-status | ForEach-Object { Write-Host "    $_" }
    git commit -m $Message | Out-Null
    if ($LASTEXITCODE -ne 0) { Fail 'git commit failed' }
    Write-Host '  [OK] committed'
}

# ---------------------------------------------------------------- push

Step "Push to $Remote/$Branch"
git fetch $Remote $Branch --quiet
$ahead = (git rev-list --count "$Remote/$Branch..HEAD")
Write-Host "  ahead by $ahead commit(s)"
if ($ahead -gt 0) {
    git push $Remote $Branch
    if ($LASTEXITCODE -ne 0) {
        Fail "Push failed. Usual causes: remote has new commits (git pull --rebase $Remote $Branch), or stale credentials (remove the github.com entry in Windows Credential Manager)."
    }
    Write-Host '  [OK] pushed'
} else {
    Write-Host '  remote already up to date, skipping push'
}

$sha = (git rev-parse HEAD).Trim()
Write-Host "  HEAD: $sha"

# ---------------------------------------------------------------- tag (optional)

if ($Tag) {
    Step "Create tag: $Tag"
    if ($Tag -notmatch '^v') {
        Write-Host "  [WARN] tag does not start with 'v'; workflows only listen for v*, so it will not trigger a build" -ForegroundColor Yellow
    }
    git tag $Tag 2>$null
    if ($LASTEXITCODE -ne 0) {
        Write-Host '  tag already exists, reusing the local one'
    }
    git push $Remote $Tag
    if ($LASTEXITCODE -ne 0) { Fail "Failed to push tag: $Tag" }
    Write-Host '  [OK] tag pushed; the workflow will be triggered by it'
}

# ---------------------------------------------------------------- trigger workflow

$token = $env:GH_TOKEN
if ([string]::IsNullOrWhiteSpace($token)) {
    $token = $env:GITHUB_TOKEN
}

$headers = @{
    'Accept'               = 'application/vnd.github+json'
    'X-GitHub-Api-Version' = '2022-11-28'
}

if ([string]::IsNullOrWhiteSpace($token)) {
    Step 'Trigger workflow'
    Write-Host '  GH_TOKEN is not set, skipping the API trigger.' -ForegroundColor Yellow
    Write-Host '  For full automation: setx GH_TOKEN <token with repo + workflow scope>, then reopen the shell.'
    if (-not $Tag) {
        Write-Host '  For now you can either:'
        Write-Host "    - rerun with -Tag v$((Get-Date).ToString('yy.MM.dd'))"
        Write-Host "    - or open https://github.com/$RepoSlug/actions and click Run workflow"
    }
    exit 0
}

$headers['Authorization'] = "Bearer $token"

Step "Trigger workflow $Workflow"
$before = $null
try {
    $runs = Invoke-RestMethod -Uri "https://api.github.com/repos/$RepoSlug/actions/workflows/$Workflow/runs?per_page=1" `
        -Headers $headers -Method Get
    if ($runs.workflow_runs.Count -gt 0) { $before = $runs.workflow_runs[0].id }
} catch {
    Write-Host "  [WARN] could not read previous runs (triggering anyway): $($_.Exception.Message)" -ForegroundColor Yellow
}

$body = @{ ref = $Branch } | ConvertTo-Json -Compress
try {
    Invoke-RestMethod -Uri "https://api.github.com/repos/$RepoSlug/actions/workflows/$Workflow/dispatches" `
        -Headers $headers -Method Post -Body $body -ContentType 'application/json' | Out-Null
} catch {
    $detail = $_.Exception.Message
    Write-Host ""
    Write-Host "[FAIL] dispatch failed: $detail" -ForegroundColor Red
    Write-Host '  403/404 usually means the token lacks the workflow scope, or the repo slug is wrong.'
    Write-Host "  You can also open https://github.com/$RepoSlug/actions and click Run workflow"
    exit 1
}
Write-Host '  [OK] dispatch requested'

# GitHub takes a moment to create the run; poll briefly for it.
$runId = $null
for ($i = 0; $i -lt 20; $i++) {
    Start-Sleep -Milliseconds 1500
    try {
        $runs = Invoke-RestMethod -Uri "https://api.github.com/repos/$RepoSlug/actions/workflows/$Workflow/runs?per_page=5" `
            -Headers $headers -Method Get
        foreach ($r in $runs.workflow_runs) {
            if ($r.head_sha -eq $sha -and $r.id -ne $before) {
                $runId = $r.id
                break
            }
        }
    } catch { }
    if ($runId) { break }
}

if (-not $runId) {
    Write-Host '  [WARN] no new run appeared yet; check the Actions page:' -ForegroundColor Yellow
    Write-Host "    https://github.com/$RepoSlug/actions"
    exit 0
}

$runUrl = "https://github.com/$RepoSlug/actions/runs/$runId"
Write-Host "  run: $runUrl"

if (-not $Wait) {
    Write-Host ''
    Write-Host '  add -Wait to print the conclusion when it finishes.'
    exit 0
}

Step 'Waiting for the run to finish'
$conclusion = $null
for ($i = 0; $i -lt 240; $i++) {
    Start-Sleep -Seconds 5
    try {
        $run = Invoke-RestMethod -Uri "https://api.github.com/repos/$RepoSlug/actions/runs/$runId" -Headers $headers
    } catch {
        Write-Host '  (query failed, retrying)' -ForegroundColor DarkGray
        continue
    }
    Write-Host ("  [{0}] {1}" -f $run.status, $run.conclusion)
    if ($run.status -eq 'completed') {
        $conclusion = $run.conclusion
        break
    }
}

Write-Host ''
if ($conclusion -eq 'success') {
    Write-Host '[OK] build succeeded' -ForegroundColor Green
    Write-Host "  artifacts: near the bottom of $runUrl"
} elseif ($conclusion) {
    Write-Host "[FAIL] conclusion: $conclusion" -ForegroundColor Red
    Write-Host "  logs: $runUrl"
    exit 1
} else {
    Write-Host '[WARN] timed out after 20 minutes; check:' -ForegroundColor Yellow
    Write-Host "  $runUrl"
}
