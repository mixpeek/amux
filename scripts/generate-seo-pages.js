#!/usr/bin/env node
// Generate static SEO pages for amux.io
const fs = require('fs');
const path = require('path');
const ROOT = path.join(__dirname, '..');

// ── Shared template ──────────────────────────────────────────────
function template({ title, description, keywords, canonical, content, breadcrumb, schema }) {
  const schemaTag = schema ? `<script type="application/ld+json">${JSON.stringify(schema)}</script>` : '';
  return `<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <meta name="viewport" content="width=device-width, initial-scale=1.0">
  <title>${esc(title)} — amux</title>
  <link rel="icon" href="/icon.svg" type="image/svg+xml">
  <meta name="description" content="${esc(description)}">
  <meta name="keywords" content="${esc(keywords)}">
  <link rel="canonical" href="https://amux.io${canonical}">
  <meta property="og:title" content="${esc(title)} — amux">
  <meta property="og:description" content="${esc(description)}">
  <meta property="og:url" content="https://amux.io${canonical}">
  <meta property="og:image" content="https://amux.io/og-image.png">
  <meta property="og:type" content="article">
  <meta name="twitter:card" content="summary_large_image">
  <link rel="preconnect" href="https://fonts.googleapis.com">
  <link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
  <link href="https://fonts.googleapis.com/css2?family=Geist:wght@400;500;600;700&family=Geist+Mono:wght@400;500&display=swap" rel="stylesheet">
  ${schemaTag}
  <style>
    *, *::before, *::after { box-sizing: border-box; margin: 0; padding: 0; }
    :root { --bg:#0a0a0a; --fg:#ededed; --muted:#888; --border:#222; --code-bg:#111; --code-border:#1e1e1e; --accent:#4ade80; --btn-bg:#1a1a1a; --btn-hover:#242424; --card-bg:#111; }
    @media (prefers-color-scheme: light) { :root { --bg:#fafafa; --fg:#111; --muted:#666; --border:#e5e5e5; --code-bg:#f4f4f4; --code-border:#e0e0e0; --accent:#16a34a; --btn-bg:#f0f0f0; --btn-hover:#e5e5e5; --card-bg:#fff; } }
    html, body { background:var(--bg); color:var(--fg); font-family:'Geist',ui-sans-serif,system-ui,-apple-system,sans-serif; font-size:15px; line-height:1.7; -webkit-font-smoothing:antialiased; }
    a { color:var(--accent); text-decoration:none; } a:hover { text-decoration:underline; }
    code { font-family:'Geist Mono',ui-monospace,monospace; font-size:0.85em; background:var(--code-bg); border:1px solid var(--code-border); padding:0.15em 0.4em; border-radius:4px; }
    pre { background:var(--code-bg); border:1px solid var(--code-border); border-radius:8px; padding:1rem 1.2rem; overflow-x:auto; margin:1.2rem 0; font-family:'Geist Mono',ui-monospace,monospace; font-size:0.8rem; line-height:1.8; }
    pre code { background:none; border:none; padding:0; }
    .site-header { max-width:42rem; margin:0 auto; padding:1.5rem 1.5rem 0; display:flex; align-items:center; justify-content:space-between; }
    .site-header a.logo { display:flex; align-items:center; gap:0.5rem; color:var(--fg); font-weight:600; font-size:1rem; text-decoration:none; }
    .site-header a.logo img { width:24px; height:24px; border-radius:5px; }
    .site-header nav { display:flex; gap:1.2rem; font-size:0.8rem; }
    .site-header nav a { color:var(--muted); } .site-header nav a:hover { color:var(--fg); }
    main { max-width:42rem; margin:0 auto; padding:2rem 1.5rem 4rem; }
    .breadcrumb { font-size:0.75rem; color:var(--muted); margin-bottom:1.5rem; }
    .breadcrumb a { color:var(--muted); } .breadcrumb a:hover { color:var(--accent); }
    h1 { font-size:1.8rem; font-weight:700; letter-spacing:-0.03em; line-height:1.25; margin-bottom:0.6rem; }
    .subtitle { font-size:1rem; color:var(--muted); margin-bottom:2rem; line-height:1.6; }
    h2 { font-size:1.2rem; font-weight:600; margin:2.2rem 0 0.8rem; letter-spacing:-0.02em; }
    h3 { font-size:1rem; font-weight:600; margin:1.5rem 0 0.6rem; }
    p { margin-bottom:1rem; }
    ul, ol { margin:0.5rem 0 1rem 1.5rem; } li { margin-bottom:0.4rem; }
    table { width:100%; border-collapse:collapse; margin:1.2rem 0; font-size:0.85rem; }
    th { text-align:left; font-weight:600; padding:0.6rem 0.8rem; border-bottom:2px solid var(--border); }
    td { padding:0.5rem 0.8rem; border-bottom:1px solid var(--border); }
    tr:hover td { background:rgba(74,222,128,0.04); }
    .cta-box { background:var(--card-bg); border:1px solid var(--border); border-radius:8px; padding:1.5rem; margin:2rem 0; }
    .cta-box h3 { margin-top:0; }
    .btn { display:inline-flex; align-items:center; gap:0.4rem; padding:0.45rem 1rem; border-radius:6px; font-size:0.85rem; font-weight:500; text-decoration:none; border:1px solid var(--border); background:var(--btn-bg); color:var(--fg); transition:background 0.12s; }
    .btn:hover { background:var(--btn-hover); text-decoration:none; }
    .btn-primary { background:var(--accent); border-color:var(--accent); color:#0a0a0a; }
    .btn-primary:hover { opacity:0.88; background:var(--accent); }
    .tag { display:inline-block; font-size:0.7rem; font-weight:500; padding:0.15rem 0.5rem; border-radius:3px; background:var(--code-bg); border:1px solid var(--code-border); color:var(--muted); margin-right:0.3rem; }
    footer { max-width:42rem; margin:0 auto; padding:0 1.5rem 3rem; border-top:1px solid var(--border); padding-top:1.5rem; display:flex; flex-wrap:wrap; gap:0.8rem; font-size:0.75rem; color:var(--muted); }
    footer a { color:var(--muted); } footer a:hover { color:var(--fg); }
    footer .sep { opacity:0.3; }
    @media (max-width:600px) { h1 { font-size:1.4rem; } main { padding:1.5rem 1rem 3rem; } .site-header { padding:1rem 1rem 0; } }
  </style>
</head>
<body>
  <header class="site-header">
    <a class="logo" href="/"><img src="/icon.svg" alt="amux">amux</a>
    <nav>
      <a href="/compare/">Compare</a>
      <a href="/use-cases/">Use Cases</a>
      <a href="/guides/">Guides</a>
      <a href="/glossary/">Glossary</a>
      <a href="/faq/">FAQ</a>
      <a href="https://github.com/mixpeek/amux" target="_blank" rel="noopener">GitHub</a>
    </nav>
  </header>
  <main>
    <div class="breadcrumb">${breadcrumb}</div>
    ${content}
  </main>
  <footer>
    <a href="/">Home</a><span class="sep">·</span>
    <a href="/compare/">Comparisons</a><span class="sep">·</span>
    <a href="/use-cases/">Use Cases</a><span class="sep">·</span>
    <a href="/guides/">Guides</a><span class="sep">·</span>
    <a href="/glossary/">Glossary</a><span class="sep">·</span>
    <a href="/faq/">FAQ</a><span class="sep">·</span>
    <a href="https://github.com/mixpeek/amux" target="_blank" rel="noopener">GitHub</a><span class="sep">·</span>
    <span>MIT License</span>
  </footer>
</body>
</html>`;
}

function esc(s) { return String(s).replace(/&/g,'&amp;').replace(/"/g,'&quot;').replace(/</g,'&lt;').replace(/>/g,'&gt;'); }

// ── Page data ────────────────────────────────────────────────────
const pages = [];

// Load page definitions from separate data file
const {
  comparisons, useCases, guides, glossary, solutions, blog, faqPage, features,
  geoPages, moreBlog, moreCompare, moreGuides, moreGlossary, moreSolutions
} = require('./seo-data.js');

// Register all pages
comparisons.forEach(p => pages.push({ ...p, category: 'compare' }));
moreCompare.forEach(p => pages.push({ ...p, category: 'compare' }));
useCases.forEach(p => pages.push({ ...p, category: 'use-cases' }));
guides.forEach(p => pages.push({ ...p, category: 'guides' }));
moreGuides.forEach(p => pages.push({ ...p, category: 'guides' }));
glossary.forEach(p => pages.push({ ...p, category: 'glossary' }));
moreGlossary.forEach(p => pages.push({ ...p, category: 'glossary' }));
solutions.forEach(p => pages.push({ ...p, category: 'solutions' }));
moreSolutions.forEach(p => pages.push({ ...p, category: 'solutions' }));
blog.forEach(p => pages.push({ ...p, category: 'blog' }));
moreBlog.forEach(p => pages.push({ ...p, category: 'blog' }));
features.forEach(p => pages.push({ ...p, category: 'features' }));
geoPages.forEach(p => pages.push({ ...p, category: 'for' }));
pages.push({ ...faqPage, category: '' });

// ── Generate ─────────────────────────────────────────────────────
let generated = 0;
const sitemapEntries = ['https://amux.io/'];

for (const page of pages) {
  const slug = page.slug;
  const dir = page.category ? path.join(ROOT, page.category, slug) : path.join(ROOT, slug);
  const canonical = page.category ? `/${page.category}/${slug}/` : `/${slug}/`;
  const breadcrumbParts = [`<a href="/">amux</a>`];
  if (page.category) {
    const catLabel = page.category.replace(/-/g, ' ').replace(/\b\w/g, c => c.toUpperCase());
    breadcrumbParts.push(`<a href="/${page.category}/">${catLabel}</a>`);
  }
  breadcrumbParts.push(`<span>${esc(page.title)}</span>`);
  const breadcrumb = breadcrumbParts.join(' <span style="opacity:0.4;margin:0 0.3rem">›</span> ');

  const html = template({
    title: page.title,
    description: page.description,
    keywords: page.keywords,
    canonical,
    content: page.content,
    breadcrumb,
    schema: page.schema || null,
  });

  fs.mkdirSync(dir, { recursive: true });
  fs.writeFileSync(path.join(dir, 'index.html'), html);
  sitemapEntries.push(`https://amux.io${canonical}`);
  generated++;
}

// ── Index pages for categories ───────────────────────────────────
const categories = [
  { slug: 'compare', title: 'Comparisons', desc: 'See how amux compares to other AI coding tools and frameworks.', items: [...comparisons, ...moreCompare] },
  { slug: 'use-cases', title: 'Use Cases', desc: 'Real workflows where parallel AI agents save hours of development time.', items: useCases },
  { slug: 'guides', title: 'Guides', desc: 'Step-by-step guides for getting the most out of amux.', items: [...guides, ...moreGuides] },
  { slug: 'glossary', title: 'Glossary', desc: 'Key concepts in AI agent orchestration and parallel development.', items: [...glossary, ...moreGlossary] },
  { slug: 'solutions', title: 'Solutions', desc: 'How amux fits different teams, roles, and project types.', items: [...solutions, ...moreSolutions] },
  { slug: 'blog', title: 'Blog', desc: 'Insights on parallel AI coding, agent orchestration, and developer productivity.', items: [...blog, ...moreBlog] },
  { slug: 'for', title: 'For Developers', desc: 'amux for every language, stack, and city.', items: geoPages },
];

for (const cat of categories) {
  const links = cat.items.map(p =>
    `<li style="margin-bottom:1rem"><a href="/${cat.slug}/${p.slug}/" style="font-weight:500">${esc(p.title)}</a><br><span style="color:var(--muted);font-size:0.85rem">${esc(p.description)}</span></li>`
  ).join('\n');
  const indexContent = `<h1>${cat.title}</h1><p class="subtitle">${cat.desc}</p><ul style="list-style:none;margin-left:0">${links}</ul>`;
  const indexHtml = template({
    title: cat.title,
    description: cat.desc,
    keywords: `amux, ${cat.title.toLowerCase()}, claude code, AI coding`,
    canonical: `/${cat.slug}/`,
    content: indexContent,
    breadcrumb: `<a href="/">amux</a> <span style="opacity:0.4;margin:0 0.3rem">›</span> <span>${cat.title}</span>`,
  });
  const dir = path.join(ROOT, cat.slug);
  fs.mkdirSync(dir, { recursive: true });
  fs.writeFileSync(path.join(dir, 'index.html'), indexHtml);
  sitemapEntries.push(`https://amux.io/${cat.slug}/`);
  generated++;
}

// ── Sitemap ──────────────────────────────────────────────────────
const sitemap = `<?xml version="1.0" encoding="UTF-8"?>
<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
${sitemapEntries.map(url => `  <url><loc>${url}</loc><changefreq>weekly</changefreq></url>`).join('\n')}
</urlset>`;
fs.writeFileSync(path.join(ROOT, 'sitemap.xml'), sitemap);

// ── robots.txt ───────────────────────────────────────────────────
fs.writeFileSync(path.join(ROOT, 'robots.txt'), `User-agent: *\nAllow: /\nSitemap: https://amux.io/sitemap.xml\n`);

console.log(`Generated ${generated} pages + ${categories.length} index pages + sitemap.xml + robots.txt`);
console.log(`Total URLs in sitemap: ${sitemapEntries.length}`);
