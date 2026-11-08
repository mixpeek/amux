// Code in the file viewer is readable in light mode too (Ethan, 2026-11-08:
// "Fix how code is rendered"). The code body stays dark in light mode, and
// plain tokens (identifiers, numbers, operators) took the PAGE text colour,
// dark grey on #1c2128, while keywords and strings stood out. Renders real
// Python through _fileHighlightHTML into #file-body and measures every token.
import { test, expect } from './fixtures';

const PY = [
  'import sqlite3,os,re,glob',
  "H=os.path.expanduser('~')",
  'def norm(n):',
  "    d=re.sub(r'\\D','',n or ''); return '1'+d if len(d)==10 else d",
  'names={}',
  'for db in glob.glob(H+"/Library"):',
  '    c=sqlite3.connect(db)',
  'rank(30);rank(90);rank(365)',
].join('\n');

function lum(rgb: number[]) {
  const c = rgb.map((v) => { v /= 255; return v <= 0.03928 ? v / 12.92 : Math.pow((v + 0.055) / 1.055, 2.4); });
  return 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
}
const ratio = (a: number[], b: number[]) => { const [x, y] = [lum(a), lum(b)].sort((p, q) => q - p); return (x + 0.05) / (y + 0.05); };

for (const theme of ['light', 'dark']) {
  test(`plain code tokens are readable on the code body (${theme})`, async ({ page }) => {
    await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
    await page.goto('/');
    await page.waitForFunction(() => typeof (window as any)._fileHighlightHTML === 'function' && !!(window as any).hljs);
    const got = await page.evaluate(([src, light]) => {
      (window as any)._applyTheme(light === 'light');   // the dashboard's own switch, not just the class
      const ov = document.getElementById('file-overlay')!;
      ov.classList.add('active');
      const body = document.getElementById('file-body')!;
      body.className = 'file-overlay-body file-code';
      body.innerHTML = (window as any)._fileHighlightHTML({ name: 'eng.py', content: src });
      // rgb(r, g, b) is 0-255; color-mix() computes to color(srgb r g b), 0-1.
      const rgb = (s: string) => {
        const n = (s.replace(/^color\(srgb/, '').match(/[\d.]+/g) || []).slice(0, 3).map(Number);
        return s.startsWith('color(') ? n.map((v) => v * 255) : n;
      };
      let el: HTMLElement | null = body, bg = 'rgba(0, 0, 0, 0)';
      while (el && /rgba\(0, 0, 0, 0\)|transparent/.test(bg)) { bg = getComputedStyle(el).backgroundColor; el = el.parentElement; }
      const code = body.querySelector('code.hljs') as HTMLElement;
      // Every text run: a bare text node takes the code's colour, a span its own.
      const runs: { text: string; color: number[] }[] = [];
      const walk = (n: Node) => n.childNodes.forEach((k) => {
        if (k.nodeType === 3 && (k.textContent || '').trim()) runs.push({ text: k.textContent!.trim().slice(0, 20), color: rgb(getComputedStyle(n as HTMLElement).color) });
        else if (k.nodeType === 1) walk(k);
      });
      walk(code);
      return { bg: rgb(bg), runs };
    }, [PY, theme]);
    expect(got.runs.length).toBeGreaterThan(20);
    const worst = got.runs.map((r) => ({ ...r, ratio: ratio(r.color, got.bg) })).sort((a, b) => a.ratio - b.ratio)[0];
    // Punctuation may recede, but nothing may drop under 3:1, and plain text
    // (the identifiers that were unreadable) must clear WCAG AA, 4.5:1.
    expect(worst.ratio, JSON.stringify(worst)).toBeGreaterThan(3);
    const plain = got.runs.filter((r) => /^[A-Za-z_]/.test(r.text)).map((r) => ratio(r.color, got.bg));
    expect(Math.min(...plain)).toBeGreaterThan(4.5);
  });
}
