// pliron-spec.typ -- theme and requirement machinery for the
// rustc_codegen_pliron specification. Adapted from dsa's nina-spec.typ.
//
// Public API (see README.md):
//   #show: spec.with(..)        document template
//   #cover(..)  #how-to-read()  front matter
//   #chapter("id", tier)        start a chapter (sets default tier)
//   #chapter-glance()           per-chapter level summary
//   #r("a.b", ..)[body]         a requirement (tracey-visible)
//   #refr("a.b")                clickable reference to a requirement
//   #dfn[..] #note[..] #drift[..] #example[..]   prose helpers
//   #llvm[..]                   "compared with LLVM" note, collected at the back
//   #level-map()  #req-index()  #differences()  #checks-index()   back matter
//
// Tracey reads `#r("id", level: "must", ..)[body]` straight from the source
// text and ignores every other named argument; this file gives those extra
// arguments (tier, title, refines, depends, see, status, checked) their
// meaning.

// ---------------------------------------------------------------- tokens

#let ink = rgb("#1c1c21")
#let muted = rgb("#6a6a75")
#let hair = rgb("#d9d6cf")
#let paper = rgb("#fdfcf9")

#let serif = "Libertinus Serif"
#let sans = "Atkinson Hyperlegible Next"
#let mono = "Atkinson Hyperlegible Mono"

// Levels (tiers). L0 is the model, L1 behaviour, L2 formats and language.
#let tiers = (
  "0": (short: "L0", name: "Model", color: rgb("#4a3f9e"), tint: rgb("#f1effb")),
  "1": (short: "L1", name: "Behaviour", color: rgb("#0b716c"), tint: rgb("#e9f5f4")),
  "2": (short: "L2", name: "Reference", color: rgb("#9c540a"), tint: rgb("#fbf1e4")),
)
#let tier-of(t) = tiers.at(str(t))

// ---------------------------------------------------------------- state

#let cur-tier = state("spec-cur-tier", 0)
#let cur-chap = state("spec-cur-chapter", "")
#let reg = <spec-req>
#let cur-req = state("spec-cur-req", none)

#let as-array(x) = if x == none { () } else if type(x) == array { x } else { (x,) }

#let all-reqs() = query(reg).map(e => e.value)

// ---------------------------------------------------------------- badges

#let badge(body, fill: ink, color: white, stroke: none) = box(
  fill: fill,
  stroke: stroke,
  radius: 2.2pt,
  inset: (x: 4.2pt, y: 0pt),
  outset: (y: 2.2pt),
  baseline: 0pt,
  text(font: sans, size: 6.6pt, weight: 700, tracking: 0.07em, fill: color, upper(body)),
)

#let tier-badge(t) = {
  let T = tier-of(t)
  badge(T.short, fill: T.color)
}

#let level-badge(l) = {
  let l = lower(l)
  if l == "must" { badge("must", fill: rgb("#7a1d2b")) }
  else if l == "should" { badge("should", fill: rgb("#b0701a")) }
  else if l == "may" { badge("may", fill: white, color: muted, stroke: 0.6pt + muted) }
  else { panic("unknown requirement level: " + l) }
}

#let status-badge(s) = badge(s, fill: white, color: muted, stroke: (paint: muted, thickness: 0.6pt, dash: "dashed"))

// ------------------------------------------------------------- references

// Look an id up in the registry. The first layout pass sees an empty
// registry; from the second pass on a missing id is a hard error, so a
// broken cross-reference fails the build instead of printing a dead link.
#let lookup(id, from: none) = {
  let all = all-reqs()
  let hit = all.find(x => x.id == id)
  if hit == none and all.len() > 0 {
    panic("unknown requirement id `" + id + "`" + if from != none { " (referenced from `" + from + "`)" } else { "" })
  }
  hit
}

#let id-text(id, color) = text(font: mono, size: 0.8em, fill: color, id)

// A clickable reference to a requirement. `#refr("a.b")` prints the id in
// its level colour; `title: true` appends the short title.
#let refr(id, title: false) = context {
  let hit = lookup(id)
  if hit == none { return text(font: mono, size: 0.8em, id) }
  let T = tier-of(hit.tier)
  link(label(id), {
    id-text(id, T.color)
    super(text(font: sans, size: 0.55em, weight: 700, fill: T.color, T.short))
    if title and hit.title != none { [ ] + emph(hit.title) }
  })
}

#let refs(ids) = as-array(ids).map(i => refr(i)).join([, ])

// -------------------------------------------------------------- relations

#let rel-row(label, ids) = if ids.len() > 0 {
  (
    text(font: sans, size: 6.8pt, weight: 700, tracking: 0.06em, fill: muted, upper(label)),
    text(size: 8.6pt, ids.map(i => refr(i)).join([,  ])),
  )
} else { () }

// `checked:` names the gate leg, harness script or test that holds a claim
// to account. Printed verbatim in the code face.
#let check-row(items) = if items.len() > 0 {
  (
    text(font: sans, size: 6.8pt, weight: 700, tracking: 0.06em, fill: muted, upper("checked by")),
    text(font: mono, size: 7.6pt, fill: ink, items.join([ · ])),
  )
} else { () }

// ------------------------------------------------------------ requirement

#let keyword-re = regex("\b(MUST NOT|MUST|SHALL NOT|SHALL|SHOULD NOT|SHOULD|REQUIRED|RECOMMENDED|NOT RECOMMENDED|MAY|OPTIONAL)\b")

#let render-req(id, T, t, title, level, status, relations, body) = {
  block(
    width: 100%,
    breakable: false,
    above: 0.8em,
    below: 0.8em,
    fill: T.tint,
    stroke: (left: 2.4pt + T.color),
    inset: (left: 11pt, right: 11pt, top: 8pt, bottom: 8pt),
    radius: (right: 3pt),
    {
      // header line: tier, id, status, level
      grid(
        columns: (auto, auto, 1fr, auto),
        column-gutter: 7pt,
        align: horizon,
        tier-badge(t),
        text(font: mono, size: 8.6pt, weight: 700, fill: T.color, id),
        if title != none { text(font: sans, size: 8.8pt, weight: 600, fill: ink, title) } else { [] },
        {
          if status != none { status-badge(status); h(4pt) }
          if level != none { level-badge(level) }
        },
      )
      v(4pt)
      {
        show keyword-re: it => text(weight: 700, fill: rgb("#7a1d2b"), it)
        cur-req.update(id)
        body
        cur-req.update(none)
      }
      if relations.len() > 0 {
        v(5pt)
        line(length: 100%, stroke: 0.4pt + T.color.lighten(55%))
        v(3pt)
        grid(columns: (auto, 1fr), column-gutter: 9pt, row-gutter: 3.5pt, align: horizon, ..relations)
      }
    },
  )
}

// A requirement. `id` is the stable tracey id; `level` is the RFC 2119 level
// tracey tracks ("must" / "should" / "may", omit for pure definitions).
#let r(
  id,
  title: none,
  level: none,
  tier: none,
  status: none,
  refines: (),
  depends: (),
  see: (),
  checked: (),
  since: none,
  until: none,
  body,
) = context {
  let t = if tier != none { tier } else { cur-tier.get() }
  let ch = cur-chap.get()
  let T = tier-of(t)
  let refines = as-array(refines)
  let depends = as-array(depends)
  let see = as-array(see)
  let checked = as-array(checked)
  if all-reqs().filter(x => x.id == id).len() > 1 {
    panic("duplicate requirement id `" + id + "`")
  }

  // Cross-level discipline, checked once the registry is complete.
  for p in refines {
    let hit = lookup(p, from: id)
    if hit != none and hit.tier >= t {
      panic("`" + id + "` (L" + str(t) + ") refines `" + p + "` (L" + str(hit.tier) + "): refinement must point to a lower level number")
    }
  }
  for p in depends + see { let _ = lookup(p, from: id) }

  let all = all-reqs()
  let refined-by = all.filter(x => id in x.refines).map(x => x.id)
  let needed-by = all.filter(x => id in x.depends).map(x => x.id)
  let relations = (
    ..rel-row("refines ↑", refines),
    ..rel-row("refined by ↓", refined-by),
    ..rel-row("depends on", depends),
    ..rel-row("needed by", needed-by),
    ..rel-row("see also", see),
    ..check-row(checked),
  )

  [#render-req(id, T, t, title, level, status, relations, body)#label(id)]
  [#metadata((
    id: id, title: title, tier: t, level: level, status: status, chapter: ch,
    refines: refines, depends: depends, see: see, checked: checked,
  ))#reg]
}

#let req = r

// ------------------------------------------------------------ prose helpers

#let dfn(body) = text(style: "italic", weight: 600, body)

// ------------------------------------------------------------- vocabulary

#let term-reg = <spec-term>
#let term-use = <spec-term-use>
#let all-terms() = query(term-reg).map(e => e.value)
#let term-color = tier-of(0).color

#let term-lookup(id) = {
  let all = all-terms()
  let hit = all.find(x => x.id == id)
  if hit == none and all.len() > 0 {
    panic("unknown vocabulary term `" + id + "`: add it to chapters/00-vocabulary.typ")
  }
  hit
}

// Inline link to a vocabulary entry: `#term("page")`, or
// `#term("page", [pages])` to choose the printed words. Each use is recorded so the
// entry can list where it is used.
#let term(id, ..shown) = context {
  let t = term-lookup(id)
  let words = if shown.pos().len() > 0 { shown.pos().first() } else if t == none { id } else { lower(t.name.split(" (").first()) }
  [#metadata((term: id, req: cur-req.get()))#term-use]
  link(label("term." + id), underline(
    stroke: (paint: term-color.lighten(35%), thickness: 0.7pt, dash: "dotted"),
    offset: 1.8pt,
    words,
  ))
}

// A vocabulary entry. `see` are requirements that define the behaviour; `src` names
// the code that confirms a fact in the definition.
#let gl(id, name, qual: none, see: (), src: none, body) = context {
  let see = as-array(see)
  if all-terms().filter(x => x.id == id).len() > 1 {
    panic("duplicate vocabulary term `" + id + "`")
  }
  for p in see { let _ = lookup(p, from: "term." + id) }
  let uses = query(term-use).map(e => e.value).filter(u => u.term == id and u.req != none).map(u => u.req).dedup().sorted()
  let rows = (
    ..rel-row("defined by", see),
    ..rel-row("used in", uses),
    ..if src != none { (
      text(font: sans, size: 6.8pt, weight: 700, tracking: 0.06em, fill: muted, upper("checked in")),
      text(font: mono, size: 7.4pt, fill: muted, src),
    ) } else { () },
  )
  [#block(
    width: 100%,
    breakable: false,
    above: 0.85em,
    below: 0.85em,
    inset: (left: 11pt, y: 1pt),
    stroke: (left: 1.6pt + term-color.lighten(50%)),
    {
      grid(
        columns: (1fr, auto),
        align: (left + horizon, right + horizon),
        {
          text(font: serif, size: 12.4pt, weight: 600, name)
          if qual != none { text(font: sans, size: 7pt, fill: muted, tracking: 0.04em)[ #h(5pt) #upper(qual)] }
        },
        text(font: mono, size: 7.2pt, fill: muted, "term." + id),
      )
      v(1pt)
      body
      if rows.len() > 0 {
        v(3pt)
        grid(columns: (auto, 1fr), column-gutter: 9pt, row-gutter: 3pt, align: top, ..rows)
      }
    },
  )#label("term." + id)]
  [#metadata((id: id, name: name, qual: qual))#term-reg]
}

#let ambiguity-table(rows) = {
  table(
    columns: (auto, 1fr, 1fr, auto),
    stroke: (x, y) => if y > 0 { (top: 0.3pt + hair) } else { none },
    inset: (x: 6pt, y: 6pt),
    align: (left + top, left + top, left + top, left + top),
    table.header(..([Word], [Meanings today], [Use], [Entries]).map(h => text(font: sans, size: 7pt, weight: 700, tracking: 0.06em, fill: muted, upper(h)))),
    ..for (w, m, u, ids) in rows {
      (
        text(font: serif, size: 11pt, weight: 600, w),
        text(size: 9.2pt, m),
        text(size: 9.2pt, u),
        text(size: 8.5pt, ids.map(i => term(i, i)).join(linebreak())),
      )
    },
  )
}


#let callout(label, color, body) = block(
  width: 100%,
  breakable: true,
  above: 1em,
  below: 1em,
  inset: (left: 11pt, y: 2pt),
  stroke: (left: 1.2pt + color),
  {
    text(font: sans, size: 6.8pt, weight: 700, tracking: 0.08em, fill: color, upper(label))
    linebreak()
    set text(size: 9.4pt)
    body
  },
)

#let note(body) = callout("Note", muted, body)
#let example(body) = callout("Example", tier-of(1).color, body)
#let drift(body) = callout("Docs and code disagree", rgb("#7a1d2b"), body)
#let resolved(body) = callout("Resolved", rgb("#2f6b3a"), body)

// Where this backend differs from stock rustc + LLVM. Each note is recorded
// with the requirement (or, outside one, the chapter) it sits in, and
// `#differences()` lists them all at the back. `kind` is one of
// "gap" (LLVM does something we do not yet), "choice" (we differ on
// purpose), "extra" (we do something LLVM does not).
#let llvm-reg = <spec-llvm>
#let llvm-kinds = (
  gap: (label: "Gap", color: rgb("#7a1d2b")),
  choice: (label: "By design", color: rgb("#4a3f9e")),
  extra: (label: "Beyond LLVM", color: rgb("#2f6b3a")),
)
#let llvm-color = rgb("#3f5f8a")
#let llvm(kind: "choice", summary: none, body) = context {
  let K = llvm-kinds.at(kind, default: none)
  if K == none { panic("unknown #llvm kind: " + kind) }
  let at = cur-req.get()
  [#metadata((kind: kind, req: at, chapter: cur-chap.get(), summary: summary, body: body))#llvm-reg]
  block(
    width: 100%,
    breakable: true,
    above: 0.9em,
    below: 0.9em,
    inset: (left: 11pt, y: 2pt),
    stroke: (left: (paint: llvm-color, thickness: 1.2pt, dash: "densely-dashed")),
    {
      text(font: sans, size: 6.8pt, weight: 700, tracking: 0.08em, fill: llvm-color)[COMPARED WITH LLVM]
      h(6pt)
      badge(K.label, fill: white, color: K.color, stroke: 0.6pt + K.color)
      linebreak()
      set text(size: 9.4pt)
      body
    },
  )
}

// -------------------------------------------------------------- chapters

#let chapter(id, tier) = {
  cur-chap.update(id)
  cur-tier.update(tier)
  pagebreak(weak: true)
}

// Level summary for the current chapter: counts, keyword mix, cross-level
// links. Drawn from the registry, so it can never drift from the text.
#let chapter-glance() = context {
  let ch = cur-chap.get()
  let mine = all-reqs().filter(x => x.chapter == ch)
  let n = mine.len()
  if n == 0 { return }
  let count(f) = mine.filter(f).len()
  let up = count(x => x.refines.len() > 0)
  let down = count(x => all-reqs().any(y => x.id in y.refines))
  let cell(num, lab, color: ink) = block(inset: (right: 16pt), {
    text(font: serif, size: 20pt, weight: 600, fill: color, number-type: "lining", str(num))
    linebreak()
    text(font: sans, size: 6.8pt, weight: 700, tracking: 0.07em, fill: muted, upper(lab))
  })
  v(4pt)
  block(
    width: 100%,
    inset: (y: 8pt),
    stroke: (top: 0.5pt + hair, bottom: 0.5pt + hair),
    {
      grid(
        columns: (auto,) * 8,
        cell(n, "requirements"),
        ..for k in (0, 1, 2) {
          let c = count(x => x.tier == k)
          if c > 0 { (cell(c, tier-of(k).short + " " + tier-of(k).name, color: tier-of(k).color),) } else { () }
        },
        cell(count(x => x.level == "must"), "must"),
        cell(count(x => x.level == "should"), "should"),
        ..(if mine.any(x => x.tier > 0) { (cell(up, "trace up"),) } else { () }),
        cell(down, "refined below"),
      )
    },
  )
  v(2pt)
}

// -------------------------------------------------------------- back matter

#let page-of(id) = context {
  let hits = query(label(id))
  if hits.len() == 0 { return [?] }
  link(label(id), numbering("1", ..counter(page).at(hits.first().location())))
}

#let glossary-index() = context {
  let all = all-terms().sorted(key: x => lower(x.name) + "~" + x.id)
  let cols = ()
  columns(2, gutter: 18pt, {
    set text(size: 8.8pt)
    for t in all {
      block(above: 0pt, below: 3.2pt, width: 100%, {
        link(label("term." + t.id), t.name)
        if t.qual != none { text(font: sans, size: 6.8pt, fill: muted)[ #upper(t.qual)] }
        h(1fr)
        text(font: sans, size: 8pt)[#page-of("term." + t.id)]
      })
    }
  })
}

#let req-index() = context {
  let all = all-reqs().sorted(key: x => x.id)
  table(
    columns: (auto, 1fr, auto, auto, auto),
    stroke: (x, y) => if y > 0 { (top: 0.3pt + hair) } else { none },
    inset: (x: 5pt, y: 4.5pt),
    align: (left, left, center, center, right),
    table.header(
      ..([Id], [Title], [Level], [Keyword], [Page]).map(h => text(font: sans, size: 7pt, weight: 700, tracking: 0.06em, fill: muted, upper(h)))
    ),
    ..for x in all {
      (
        refr(x.id),
        text(size: 9pt, if x.title != none { x.title } else { text(fill: muted)[–] }),
        tier-badge(x.tier),
        if x.level != none { level-badge(x.level) } else { [] },
        text(font: sans, size: 8.5pt, page-of(x.id)),
      )
    },
  )
}

// Every L0 requirement with the lower-level requirements that refine it,
// transitively: the cross-level trace in one view.
#let level-map() = context {
  let all = all-reqs()
  let kids(id) = all.filter(x => id in x.refines).sorted(key: x => x.id)
  let tree(id, depth) = {
    for k in kids(id) {
      block(
        above: 0pt,
        below: 3pt,
        inset: (left: 14pt * depth),
        {
          text(font: sans, size: 7pt, fill: muted)[└ ]
          refr(k.id)
          if k.title != none { text(size: 8.8pt)[ #h(2pt) #k.title] }
        },
      )
      tree(k.id, depth + 1)
    }
  }
  for root in all.filter(x => x.tier == 0).sorted(key: x => x.id) {
    block(breakable: false, above: 9pt, below: 3pt, {
      refr(root.id)
      if root.title != none { text(size: 9.4pt, weight: 600)[ #h(2pt) #root.title] }
      v(2pt)
      tree(root.id, 1)
    })
  }
  let orphans = all.filter(x => x.tier > 0 and x.refines.len() == 0)
  if orphans.len() > 0 {
    v(8pt)
    text(font: sans, size: 7pt, weight: 700, tracking: 0.07em, fill: muted)[NOT YET TRACED TO A HIGHER LEVEL]
    v(3pt)
    orphans.sorted(key: x => x.id).map(x => refr(x.id)).join([,  ])
  }
}

#let th(h) = text(font: sans, size: 7pt, weight: 700, tracking: 0.06em, fill: muted, upper(h))

// Every `#llvm[..]` note, grouped by kind, with the requirement it sits in.
#let differences() = context {
  let all = query(llvm-reg)
  for (k, K) in llvm-kinds {
    let mine = all.filter(e => e.value.kind == k)
    if mine.len() == 0 { continue }
    block(above: 12pt, below: 5pt, sticky: true, {
      badge(K.label, fill: white, color: K.color, stroke: 0.6pt + K.color)
      h(6pt)
      text(font: sans, size: 8pt, fill: muted, str(mine.len()) + " notes")
    })
    table(
      columns: (8.2em, 1fr, auto),
      stroke: (x, y) => if y > 0 { (top: 0.3pt + hair) } else { none },
      inset: (x: 5pt, y: 4.5pt),
      align: (left + top, left + top, right + top),
      table.header(th[Where], th[Difference], th[Page]),
      ..for e in mine {
        let v = e.value
        (
          if v.req != none { refr(v.req) } else { text(font: mono, size: 0.8em, fill: muted, v.chapter) },
          text(size: 8.8pt, if v.summary != none { v.summary } else { v.body }),
          text(font: sans, size: 8.5pt, link(e.location(), numbering("1", ..counter(page).at(e.location())))),
        )
      },
    )
  }
}

// Every requirement that names a check: the properties the implementation
// claims and the gate leg, script or test that holds each to account.
#let checks-index() = context {
  let all = all-reqs().filter(x => x.checked.len() > 0).sorted(key: x => x.id)
  table(
    columns: (auto, 1fr, auto),
    stroke: (x, y) => if y > 0 { (top: 0.3pt + hair) } else { none },
    inset: (x: 5pt, y: 4.5pt),
    align: (left + top, left + top, left + top),
    table.header(th[Id], th[Claim], th[Checked by]),
    ..for x in all {
      (
        refr(x.id),
        text(size: 9pt, if x.title != none { x.title } else { [–] }),
        text(font: mono, size: 7.4pt, x.checked.join(linebreak())),
      )
    },
  )
}

// ---------------------------------------------------------------- front matter

#let cover(title: [], subtitle: [], version: "", date: "") = {
  set page(header: none, footer: none, margin: (x: 1.5in, y: 1.4in), fill: paper)
  set par(justify: false, leading: 0.5em)
  v(1fr)
  text(font: sans, size: 8pt, weight: 700, tracking: 0.22em, fill: muted)[SPECIFICATION]
  v(12pt)
  text(font: serif, size: 50pt, weight: 600, tracking: -0.01em, title)
  v(10pt)
  text(font: serif, size: 15pt, style: "italic", fill: muted, subtitle)
  v(34pt)
  // The three levels, drawn as the spec's own legend.
  for k in (0, 1, 2) {
    let T = tier-of(k)
    block(above: 0pt, below: 7pt, width: 100%, {
      grid(
        columns: (36pt, auto, 1fr),
        column-gutter: 10pt,
        align: horizon,
        box(fill: T.color, radius: 2pt, width: 36pt, height: 20pt, align(center + horizon, text(font: sans, size: 8pt, weight: 700, fill: white, tracking: 0.06em, T.short))),
        text(font: sans, size: 10.5pt, weight: 600, fill: T.color, T.name),
        text(size: 9.8pt, fill: muted, (
          [the contract: inputs, outputs, properties],
          [what each stage does, and must do],
          [formats, symbols, knobs, step lists],
        ).at(k)),
      )
    })
    if k < 2 { block(above: 0pt, below: 7pt, inset: (left: 14pt), text(font: sans, size: 9pt, fill: muted)[↓ refined by]) }
  }
  v(1fr)
  grid(
    columns: (1fr, auto),
    align: (left + bottom, right + bottom),
    text(font: sans, size: 8.5pt, fill: muted)[Draft #version · #date#linebreak()Requirements traced with tracey],
    context text(font: serif, size: 28pt, weight: 600, fill: ink, number-type: "lining", str(all-reqs().len())) + text(font: sans, size: 8pt, fill: muted)[ requirements],
  )
  pagebreak()
}

#let how-to-read() = {
  heading(level: 1, numbering: none, outlined: false)[How to read this specification]
  [
    This document describes how rustc_codegen_pliron, a rustc code generator
    with no LLVM in it, actually works: what it promises, how each stage keeps
    the promise, where it differs from stock rustc + LLVM, and which check
    holds each claim to account. Every normative
    statement is a #dfn[requirement]: one atomic, testable sentence with a stable
    id such as #text(font: mono, size: 0.85em)[model.equiv]. Ids never change
    meaning; when the text of a requirement changes in a way that matters, the id
    gets a new version and code that cites the old one is flagged as stale.

    The requirements live on three #dfn[levels]. Reading down the levels goes
    from why to how:
  ]
  for k in (0, 1, 2) {
    let T = tier-of(k)
    block(above: 9pt, below: 9pt, inset: (left: 12pt, y: 1pt), stroke: (left: 2.4pt + T.color), {
      text(font: sans, weight: 700, size: 9pt, fill: T.color)[#T.short #T.name]
      v(1pt)
      (
        [The contract with rustc, the linker and the user: what goes in, what comes out, and the properties that hold whatever the configuration. L0 requirements are the roots of the trace.],
        [Says what each stage of the pipeline does, in terms of L0 words. Behaviour is checked against code and tests.],
        [Pins down exact formats, symbol names, section layouts, knob names and step lists: things a second implementation, or a tool reading our output, must agree on byte for byte.],
      ).at(k)
    })
  }
  [
    A requirement may #dfn[refine] a requirement on a lower level number (an L1
    requirement refines an L0 one), #dfn[depend on] another that must hold first,
    or point to related ones with #dfn[see also]. Each link is printed on both
    ends, and the #emph[Level map] at the back shows the whole trace. Every id
    printed in colour is a link.

    The key words MUST, MUST NOT, SHOULD, SHOULD NOT and MAY are used as in
    RFC 2119 and are set in bold. A requirement marked #status-badge("draft") has
    been checked against the project's documents but not yet against code or a test.
    Where the documents and the code disagree, this specification follows the code
    and says so in a red-ruled note.

    A requirement that names a #dfn[check] (a gate leg, a harness script, a
    `test.sh` stanza) prints it under "checked by"; the #emph[Properties and
    checks] table at the back lists them all. A dashed blue note marked
    #text(font: sans, size: 0.8em, weight: 700, fill: llvm-color)[COMPARED WITH LLVM]
    records a difference from stock rustc + LLVM: a #emph[gap] (LLVM does
    something we do not yet), a #emph[design choice], or something
    #emph[beyond LLVM]. All of them are collected in #emph[Where we differ from LLVM].
  ]
  v(6pt)
  render-req(
    "example.id", tier-of(1), 1, "A sample requirement", "must", none,
    (..rel-row("refines ↑", ("model.equiv",)), ..check-row(("harness/gate.py",))),
    [This is what a requirement looks like. The id and level badge are at the top; the body states one thing a conforming implementation MUST do.],
  )
}

// ---------------------------------------------------------------- template

#let spec(title: "", doc) = {
  set document(title: title, author: "rustc_codegen_pliron")
  set page(
    paper: "us-letter",
    margin: (x: 1.3in, top: 0.95in, bottom: 1.0in),
    fill: paper,
    header: context {
      let n = counter(page).get().first()
      let all-h = query(heading.where(level: 1))
      // none on the cover or on a chapter's opening page
      if n <= 1 or all-h.any(h => counter(page).at(h.location()).first() == n) { return }
      let hs = all-h.filter(h => h.outlined and counter(page).at(h.location()).first() <= n)
      if hs.len() == 0 { return }
      set text(font: sans, size: 6.6pt, fill: muted, tracking: 0.08em)
      upper(hs.last().body)
    },
    footer: context {
      let n = counter(page).get().first()
      if n <= 1 { return }
      set text(font: sans, size: 7.5pt, fill: muted)
      align(center, str(n))
    },
  )
  set text(font: serif, size: 10.4pt, fill: ink, lang: "en", hyphenate: false, number-type: "old-style")
  set par(justify: true, leading: 0.68em, spacing: 0.95em, first-line-indent: 0pt)
  set heading(numbering: "1.1")
  set list(indent: 0.4em, body-indent: 0.6em, spacing: 0.7em)
  set enum(indent: 0.4em, body-indent: 0.6em)
  set table(stroke: none)

  show heading.where(level: 1): it => context {
    let t = cur-tier.get()
    let T = tier-of(t)
    v(0.35in)
    if it.numbering != none {
      text(font: sans, size: 8pt, weight: 700, tracking: 0.2em, fill: T.color)[
        CHAPTER #counter(heading).display("1") #h(6pt) #tier-badge(t) #h(3pt) #text(fill: T.color, upper(T.name))
      ]
      v(6pt)
    }
    text(font: serif, size: 30pt, weight: 600, tracking: -0.005em, it.body)
    v(10pt)
  }
  show heading.where(level: 2): it => {
    v(1.1em, weak: true)
    block(sticky: true, below: 0.6em, {
      text(font: sans, size: 6.5pt, weight: 700, tracking: 0.1em, fill: muted, counter(heading).display(it.numbering))
      h(7pt)
      text(font: serif, size: 15pt, weight: 600, it.body)
    })
  }
  show heading.where(level: 3): it => {
    v(0.8em, weak: true)
    block(sticky: true, below: 0.5em, text(font: sans, size: 9.6pt, weight: 700, tracking: 0.02em, it.body))
  }

  show raw: set text(font: mono, size: 0.82em)
  show raw.where(block: false): it => box(fill: rgb("#efece5"), radius: 2pt, outset: (x: 1.6pt, y: 2.4pt), it)
  show raw.where(block: true): it => block(
    width: 100%,
    fill: rgb("#f3f0ea"),
    radius: 3pt,
    inset: (x: 10pt, y: 8pt),
    above: 0.9em,
    below: 0.9em,
    breakable: false,
    it,
  )
  show link: set text(fill: rgb("#2a4d8f"))
  show strong: set text(weight: 650)

  show outline.entry.where(level: 1): it => {
    v(7pt, weak: true)
    set text(font: sans, size: 9.6pt, weight: 600)
    it
  }
  show outline.entry.where(level: 2): it => {
    set text(size: 9.4pt)
    it
  }

  doc
}
