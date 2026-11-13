#!/usr/bin/env python3
"""The JavaScript dana.sh evaluates, as amux browser eval bodies.

Built here rather than inline in dana.sh: macOS bash 3.2 mis-parses the
parentheses of a nested $(...) inside double quotes, which garbled every tap
("SyntaxError: missing ) after argument list", the first Dana run).
"""
import json
import sys

LOOK = r"""(() => { const out = []; const W = innerWidth, H = innerHeight;
  const tw = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT);
  let n, last = null;
  while ((n = tw.nextNode())) { const t = n.textContent.replace(/\s+/g, ' ').trim(); if (!t) continue;
    const el = n.parentElement; if (!el) continue; const r = el.getBoundingClientRect();
    if (r.width < 1 || r.height < 1 || r.bottom < 0 || r.right < 0 || r.top > H || r.left > W) continue;
    const x = Math.min(W - 1, Math.max(0, r.left + Math.min(r.width, 20) / 2)), y = Math.min(H - 1, Math.max(0, r.top + Math.min(r.height, 20) / 2));
    const top = document.elementFromPoint(x, y); if (!top || !(top === el || el.contains(top) || top.contains(el))) continue;
    if (el !== last) out.push(t); else out[out.length - 1] += ' ' + t; last = el; }
  return out.join('\n').slice(0, 6000); })()"""

TAP = r"""(() => { const want = %s;
  // Only what a person could tap: on screen and on top at its centre (not an
  // element hidden under an overlay, which a finger cannot reach).
  const onTop = e => { const r = e.getBoundingClientRect(); if (r.width < 1 || r.height < 1 || r.bottom < 0 || r.top > innerHeight) return false;
    const t = document.elementFromPoint(Math.min(innerWidth - 1, r.left + r.width / 2), Math.min(innerHeight - 1, Math.max(0, r.top + r.height / 2)));
    return !!t && (t === e || e.contains(t) || t.contains(e)); };
  const els = [...document.querySelectorAll('button, a, [role=button], [role=listitem], label, summary, input[type=radio], input[type=checkbox]')]
    .filter(onTop);
  const txt = e => (e.innerText || e.value || e.getAttribute('aria-label') || '').trim().toLowerCase();
  const el = els.find(e => txt(e) === want) || els.find(e => txt(e).startsWith(want)) || els.find(e => txt(e).includes(want));
  if (!el) return 'NOT FOUND: nothing on screen says ' + JSON.stringify(want);
  el.scrollIntoView({block: 'center'}); el.click(); return 'tapped: ' + txt(el).slice(0, 80); })()"""

TYPE = r"""(() => { const a = document.activeElement;
  const box = (a && /TEXTAREA|INPUT/.test(a.tagName) && a.offsetParent) ? a
    : [...document.querySelectorAll('textarea, input[type=text], input:not([type])')].find(e => e.offsetParent !== null);
  if (!box) return 'NOT FOUND: no text box on screen';
  box.focus(); box.value = %s; box.dispatchEvent(new Event('input', {bubbles: true}));
  return 'typed into: ' + (box.placeholder || box.getAttribute('aria-label') || box.tagName); })()"""

SCROLL = r"""(() => { const dir = %s;
  // The scrollable area under the middle of the screen: what a thumb would drag.
  let el = document.elementFromPoint(innerWidth / 2, innerHeight / 2);
  while (el && el !== document.body) { const s = getComputedStyle(el);
    if (/(auto|scroll)/.test(s.overflowY) && el.scrollHeight > el.clientHeight + 4) break; el = el.parentElement; }
  if (!el || el === document.body) el = document.scrollingElement;
  el.scrollBy(0, (dir === 'up' ? -1 : 1) * el.clientHeight * 0.8); return 'scrolled ' + dir; })()"""

kind = sys.argv[1]
arg = sys.argv[2] if len(sys.argv) > 2 else ""
script = {"look": LOOK, "tap": TAP % json.dumps(arg.strip().lower()), "type": TYPE % json.dumps(arg),
          "scroll": SCROLL % json.dumps(arg or "down")}[kind]
print(json.dumps({"action": "eval", "script": script}))
