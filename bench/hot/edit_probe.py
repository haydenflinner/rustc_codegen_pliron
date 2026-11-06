#!/usr/bin/env python3
"""Surgically rewrite probe()'s body in the generated leaf source.

Usage: edit_probe.py LIB_RS BODY_EXPR
Replaces the single expression inside `pub fn probe` (marked by the
HOT_EDIT comment) with BODY_EXPR, e.g. `1000 + COUNTER.load(Ordering::Relaxed)`.
"""
import re
import sys

path, body = sys.argv[1], sys.argv[2]
src = open(path).read()
pat = re.compile(
    r"(pub fn probe\(_x: u64\) -> u64 \{\n"
    r"    // HOT_EDIT marker: the harness rewrites this body\.\n)"
    r"    [^\n]*\n"
    r"(\})"
)
new, n = pat.subn(lambda m: m.group(1) + "    " + body + "\n" + m.group(2), src)
if n != 1:
    sys.exit(f"probe marker matched {n} times in {path}")
open(path, "w").write(new)
