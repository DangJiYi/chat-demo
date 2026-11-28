#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""add -> commit -> push，然后打印本次触发的 Actions 页面地址。

只做这一件事：不做版本号自增、不打 tag、不发 release、不假装知道构建结果。
构建由 `.github/workflows/rust-build.yml` 在 push 到 main 时自动跑
（该工作流的 `on.push.branches` 已包含 main；早期只监听 tag，所以推代码没反应）。

用法（在 chat/ 下，或双击 push.bat）：

    push.bat                    提交全部改动并推送
    push.bat 修好语音输入        自定义提交信息
    push.bat --dry-run          只看会提交什么，不推
"""

import re
import subprocess
import sys
from pathlib import Path

# Windows 控制台默认 cp1252，直接 print 中文会 UnicodeEncodeError。
for stream in (sys.stdout, sys.stderr):
    try:
        stream.reconfigure(encoding="utf-8", errors="replace")
    except Exception:
        pass

ROOT = Path(__file__).resolve().parent.parent
WORKFLOW = "rust-build.yml"  # .github/workflows/ 下的文件名


def run(args, check=True):
    proc = subprocess.run(
        args,
        cwd=str(ROOT),
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
    )
    if check and proc.returncode != 0:
        sys.stderr.write("[error] `%s` 失败\n" % " ".join(args))
        if proc.stdout.strip():
            sys.stderr.write(proc.stdout.strip() + "\n")
        if proc.stderr.strip():
            sys.stderr.write(proc.stderr.strip() + "\n")
        sys.exit(proc.returncode)
    return proc


def repo_url():
    """把 remote 地址规范成 https://github.com/OWNER/REPO。"""
    raw = run(["git", "remote", "get-url", "origin"], check=False)
    url = (raw.stdout or "").strip()
    if not url:
        return None
    url = re.sub(r"\.git$", "", url)
    m = re.match(r"^git@([^:]+):(.+)$", url)
    if m:
        return "https://%s/%s" % (m.group(1), m.group(2))
    m = re.match(r"^(https?://[^/]+/[^/]+/[^/]+)$", url)
    return m.group(1) if m else None


def main(argv):
    dry = "--dry-run" in argv
    words = [a for a in argv if a != "--dry-run"]
    message = " ".join(words).strip() or "update"

    branch = (run(["git", "branch", "--show-current"]).stdout or "").strip()
    if not branch:
        sys.stderr.write("[error] detached HEAD，先切回分支\n")
        return 2

    run(["git", "add", "-A"])

    staged = run(["git", "diff", "--cached", "--name-only"], check=False)
    files = [f for f in (staged.stdout or "").splitlines() if f.strip()]
    if files:
        print("[ok] 暂存 %d 个文件" % len(files))
        for f in files[:20]:
            print("     %s" % f)
        if len(files) > 20:
            print("     … 另有 %d 个" % (len(files) - 20))
        if dry:
            print("[warn] --dry-run：没有提交也没有推送")
            return 0
        run(["git", "commit", "-m", message])
        sha = (run(["git", "rev-parse", "--short", "HEAD"]).stdout or "").strip()
        print("[ok] 已提交 %s" % sha)
    else:
        print("[warn] 没有改动，跳过提交")
        if dry:
            return 0

    push = run(["git", "push", "origin", branch], check=False)
    if push.returncode != 0:
        sys.stderr.write((push.stderr or "") + "\n")
        sys.stderr.write(
            "[error] 推送失败。常见原因：没有凭据、分支受保护、需要先 pull。\n"
            "        凭据助手查法：git config --get credential.helper\n"
            "        没配就配全局：git config --global credential.helper manager\n"
        )
        return push.returncode

    print("[ok] 已推送 origin/%s" % branch)
    base = repo_url()
    if base:
        print("\n构建（push 到 main 会自动触发）：")
        print("  %s/actions/workflows/%s" % (base, WORKFLOW))
        print("产物在 run 页面底部的 Artifacts。")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
