"""生成一份 `embedded_www.rs` 的模拟件并做语法校验。

**这是开发辅助脚本，不参与构建。** 用途只有一个：
`build.rs` 里那段生成代码是字符串拼接，写错括号这种事语法检查抓不到
（生成物才在 CI 上炸）。这个脚本用同样的规则生成一份真实产物，
交给 rustfmt 解析——本地就能发现形状错误，不用等一轮 CI。

用法（在 rust/ 目录下）：

    python gen_check.py            # 生成 _gen_check.rs 并校验
    python gen_check.py --keep     # 保留 _gen_check.rs 便于肉眼核对
"""

import json
import os
import subprocess
import sys

KEEP = "--keep" in sys.argv
OUT = "_gen_check.rs"


def main() -> int:
    root = os.path.abspath(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "www"))
    if not os.path.isdir(root):
        print("找不到前端目录：%s" % root)
        return 1

    files = []
    for dirpath, _dirnames, filenames in os.walk(root):
        for fn in filenames:
            full = os.path.join(dirpath, fn)
            rel = os.path.relpath(full, root).replace(os.sep, "/")
            files.append((rel, full))
    files.sort()

    # 与 build.rs 的拼法保持一致：一行一个元组，
    #     ("rel", include_bytes!("abs")),
    # 字符串用 json.dumps 转义——对反斜杠和引号的处理与 Rust 的 {:?} 相同。
    lines = ["pub static EMBEDDED_WWW: &[(&str, &[u8])] = &["]
    for rel, full in files:
        lines.append("    (%s, include_bytes!(%s))," % (json.dumps(rel), json.dumps(full)))
    lines.append("];")

    with open(OUT, "w", encoding="utf-8") as f:
        f.write("\n".join(lines) + "\n")

    print("文件数:", len(files))
    print("前 3 行:")
    for line in lines[1:4]:
        print(line)

    try:
        result = subprocess.run(
            ["rustfmt", "--edition", "2021", "--check", OUT],
            capture_output=True,
            text=True,
        )
    except FileNotFoundError:
        print("没装 rustfmt，跳过语法校验")
        return 0

    if result.returncode != 0:
        print("语法不通过（CI 会在这一步炸）：")
        print(result.stderr.strip()[:2000])
        return 1
    print("语法通过")

    if not KEEP:
        os.remove(OUT)
    return 0


if __name__ == "__main__":
    sys.exit(main())
