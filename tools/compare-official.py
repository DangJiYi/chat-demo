"""Compare the official frontend bundle with our build.

Whenever an official release drops (a zip with static/js/main.*.js), this tells
you whether it actually changed anything user-facing, before you invest time in
rebasing our fork onto it.

Usage (from chat/):

    python tools/compare-official.py path/to/official/main.xxxx.js
    python tools/compare-official.py path/to/official/main.xxxx.js www/static/js/main.yyyy.js

Defaults for the second argument: the newest www/static/js/main.*.js.

What it reports:
  1. Chinese UI strings only present in one side  -> new/removed UI text
  2. Long identifiers only present in one side    -> new/removed internal APIs

Caveats worth knowing:
  - The official bundle is the pure-static variant (IndexedDB storage), ours is
    the local-service variant, so a big chunk of "official-only" identifiers will
    always be IndexedDB/Canvas internals. That is expected, not a new feature.
  - This is a smoke signal, not a diff. For real porting you still need source.

Output is ASCII-only on purpose: it runs on Windows CI/dev shells where stdout
is cp1252 and printing CJK would raise UnicodeEncodeError.
"""

import glob
import os
import re
import sys

CJK = re.compile(r"[\u4e00-\u9fa5]{2,14}")
IDENT = re.compile(r"[a-z][a-zA-Z0-9]{6,30}")

# Substrings that only show up because official uses IndexedDB and we do not.
IDB_NOISE = (
    "indexeddb",
    "objectstore",
    "objectstorenames",
    "createdobjectstore",
    "createobjectstore",
    "createindex",
    "openCursor".lower(),
    "getallkeys",
    "continueprimarykey",
    "readwrite",
    "transaction",
    "keypath",
    "multientry",
    "indexnames",
    "oldversion",
    "newversion",
)


def read(path):
    with open(path, encoding="utf-8", errors="ignore") as f:
        return f.read()


def set_of(pattern, text):
    return set(pattern.findall(text))


def newest_local():
    cands = glob.glob(os.path.join("www", "static", "js", "main.*.js"))
    if not cands:
        return None
    return max(cands, key=os.path.getmtime)


def summary(title, only_a, only_b, label_a, label_b, limit=25):
    print("")
    print("== %s" % title)
    print("  only in %s: %d" % (label_a, len(only_a)))
    for item in only_a[:limit]:
        print("    %s" % item)
    if len(only_a) > limit:
        print("    ... and %d more" % (len(only_a) - limit))
    print("  only in %s: %d" % (label_b, len(only_b)))
    for item in only_b[:limit]:
        print("    %s" % item)
    if len(only_b) > limit:
        print("    ... and %d more" % (len(only_b) - limit))


def main(argv):
    if len(argv) < 2:
        print(__doc__)
        return 2
    off_path = argv[1]
    ours_path = argv[2] if len(argv) > 2 else newest_local()

    if not os.path.isfile(off_path):
        print("official bundle not found: %s" % off_path)
        return 1
    if not ours_path or not os.path.isfile(ours_path):
        print("our bundle not found; pass it explicitly")
        return 1

    print("official: %s" % off_path)
    print("ours    : %s" % ours_path)
    print("official size: %d bytes" % os.path.getsize(off_path))
    print("our size     : %d bytes" % os.path.getsize(ours_path))

    off = read(off_path)
    ours = read(ours_path)

    a_cjk, b_cjk = set_of(CJK, off), set_of(CJK, ours)
    summary(
        "Chinese UI strings",
        sorted(a_cjk - b_cjk),
        sorted(b_cjk - a_cjk),
        "official",
        "ours",
    )

    a_id, b_id = set_of(IDENT, off), set_of(IDENT, ours)
    off_only = sorted(x for x in (a_id - b_id) if x.lower() not in IDB_NOISE)
    summary("Identifiers (IndexedDB noise filtered)", off_only, sorted(b_id - a_id), "official", "ours")

    print("")
    # The verdict keys off USER-FACING text only. Identifiers differ structurally
    # here (official keeps IndexedDB, ours talks to the local service), so they
    # can never be empty and would make every comparison say "differs".
    off_only_cjk = sorted(a_cjk - b_cjk)
    if not off_only_cjk:
        print("Verdict: no new user-facing text in the official build.")
        print("         Treat it as a bugfix/maintenance release.")
        print("         To absorb the fixes you still need its SOURCE;")
        print("         a minified dist bundle cannot be merged.")
    else:
        print("Verdict: the official build has %d new UI string(s); look at them above." % len(off_only_cjk))
        print("         Porting still needs the SOURCE, not this bundle.")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
