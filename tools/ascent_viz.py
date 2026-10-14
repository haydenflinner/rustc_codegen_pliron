#!/usr/bin/env python3
"""ascent_viz.py — render the `ascent!` programs in proots.rs as Mermaid
rule-dependency graphs. Derived relations are boxes; base (input)
relations are stadiums; an edge body -> head is a rule; a dashed edge is
a negated body atom. Usage:

    python3 tools/ascent_viz.py [proots.rs]

Prints one ```mermaid fenced graph per program, in source order.
"""
import re
import sys

SRC = sys.argv[1] if len(sys.argv) > 1 else "pliron-clif-opt/src/proots.rs"


def parse(src):
    """Yield (program, relations, rules): rules are (head, [body deps]).
    Each dep is (relation, negated)."""
    for block in re.findall(r"ascent::ascent!\s*\{(.*?)\n\}", src, re.S):
        program = re.search(r"struct\s+(\w+)\s*;", block).group(1)
        rels = re.findall(r"relation\s+(\w+)\s*\(([^)]*)\)", block)
        rules = []
        for m in re.finditer(
            r"(\w+)\s*\(([^)]*)\)\s*<--\s*(.*?);", block, re.S
        ):
            head, body = m.group(1), m.group(3)
            body = re.sub(r"if\b.*", "", body, flags=re.S)
            deps = [
                (a.group(2), a.group(1) == "!")
                for a in re.finditer(r"(!?)\s*(\w+)\s*\(", body)
            ]
            rules.append((head, deps))
        yield program, [r for r, _ in rels], rules


def mermaid(program, rels, rules):
    lines = ["graph LR", f"    subgraph {program}"]
    heads = {h for h, _ in rules}
    for r in rels:
        shape = (f"{r}[{r}]", f"{r}([{r}])")[r not in heads]
        lines.append(f"        {shape}")
    seen = set()
    for head, deps in rules:
        for dep, neg in deps:
            if dep not in rels or (dep, head, neg) in seen:
                continue
            seen.add((dep, head, neg))
            arrow = "-.-o" if neg else "-->"
            lines.append(f"        {dep} {arrow} {head}")
    lines.append("    end")
    return "\n".join(lines)


src = open(SRC).read()
for program, rels, rules in parse(src):
    print("```mermaid")
    print(mermaid(program, rels, rules))
    print("```\n")
