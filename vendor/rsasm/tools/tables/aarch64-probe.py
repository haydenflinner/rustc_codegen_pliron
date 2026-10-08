#!/usr/bin/env python3
"""Fits one line the way aarch64.py does and prints what it measured.

    tools/tables/aarch64-probe.py 'scvtf s0, w1, #3'
"""
import os
import random
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import a64  # noqa: E402
import importlib  # noqa: E402

gen = importlib.import_module("aarch64")  # the generator next to this file


def main():
    a64.ensure_codes()
    for text in sys.argv[1:]:
        w = a64.assemble([text])[0]
        if w is None:
            print("%s: llvm-mc refuses it" % text)
            continue
        mn, atoms = a64.parse_line(text)
        print("%s  ->  %08x" % (text, w))
        try:
            atoms2, slots, base, sp = gen.fit_form(mn, [(w, atoms)], random.Random(1))
        except gen.Fail as e:
            print("  fail: %s" % e)
            continue
        print("  base %08x" % base)
        for sl in slots:
            print("  operand %d.%d: %s  (%d values)" % (sl["slot"][0], sl["slot"][1],
                                                     sl["enc"], len(sl["values"])))
        try:
            gen.check_form(mn, atoms2, slots, base, sp, random.Random(2))
            print("  checked")
        except gen.Fail as e:
            print("  check: %s" % e)


if __name__ == "__main__":
    main()
