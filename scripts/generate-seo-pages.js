#!/usr/bin/env node
// Generate static SEO pages for amux.io
const fs = require('fs');
const path = require('path');
const ROOT = path.join(__dirname, '..');

// ── Shared template ──────────────────────────────────────────────
const TODAY = new Date().toISOString().slice(0, 10);

// Auto-generate JSON-LD schema based on category + page data
function autoSchema(page, category, canonical) {
  const url = `https://amux.io${canonical}`;
  const breadcrumbItems = [
    { '@type': 'ListItem', position: 1, name: 'amux', item: 'https://amux.io/' },
  ];
  if (category) {
    const catLabel = category.replace(/-/g, ' ').replace(/\b\w/g, c => c.toUpperCase());
    breadcrumbItems.push({ '@type': 'ListItem', position: 2, name: catLabel, item: `https://amux.io/${category}/` });
    breadcrumbItems.push({ '@type': 'ListItem', position: 3, name: page.title, item: url });
  } else {
    breadcrumbItems.push({ '@type': 'ListItem', position: 2, name: page.title, item: url });
  }
  const breadcrumbList = { '@context': 'https://schema.org', '@type': 'BreadcrumbList', itemListElement: breadcrumbItems };
  const amuxOrg = { '@type': 'Organization', name: 'amux', url: 'https://amux.io/' };

  let main = null;
  if (category === 'blog') {
    main = { '@context': 'https://schema.org', '@type': 'BlogPosting', headline: page.title, description: page.description, url, datePublished: TODAY, dateModified: TODAY, author: amuxOrg, publisher: amuxOrg, mainEntityOfPage: { '@type': 'WebPage', '@id': url } };
  } else if (category === 'guides' || category === 'use-cases' || category === 'solutions') {
    main = { '@context': 'https://schema.org', '@type': 'Article', headline: page.title, description: page.description, url, datePublished: TODAY, dateModified: TODAY, author: amuxOrg };
  } else if (category === 'glossary') {
    main = { '@context': 'https://schema.org', '@type': 'DefinedTerm', name: page.title, description: page.description, url, inDefinedTermSet: { '@type': 'DefinedTermSet', name: 'amux Glossary', url: 'https://amux.io/glossary/' } };
  } else if (category === 'compare') {
    main = { '@context': 'https://schema.org', '@type': 'WebPage', name: page.title, description: page.description, url, about: { '@type': 'SoftwareApplication', name: 'amux', url: 'https://amux.io/', applicationCategory: 'DeveloperApplication' } };
  } else if (category === 'features') {
    main = { '@context': 'https://schema.org', '@type': 'WebPage', name: page.title, description: page.description, url, about: { '@type': 'SoftwareApplication', name: 'amux', url: 'https://amux.io/', featureList: page.title } };
  } else if (category === 'for') {
    main = { '@context': 'https://schema.org', '@type': 'WebPage', name: page.title, description: page.description, url, audience: { '@type': 'Audience', audienceType: page.title } };
  }
  return main ? [breadcrumbList, main] : [breadcrumbList];
}

function template({ title, description, keywords, canonical, content, breadcrumb, schema }) {
  const schemas = schema ? (Array.isArray(schema) ? schema : [schema]) : [];
  const schemaTag = schemas.map(s => `<script type="application/ld+json">${JSON.stringify(s)}</script>`).join('\n  ');
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
  <link rel="alternate" type="application/rss+xml" title="amux Blog" href="https://amux.io/blog/rss.xml">
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
const blogRssItems = []; // collect blog posts for RSS feed

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

  // Use explicit schema if provided, otherwise auto-generate from category
  const schema = page.schema || autoSchema(page, page.category, canonical);

  // Collect blog posts for RSS
  if (page.category === 'blog') {
    blogRssItems.push({ title: page.title, description: page.description, url: `https://amux.io${canonical}` });
  }

  const html = template({
    title: page.title,
    description: page.description,
    keywords: page.keywords,
    canonical,
    content: page.content,
    breadcrumb,
    schema,
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

// ── Blog RSS Feed ─────────────────────────────────────────────────
const rssItems = blogRssItems.map(item => `  <item>
    <title><![CDATA[${item.title}]]></title>
    <description><![CDATA[${item.description}]]></description>
    <link>${item.url}</link>
    <guid isPermaLink="true">${item.url}</guid>
    <pubDate>${new Date().toUTCString()}</pubDate>
  </item>`).join('\n');
const rssFeed = `<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xmlns:atom="http://www.w3.org/2005/Atom">
  <channel>
    <title>amux Blog — AI Agent Orchestration &amp; Parallel Coding</title>
    <link>https://amux.io/blog/</link>
    <description>Insights on parallel AI coding, agent orchestration, and developer productivity with amux.</description>
    <language>en-us</language>
    <lastBuildDate>${new Date().toUTCString()}</lastBuildDate>
    <atom:link href="https://amux.io/blog/rss.xml" rel="self" type="application/rss+xml"/>
    <image><url>https://amux.io/og-image.png</url><title>amux</title><link>https://amux.io/</link></image>
${rssItems}
  </channel>
</rss>`;
const blogDir = path.join(ROOT, 'blog');
fs.mkdirSync(blogDir, { recursive: true });
fs.writeFileSync(path.join(blogDir, 'rss.xml'), rssFeed);

// ── llms.txt (AI crawler discovery file) ─────────────────────────
const guideLinks = [...guides, ...moreGuides].slice(0, 10).map(g => `- [${g.title}](https://amux.io/guides/${g.slug}/): ${g.description}`).join('\n');
const useCaseLinks = useCases.map(u => `- [${u.title}](https://amux.io/use-cases/${u.slug}/)`).join('\n');
const compareLinks = comparisons.map(c => `- [${c.title}](https://amux.io/compare/${c.slug}/)`).join('\n');
const llmsTxt = `# amux

> Open-source Claude Code agent multiplexer — run dozens of parallel AI coding agents unattended via tmux, with a web dashboard, kanban board, self-healing watchdog, and agent-to-agent REST API orchestration.

amux (Agent Multiplexer) is a Python + tmux tool that runs multiple Claude Code sessions in parallel. It provides a mobile-friendly PWA dashboard, atomic kanban board (SQLite), session peek/send, iCal sync, and a REST API for agent orchestration. MIT licensed. No cloud dependency.

## Quick Start

\`\`\`bash
git clone https://github.com/mixpeek/amux && cd amux
python3 amux-server.py  # opens dashboard at https://localhost:8822
\`\`\`

## Documentation

- [FAQ](https://amux.io/faq/): What is amux, how does it work, pricing, requirements
- [Getting Started](https://amux.io/guides/getting-started-with-amux/): Installation, first session, dashboard tour
- [Parallel Agents Guide](https://amux.io/guides/running-parallel-claude-agents/): Run multiple agents simultaneously
- [Agent Orchestration](https://amux.io/guides/agent-orchestration-patterns/): Multi-agent coordination patterns
- [Self-Healing Agents](https://amux.io/guides/self-healing-agent-watchdog/): Watchdog auto-restart setup
${guideLinks}

## Use Cases

${useCaseLinks}

## Comparisons

${compareLinks}

## Key Features

- Parallel Claude Code sessions via tmux (unlimited agents)
- Self-healing watchdog (auto-restarts stalled sessions)
- Web dashboard (PWA, mobile-friendly, dark/light mode)
- Kanban board with atomic task claiming (SQLite)
- Agent-to-agent REST API (POST /api/sessions/NAME/send)
- iCal feed for board items with due dates
- Single-file architecture (amux-server.py — no Docker, no config)
- MIT License — free, open-source, self-hosted

## Source

- GitHub: https://github.com/mixpeek/amux
- Homepage: https://amux.io
- Blog: https://amux.io/blog/
- RSS: https://amux.io/blog/rss.xml
- Sitemap: https://amux.io/sitemap.xml
`;
fs.writeFileSync(path.join(ROOT, 'llms.txt'), llmsTxt);

console.log(`Generated ${generated} pages + ${categories.length} index pages + sitemap.xml + robots.txt + rss.xml + llms.txt`);
console.log(`Total URLs in sitemap: ${sitemapEntries.length}`);
