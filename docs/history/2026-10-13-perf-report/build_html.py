#!/usr/bin/env python3
"""build_html.py — render wasm-and-native-performance.md to one self-contained HTML page
(charts inlined). Minimal converter for the Markdown subset the report uses."""
import html
import os
import re

HERE = os.path.dirname(os.path.abspath(__file__))
src = open(os.path.join(HERE, "wasm-and-native-performance.md"), encoding="utf-8").read().split("\n")


def inline(t):
    t = html.escape(t, quote=False)
    t = re.sub(r"`([^`]+)`", r"<code>\1</code>", t)
    t = re.sub(r"\*\*([^*]+)\*\*", r"<strong>\1</strong>", t)
    return t


out = []
i = 0
title = "pliron against LLVM"
while i < len(src):
    ln = src[i]
    if ln.startswith("```"):
        j = i + 1
        buf = []
        while not src[j].startswith("```"):
            buf.append(src[j])
            j += 1
        out.append("<pre><code>" + html.escape("\n".join(buf)) + "</code></pre>")
        i = j + 1
        continue
    m = re.match(r"^(#{1,3}) (.*)", ln)
    if m:
        n = len(m.group(1))
        out.append(f"<h{n}>{inline(m.group(2))}</h{n}>")
        i += 1
        continue
    m = re.match(r"^!\[(.*)\]\((.*)\)", ln)
    if m:
        svg = open(os.path.join(HERE, m.group(2)), encoding="utf-8").read()
        out.append(f'<figure role="img" aria-label="{html.escape(m.group(1))}">{svg}</figure>')
        i += 1
        continue
    if ln.startswith("|"):
        rows = []
        while i < len(src) and src[i].startswith("|"):
            rows.append([c.strip() for c in src[i].strip().strip("|").split("|")])
            i += 1
        head, body = rows[0], rows[2:]
        t = ["<div class=\"tw\"><table><thead><tr>" + "".join(f"<th>{inline(c)}</th>" for c in head) + "</tr></thead><tbody>"]
        for r in body:
            cells = []
            for k, c in enumerate(r):
                cls = ' class="n"' if re.match(r"^[\d.,\s]+(B|x| s| ms)?$", c.replace("**", "")) and k > 0 else ""
                cells.append(f"<td{cls}>{inline(c)}</td>")
            t.append("<tr>" + "".join(cells) + "</tr>")
        t.append("</tbody></table></div>")
        out.append("".join(t))
        continue
    if re.match(r"^(- |\d+\. )", ln):
        ordered = bool(re.match(r"^\d+\. ", ln))
        items = []
        while i < len(src) and re.match(r"^(- |\d+\. )", src[i]):
            items.append(re.sub(r"^(- |\d+\. )", "", src[i]))
            i += 1
        tag = "ol" if ordered else "ul"
        out.append(f"<{tag}>" + "".join(f"<li>{inline(x)}</li>" for x in items) + f"</{tag}>")
        continue
    if ln.strip() == "":
        i += 1
        continue
    para = [ln]
    i += 1
    while i < len(src) and src[i].strip() and not re.match(r"^(#|\||```|- |\d+\. |!\[)", src[i]):
        para.append(src[i])
        i += 1
    out.append("<p>" + inline(" ".join(para)) + "</p>")

body = "\n".join(out)
page = """<title>pliron against LLVM</title>
<link rel="stylesheet" href="https://fonts.googleapis.com/css2?family=IBM+Plex+Mono&family=IBM+Plex+Sans:wght@400;600&family=Source+Serif+4:wght@400;600&display=swap">
<style>
:root {
  /* layout: one reading column, tables and charts may be wider and scroll inside their own box */
  --bg: #fbfaf8; --fg: #1c1f24; --muted: #5b6370; --rule: #dcd8d0; --accent: #1f5a8a;
  --code-bg: #efece6; --th: #e8e4dc; --chart-bg: #ffffff;
  --serif: "Source Serif 4", Georgia, "Times New Roman", serif;
  --sans: "IBM Plex Sans", "Segoe UI", system-ui, sans-serif;
  --mono: "IBM Plex Mono", Consolas, "Courier New", monospace;
}
@media (prefers-color-scheme: dark) {
  :root:not([data-theme="light"]) {
    --bg: #15181c; --fg: #e6e3dc; --muted: #9aa3ad; --rule: #333840; --accent: #7fb2e0;
    --code-bg: #20252b; --th: #232930; --chart-bg: #ffffff; color-scheme: dark;
  }
}
:root[data-theme="dark"] {
  --bg: #15181c; --fg: #e6e3dc; --muted: #9aa3ad; --rule: #333840; --accent: #7fb2e0;
  --code-bg: #20252b; --th: #232930; --chart-bg: #ffffff; color-scheme: dark;
}
body { background: var(--bg); color: var(--fg); font-family: var(--serif); font-size: 17px; line-height: 1.6;
  padding-inline: 16px; padding-block: 32px; margin: 0; }
main { max-width: 46rem; margin-inline: auto; }
h1, h2, h3 { font-family: var(--sans); line-height: 1.2; text-wrap: balance; }
h1 { font-size: 1.9rem; margin: 0 0 1rem; }
h2 { font-size: 1.3rem; margin: 2.4rem 0 0.8rem; padding-top: 1rem; border-top: 1px solid var(--rule); }
h3 { font-size: 1.05rem; margin: 1.6rem 0 0.4rem; color: var(--muted); }
p, li { max-width: 65ch; }
code { font-family: var(--mono); font-size: 0.86em; background: var(--code-bg); padding: 0.05em 0.3em; border-radius: 3px; }
pre { background: var(--code-bg); padding: 12px 14px; border-radius: 4px; overflow-x: auto; }
pre code { background: none; padding: 0; }
.tw { overflow-x: auto; margin: 1rem 0; }
table { border-collapse: collapse; font-family: var(--sans); font-size: 0.86rem; min-width: 100%; }
th, td { text-align: left; padding: 6px 10px; border-bottom: 1px solid var(--rule); vertical-align: top; }
th { background: var(--th); font-weight: 600; }
td.n { text-align: right; font-variant-numeric: tabular-nums; white-space: nowrap; }
figure { margin: 1.4rem 0; background: var(--chart-bg); border: 1px solid var(--rule); border-radius: 4px;
  padding: 8px; overflow-x: auto; }
figure svg { width: 100%; height: auto; min-width: 520px; display: block; }
a { color: var(--accent); }
</style>
<main>
""" + body + "\n</main>\n"
open(os.path.join(HERE, "report.html"), "w", encoding="utf-8").write(page)
print("report.html", len(page))
