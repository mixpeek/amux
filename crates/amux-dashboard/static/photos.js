// Photos: a media library browsed, searched and rediscovered (Ethan,
// 2026-11-08). Google Photos in shape, Apple Photos in finish: media is the
// interface, chrome stays out of the way.
//
// Where media comes from is a SOURCE (list / item / similar / collections /
// suggest). The library a person connects is their own Mixpeek namespace
// (api/photos.rs holds the connection; the key never reaches this page).
// Ingestion and search against it are not built yet, so every view runs on
// SampleSource: 10,000 generated photos and videos, drawn here as SVG, so the
// tab works offline and at the size the real one must handle. Replacing it
// means writing a MixpeekSource with the same five methods.
//
// Mounted by app.js (_photosEnter) into #photos-view. One instance per page;
// mount() again after leaving restores the view, the search and the scroll.
(function () {
  'use strict';
  const API = (typeof window.API === 'string') ? window.API : '';
  const headers = (extra) => Object.assign({}, typeof window._authHeaders === 'function' ? window._authHeaders() : {}, extra || {});
  const esc = (s) => String(s == null ? '' : s).replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
  const toast = (m) => { if (typeof window.showToast === 'function') window.showToast(m); };
  const store = {
    get(k, d) { try { const v = JSON.parse(localStorage.getItem('amux_photos_' + k) || 'null'); return v == null ? d : v; } catch (e) { console.warn('photos: unreadable ' + k, e); return d; } },
    set(k, v) { try { localStorage.setItem('amux_photos_' + k, JSON.stringify(v)); } catch (e) { console.warn('photos: not saved ' + k, e); } },
  };

  // ── Sample library ──────────────────────────────────────────────────────
  function rng(seed) { let s = seed >>> 0 || 1; return () => { s ^= s << 13; s >>>= 0; s ^= s >> 17; s ^= s << 5; s >>>= 0; return s / 4294967296; }; }
  const THEMES = [
    { id: 'sunset', label: 'Sunsets', kind: 'landscape', words: ['sunset', 'sunrise', 'golden hour', 'dusk', 'sky', 'orange'], pal: ['#ff9a5a', '#e2567a', '#5b2a6e', '#f6c26b'] },
    { id: 'beach', label: 'Beaches', kind: 'landscape', words: ['beach', 'ocean', 'sea', 'sand', 'waves', 'coast', 'summer'], pal: ['#7ec8f0', '#2a8fc4', '#f2dcaa', '#ffffff'] },
    { id: 'mountains', label: 'Mountains', kind: 'landscape', words: ['mountain', 'mountains', 'peak', 'hiking', 'alpine', 'summit'], pal: ['#a9c8ea', '#4f6d8a', '#2f4357', '#f4f7fb'] },
    { id: 'forest', label: 'Forests', kind: 'landscape', words: ['forest', 'trees', 'woods', 'hiking', 'trail', 'green'], pal: ['#cfe8c2', '#3f7a45', '#24502c', '#86b16f'] },
    { id: 'city', label: 'Cities', kind: 'place', words: ['city', 'skyline', 'street', 'buildings', 'urban', 'night'], pal: ['#2b3a67', '#e8a87c', '#1b2140', '#ffd27a'] },
    { id: 'snow', label: 'Snow', kind: 'landscape', words: ['snow', 'winter', 'ski', 'cold', 'ice'], pal: ['#dfeaf5', '#9db6cf', '#ffffff', '#6f8aa6'] },
    { id: 'lake', label: 'Lakes', kind: 'landscape', words: ['lake', 'water', 'reflection', 'calm', 'shore'], pal: ['#b7d7ee', '#3b7da8', '#5c7a5a', '#e9f3fb'] },
    { id: 'flowers', label: 'Flowers', kind: 'subject', words: ['flower', 'flowers', 'garden', 'bloom', 'spring', 'macro'], pal: ['#bfe3a5', '#e2507a', '#f6c34c', '#9b5de5'] },
    { id: 'desert', label: 'Deserts', kind: 'landscape', words: ['desert', 'dunes', 'sand', 'canyon', 'dry'], pal: ['#f6d7a7', '#d98f4e', '#a85a32', '#7fb3d5'] },
    { id: 'waterfall', label: 'Waterfalls', kind: 'landscape', words: ['waterfall', 'waterfalls', 'falls', 'river', 'cascade'], pal: ['#41594a', '#e7f2f5', '#27382e', '#8fb3a0'] },
    { id: 'dog', label: 'Dogs', kind: 'subject', words: ['dog', 'dogs', 'puppy', 'pet', 'golden retriever'], pal: ['#f3e2c7', '#d6a25f', '#8a5a2b', '#6fa86a'] },
    { id: 'food', label: 'Food', kind: 'subject', words: ['food', 'dinner', 'lunch', 'meal', 'restaurant', 'breakfast'], pal: ['#f1e6d8', '#ffffff', '#d9483b', '#7fb069'] },
    { id: 'night', label: 'Night skies', kind: 'landscape', words: ['night', 'stars', 'moon', 'astro', 'milky way'], pal: ['#0c1530', '#1d2b5a', '#f5f1d0', '#3a4f8a'] },
    { id: 'autumn', label: 'Autumn', kind: 'landscape', words: ['autumn', 'fall', 'leaves', 'foliage', 'october'], pal: ['#f3d9a4', '#d9622b', '#a3301c', '#e8a93a'] },
  ];
  const THEME = Object.fromEntries(THEMES.map((t) => [t.id, t]));
  const PLACES = [
    { name: 'Maui, Hawaii', lat: 20.7984, lon: -156.3319, themes: ['beach', 'sunset', 'waterfall', 'food'] },
    { name: 'Santa Cruz, California', lat: 36.9741, lon: -122.0308, themes: ['beach', 'dog', 'sunset'] },
    { name: 'Lake Tahoe, California', lat: 39.0968, lon: -120.0324, themes: ['lake', 'snow', 'mountains', 'forest'] },
    { name: 'Yosemite, California', lat: 37.8651, lon: -119.5383, themes: ['mountains', 'waterfall', 'forest', 'night'] },
    { name: 'San Francisco, California', lat: 37.7749, lon: -122.4194, themes: ['city', 'food', 'dog', 'sunset'] },
    { name: 'New York, New York', lat: 40.7128, lon: -74.006, themes: ['city', 'food', 'snow', 'autumn'] },
    { name: 'Tokyo, Japan', lat: 35.6762, lon: 139.6503, themes: ['city', 'food', 'flowers', 'night'] },
    { name: 'Kyoto, Japan', lat: 35.0116, lon: 135.7681, themes: ['autumn', 'flowers', 'forest', 'food'] },
    { name: 'Reykjavik, Iceland', lat: 64.1466, lon: -21.9426, themes: ['waterfall', 'night', 'snow', 'mountains'] },
    { name: 'Banff, Canada', lat: 51.1784, lon: -115.5708, themes: ['lake', 'mountains', 'snow', 'forest'] },
    { name: 'Lisbon, Portugal', lat: 38.7223, lon: -9.1393, themes: ['city', 'sunset', 'food', 'beach'] },
    { name: 'Joshua Tree, California', lat: 33.8734, lon: -115.901, themes: ['desert', 'night', 'sunset'] },
    { name: 'Home', lat: 37.4419, lon: -122.143, themes: ['dog', 'food', 'flowers', 'autumn'] },
  ];
  const CAMERAS = [
    { make: 'Apple', model: 'iPhone 15 Pro', lens: 'iPhone 15 Pro back triple camera 6.86mm f/1.78', f: [6.86, 2.22, 9.0], ap: [1.78, 2.2, 2.8], profile: 'Display P3' },
    { make: 'Sony', model: 'ILCE-7M4', lens: 'FE 24-70mm F2.8 GM II', f: [24, 35, 50, 70], ap: [2.8, 4, 5.6, 8], profile: 'sRGB IEC61966-2.1' },
    { make: 'FUJIFILM', model: 'X100VI', lens: 'Fujinon 23mm F2', f: [23], ap: [2, 2.8, 4, 5.6], profile: 'sRGB IEC61966-2.1' },
    { make: 'DJI', model: 'Mini 4 Pro', lens: '24mm f/1.7', f: [6.7], ap: [1.7], profile: 'sRGB IEC61966-2.1' },
  ];
  const SHAPES = [[4, 3], [3, 4], [3, 2], [2, 3], [16, 9], [1, 1], [4, 5]];

  function makeLibrary(n) {
    const r = rng(20261108);
    const items = [];
    let t = Date.now() - 1000 * 60 * 60 * 6;
    let id = 0;
    while (items.length < n) {
      // An outing: one place, one or two days, a handful of themes.
      const place = PLACES[Math.floor(r() * PLACES.length)];
      const themes = place.themes.slice().sort(() => r() - 0.5).slice(0, 1 + Math.floor(r() * 3));
      const count = Math.min(n - items.length, 3 + Math.floor(r() * 26));
      const span = (1 + Math.floor(r() * 2)) * 86400000;
      const cam = CAMERAS[Math.floor(r() * CAMERAS.length)];
      for (let i = 0; i < count; i++) {
        const theme = themes[Math.floor(r() * themes.length)];
        const video = r() < 0.12;
        const [aw, ah] = video ? (r() < 0.7 ? [16, 9] : [9, 16]) : SHAPES[Math.floor(r() * SHAPES.length)];
        const taken = t - Math.floor(r() * span);
        const seed = Math.floor(r() * 1e9);
        const item = {
          id: 's' + (id++), type: video ? 'video' : 'photo', taken, aw, ah, theme, place: place.name,
          lat: place.lat + (r() - 0.5) * 0.08, lon: place.lon + (r() - 0.5) * 0.08, seed, cam,
          duration: video ? 4 + Math.floor(r() * 170) : 0,
        };
        item.tags = [...THEME[theme].words.slice(0, 3), ...(video ? ['video'] : ['photo'])];
        items.push(item);
      }
      t -= span + Math.floor(r() * 9 * 86400000);
    }
    items.sort((a, b) => b.taken - a.taken);
    items.forEach((it, i) => { it.index = i; });
    return items;
  }

  // Search words that mean a field rather than a subject.
  const STOP = new Set(['of', 'the', 'a', 'an', 'my', 'at', 'in', 'on', 'with', 'and', 'from', 'photos', 'photo', 'pictures', 'pics', 'shots', 'me', 'our', 'over', 'some']);
  const MONTHS = ['january', 'february', 'march', 'april', 'may', 'june', 'july', 'august', 'september', 'october', 'november', 'december'];

  function SampleSource(n) {
    const items = makeLibrary(n);
    const byId = new Map(items.map((i) => [i.id, i]));
    const words = (it) => (it._w ||= [...THEME[it.theme].words, THEME[it.theme].label.toLowerCase(), it.place.toLowerCase(), it.type,
      MONTHS[new Date(it.taken).getMonth()], String(new Date(it.taken).getFullYear())].join(' '));
    function parse(q) {
      const out = { terms: [], type: '', year: '', month: '' };
      const raw = String(q || '').toLowerCase().replace(/[^a-z0-9 ]+/g, ' ').split(/\s+/).filter(Boolean);
      for (const w of raw) {
        if (w === 'videos' || w === 'video' || w === 'clips') out.type = 'video';
        else if (/^(19|20)\d\d$/.test(w)) out.year = w;
        else if (MONTHS.includes(w)) out.month = w;
        else if (!STOP.has(w)) out.terms.push(w.replace(/s$/, ''));
      }
      return out;
    }
    function matches(it, f) {
      if (f.type && it.type !== f.type) return false;
      if (f.year && new Date(it.taken).getFullYear() !== Number(f.year)) return false;
      if (f.place && it.place !== f.place) return false;
      if (f.theme && it.theme !== f.theme) return false;
      if (f.ids && !f.ids.has(it.id)) return false;
      return true;
    }
    return {
      kind: 'sample',
      count: items.length,
      // {query, filters} -> items, newest first. A query keeps an item whose
      // words cover every subject term (a stem match, so "sunsets" finds sunset).
      list({ query, filters }) {
        const f = Object.assign({}, filters || {});
        const p = parse(query);
        if (p.type) f.type = p.type;
        if (p.year) f.year = p.year;
        return items.filter((it) => {
          if (!matches(it, f)) return false;
          if (p.month && MONTHS[new Date(it.taken).getMonth()] !== p.month) return false;
          if (!p.terms.length) return true;
          const w = words(it);
          return p.terms.every((t) => w.includes(t));
        });
      },
      item: (id) => byId.get(id),
      // Same theme first, then same place, nearest in time.
      similar(id, k) {
        const a = byId.get(id);
        if (!a) return [];
        return items.filter((b) => b.id !== id && (b.theme === a.theme || b.place === a.place))
          .map((b) => [b, (b.theme === a.theme ? 2 : 0) + (b.place === a.place ? 1 : 0) - Math.abs(b.taken - a.taken) / 3.15e11])
          .sort((x, y) => y[1] - x[1]).slice(0, k || 24).map((x) => x[0]);
      },
      collections() {
        const group = (key, label) => {
          const m = new Map();
          for (const it of items) { const k = key(it); if (!m.has(k)) m.set(k, []); m.get(k).push(it); }
          return [...m.entries()].map(([k, list]) => ({ key: k, label: label(k), count: list.length, cover: list.slice(0, 4) }))
            .sort((a, b) => b.count - a.count);
        };
        const byTheme = group((it) => it.theme, (k) => THEME[k].label);
        return {
          places: group((it) => it.place, (k) => k.split(',')[0]),
          things: byTheme.filter((c) => THEME[c.key].kind === 'subject'),
          landscapes: byTheme.filter((c) => THEME[c.key].kind !== 'subject'),
        };
      },
      suggest(q) {
        const s = String(q || '').trim().toLowerCase();
        if (!s) return [];
        const out = [];
        for (const t of THEMES) if (t.label.toLowerCase().startsWith(s) || t.words.some((w) => w.startsWith(s))) out.push(t.label.toLowerCase());
        for (const p of PLACES) if (p.name.toLowerCase().includes(s)) out.push(p.name.split(',')[0]);
        const base = out[0] || s;
        return [...new Set([s, base + ' videos', ...out])].slice(0, 6);
      },
      places: () => PLACES.map((p) => p.name),
      themes: () => THEMES,
      years: () => [...new Set(items.map((i) => new Date(i.taken).getFullYear()))].sort((a, b) => b - a),
    };
  }

  // ── Drawing ─────────────────────────────────────────────────────────────
  // Each sample is a small scene for its theme. Cached per item.
  const art = new Map();
  function scene(it) {
    const hit = art.get(it.id);
    if (hit) return hit;
    const r = rng(it.seed);
    const W = it.aw * 100, H = it.ah * 100;
    const P = THEME[it.theme].pal;
    const j = (v) => v + (r() - 0.5) * 0.2;
    let body = '';
    const sky = (a, b) => `<defs><linearGradient id="g" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="${a}"/><stop offset="1" stop-color="${b}"/></linearGradient></defs><rect width="${W}" height="${H}" fill="url(#g)"/>`;
    const hills = (y, c, amp) => { let d = `M0 ${H} L0 ${y}`; for (let x = 0; x <= W; x += W / 6) d += ` Q${x + W / 12} ${y - amp * r()} ${x + W / 6} ${y + (r() - 0.5) * amp}`; return `<path d="${d} L${W} ${H} Z" fill="${c}"/>`; };
    const peaks = (base, c, cap) => { let s = ''; for (let i = 0; i < 3; i++) { const cx = W * (0.15 + i * 0.35 + (r() - 0.5) * 0.2), h = H * (0.35 + r() * 0.3), w = W * (0.35 + r() * 0.2); s += `<polygon points="${cx - w / 2},${base} ${cx},${base - h} ${cx + w / 2},${base}" fill="${c}"/>`; if (cap) s += `<polygon points="${cx - w * 0.12},${base - h * 0.76} ${cx},${base - h} ${cx + w * 0.12},${base - h * 0.76}" fill="${cap}"/>`; } return s; };
    switch (it.theme) {
      case 'sunset': body = sky(P[0], P[2]) + `<circle cx="${W * j(0.5)}" cy="${H * 0.62}" r="${Math.min(W, H) * 0.12}" fill="${P[3]}"/>` + `<rect y="${H * 0.66}" width="${W}" height="${H * 0.34}" fill="${P[1]}" opacity=".85"/>`; break;
      case 'beach': body = sky(P[3], P[0]) + `<rect y="${H * 0.5}" width="${W}" height="${H * 0.22}" fill="${P[1]}"/>` + hills(H * 0.74, P[2], H * 0.03); break;
      case 'mountains': body = sky(P[3], P[0]) + peaks(H * 0.85, P[1], P[3]) + hills(H * 0.85, P[2], H * 0.05); break;
      case 'forest': { body = sky(P[0], P[3]); for (let i = 0; i < 14; i++) { const x = W * r(), h = H * (0.3 + r() * 0.4), w = h * 0.4; body += `<polygon points="${x - w / 2},${H} ${x},${H - h} ${x + w / 2},${H}" fill="${i % 2 ? P[1] : P[2]}"/>`; } break; }
      case 'city': { body = sky(P[2], P[0]); for (let x = 0; x < W; ) { const w = W * (0.06 + r() * 0.08), h = H * (0.25 + r() * 0.5); body += `<rect x="${x}" y="${H - h}" width="${w - 3}" height="${h}" fill="${P[2]}"/>`; for (let k = 0; k < 6; k++) body += `<rect x="${x + w * r() * 0.7}" y="${H - h * r()}" width="${w * 0.12}" height="${h * 0.03}" fill="${P[3]}"/>`; x += w; } break; }
      case 'snow': body = sky(P[0], P[2]) + peaks(H * 0.8, P[1], P[2]) + hills(H * 0.78, P[2], H * 0.06); break;
      case 'lake': body = sky(P[3], P[0]) + peaks(H * 0.55, P[2], '') + `<rect y="${H * 0.55}" width="${W}" height="${H * 0.45}" fill="${P[1]}"/>` + `<rect y="${H * 0.6}" x="${W * 0.2}" width="${W * 0.6}" height="${H * 0.02}" fill="${P[3]}" opacity=".5"/>`; break;
      case 'flowers': { body = sky(P[0], '#7fae63'); for (let i = 0; i < 22; i++) { const x = W * r(), y = H * (0.3 + r() * 0.7), s = Math.min(W, H) * (0.03 + r() * 0.05); body += `<circle cx="${x}" cy="${y}" r="${s}" fill="${P[1 + (i % 3)]}"/><circle cx="${x}" cy="${y}" r="${s * 0.35}" fill="#fff6c9"/>`; } break; }
      case 'desert': body = sky(P[3], P[0]) + hills(H * 0.62, P[1], H * 0.12) + hills(H * 0.78, P[2], H * 0.08); break;
      case 'waterfall': body = sky(P[3], P[0]) + `<rect x="${W * j(0.42)}" width="${W * 0.16}" height="${H}" fill="${P[1]}" opacity=".9"/>` + hills(H * 0.8, P[2], H * 0.05); break;
      case 'dog': body = sky(P[0], P[3]) + `<ellipse cx="${W * 0.5}" cy="${H * 0.68}" rx="${W * 0.26}" ry="${H * 0.16}" fill="${P[1]}"/><circle cx="${W * 0.66}" cy="${H * 0.46}" r="${Math.min(W, H) * 0.13}" fill="${P[1]}"/><ellipse cx="${W * 0.6}" cy="${H * 0.36}" rx="${W * 0.04}" ry="${H * 0.08}" fill="${P[2]}"/><circle cx="${W * 0.7}" cy="${H * 0.45}" r="${Math.min(W, H) * 0.015}" fill="#222"/>`; break;
      case 'food': body = sky('#b98a5e', '#7a5636') + `<circle cx="${W / 2}" cy="${H / 2}" r="${Math.min(W, H) * 0.34}" fill="${P[1]}"/><circle cx="${W / 2}" cy="${H / 2}" r="${Math.min(W, H) * 0.24}" fill="${P[0]}"/>` + [0, 1, 2, 3, 4].map(() => `<circle cx="${W / 2 + (r() - 0.5) * Math.min(W, H) * 0.3}" cy="${H / 2 + (r() - 0.5) * Math.min(W, H) * 0.3}" r="${Math.min(W, H) * 0.04}" fill="${r() < 0.5 ? P[2] : P[3]}"/>`).join(''); break;
      case 'night': { body = sky(P[0], P[3]); for (let i = 0; i < 70; i++) body += `<circle cx="${W * r()}" cy="${H * r() * 0.7}" r="${1 + r() * 2.2}" fill="${P[2]}" opacity="${0.4 + r() * 0.6}"/>`; body += `<circle cx="${W * j(0.75)}" cy="${H * 0.2}" r="${Math.min(W, H) * 0.06}" fill="${P[2]}"/>` + hills(H * 0.85, '#05091a', H * 0.06); break; }
      case 'autumn': { body = sky(P[0], '#f7e7c4'); for (let i = 0; i < 9; i++) { const x = W * r(), y = H * (0.35 + r() * 0.3), s = Math.min(W, H) * (0.1 + r() * 0.1); body += `<rect x="${x - 3}" y="${y}" width="6" height="${H - y}" fill="#5b3a22"/><circle cx="${x}" cy="${y}" r="${s}" fill="${P[1 + (i % 3)]}"/>`; } break; }
      default: body = sky(P[0], P[1]);
    }
    const url = 'data:image/svg+xml;charset=utf-8,' + encodeURIComponent(`<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 ${W} ${H}" preserveAspectRatio="xMidYMid slice">${body}</svg>`);
    if (art.size > 3000) art.delete(art.keys().next().value);
    art.set(it.id, url);
    return url;
  }

  // ── Metadata ────────────────────────────────────────────────────────────
  // Three origins, never mixed: what the file carried (EXIF/XMP/IPTC or the
  // video container), what the file system says, and what Mixpeek derived.
  // `raw` keeps every field as read, vendor ones included; the sections are a
  // view onto it, not a replacement.
  function metadata(it, src) {
    const r = rng(it.seed + 7);
    const d = new Date(it.taken);
    const iso = 50 * Math.pow(2, Math.floor(r() * 6));
    const fl = it.cam.f[Math.floor(r() * it.cam.f.length)];
    const ap = it.cam.ap[Math.floor(r() * it.cam.ap.length)];
    const sh = [1 / 2000, 1 / 1000, 1 / 500, 1 / 250, 1 / 125, 1 / 60, 1 / 30][Math.floor(r() * 7)];
    const px = it.type === 'video' ? (it.aw > it.ah ? [3840, 2160] : [2160, 3840]) : [it.aw * 1008, it.ah * 1008];
    const ext = it.type === 'video' ? 'MOV' : (it.cam.make === 'Apple' ? 'HEIC' : 'JPG');
    const name = (it.cam.make === 'Apple' ? 'IMG_' : 'DSC') + String(1000 + (it.seed % 9000)) + '.' + ext;
    const exifDate = d.toISOString().replace('T', ' ').replace(/-/g, ':').slice(0, 19);
    const raw = it.type === 'video' ? {
      QuickTime: { CreationDate: d.toISOString(), Duration: it.duration, ImageWidth: px[0], ImageHeight: px[1], VideoFrameRate: [24, 30, 60][it.seed % 3], CompressorID: 'hvc1', AvgBitrate: 42000000 + (it.seed % 9) * 1e6, AudioFormat: 'mp4a', AudioChannels: 2, AudioSampleRate: 48000, 'com.apple.quicktime.location.ISO6709': `+${it.lat.toFixed(4)}${it.lon >= 0 ? '+' : ''}${it.lon.toFixed(4)}/`, 'com.apple.quicktime.make': it.cam.make, 'com.apple.quicktime.model': it.cam.model },
    } : {
      EXIF: { Make: it.cam.make, Model: it.cam.model, LensModel: it.cam.lens, FocalLength: fl, FNumber: ap, ExposureTime: sh, ISO: iso, ExposureProgram: 'Normal program', ExposureBiasValue: 0, Flash: 'Off, Did not fire', Orientation: 'Horizontal (normal)', DateTimeOriginal: exifDate, OffsetTimeOriginal: '-08:00', PixelXDimension: px[0], PixelYDimension: px[1], ColorSpace: 'Uncalibrated' },
      GPS: { GPSLatitude: Math.abs(it.lat), GPSLatitudeRef: it.lat >= 0 ? 'N' : 'S', GPSLongitude: Math.abs(it.lon), GPSLongitudeRef: it.lon >= 0 ? 'E' : 'W', GPSAltitude: Math.round(r() * 2400) },
      ICC: { ProfileDescription: it.cam.profile },
      XMP: { 'xmp:CreatorTool': it.cam.make === 'Apple' ? '17.1' : 'Firmware 2.10', 'photoshop:DateCreated': d.toISOString() },
      IPTC: {},
      MakerNotes: it.cam.make === 'Apple' ? { 'Apple:RunTimeFlags': 'Valid', 'Apple:ContentIdentifier': 'A' + it.seed.toString(16).toUpperCase(), 'Apple:HDRHeadroom': +(r() * 3).toFixed(2) }
        : { [it.cam.make + ':ShutterCount']: 1000 + (it.seed % 40000), [it.cam.make + ':FilmMode']: 'Standard', [it.cam.make + ':InternalSerialNumber']: 'XXXX' + (it.seed % 9999) },
    };
    const size = it.type === 'video' ? it.duration * 5.3e6 : (2.1 + r() * 4) * 1e6;
    const checksum = (it.seed * 2654435761 >>> 0).toString(16).padStart(8, '0') + (it.seed >>> 3).toString(16).padStart(8, '0');
    const coll = src.collections();
    return {
      raw,
      general: [
        ['Date taken', d.toLocaleString(undefined, { dateStyle: 'full', timeStyle: 'short' }), 'embedded'],
        ['Dimensions', px[0] + ' × ' + px[1] + (it.type === 'photo' ? ' · ' + (px[0] * px[1] / 1e6).toFixed(1) + ' MP' : ''), 'embedded'],
        ['Color profile', it.type === 'photo' ? it.cam.profile : null, 'embedded'],
      ],
      camera: it.type === 'video' ? [['Device', it.cam.make + ' ' + it.cam.model, 'embedded']] : [
        ['Camera', it.cam.make + ' ' + it.cam.model, 'embedded'],
        ['Lens', it.cam.lens, 'embedded'],
        ['Exposure', 'ƒ/' + ap + ' · ' + (sh < 1 ? '1/' + Math.round(1 / sh) : sh) + ' s · ISO ' + iso, 'embedded'],
        ['Focal length', fl + ' mm', 'embedded'],
        ['Flash', 'Did not fire', 'embedded'],
        ['Orientation', 'Horizontal (normal)', 'embedded'],
      ],
      location: { place: it.place, lat: it.lat, lon: it.lon, alt: it.type === 'photo' ? raw.GPS.GPSAltitude : null },
      video: it.type === 'video' ? [
        ['Duration', fmtDuration(it.duration), 'embedded'],
        ['Resolution', px[0] + ' × ' + px[1], 'embedded'],
        ['Frame rate', raw.QuickTime.VideoFrameRate + ' fps', 'embedded'],
        ['Codec', 'HEVC (hvc1)', 'embedded'],
        ['Bitrate', (raw.QuickTime.AvgBitrate / 1e6).toFixed(0) + ' Mb/s', 'embedded'],
        ['Audio', 'AAC, stereo, 48 kHz', 'embedded'],
      ] : null,
      file: [
        ['Filename', name, 'file'],
        ['Format', ext + ' · ' + (it.type === 'video' ? 'video/quicktime' : ext === 'HEIC' ? 'image/heic' : 'image/jpeg'), 'file'],
        ['Size', fmtBytes(size), 'file'],
        ['Path', '/Volumes/Library/' + d.getFullYear() + '/' + String(d.getMonth() + 1).padStart(2, '0') + '/' + name, 'file'],
        ['Modified', d.toLocaleDateString(), 'file'],
        ['Checksum', 'sha256:' + checksum + '…', 'file'],
      ],
      mixpeek: [
        ['Namespace', null, 'mixpeek'],
        ['Document', null, 'mixpeek'],
        ['Tags', THEME[it.theme].words.slice(0, 4).join(', '), 'mixpeek'],
        ['Collection', THEME[it.theme].label, 'mixpeek'],
        ['Cluster', (coll.landscapes.concat(coll.things).findIndex((c) => c.key === it.theme) + 1) || null, 'mixpeek'],
      ],
    };
  }
  function fmtBytes(n) { return n > 1e9 ? (n / 1e9).toFixed(2) + ' GB' : (n / 1e6).toFixed(1) + ' MB'; }
  function fmtDuration(s) { const m = Math.floor(s / 60); return m + ':' + String(Math.floor(s % 60)).padStart(2, '0'); }

  // ── State ───────────────────────────────────────────────────────────────
  const S = {
    el: null, src: null, view: 'photos', query: '', filters: {}, collection: null, albumId: null,
    selected: new Set(), lastPick: -1, list: [], scroll: {}, layout: null,
    conn: { status: 'loading' }, onboarding: !store.get('sample_ok', false),
    favorites: new Set(store.get('favorites', [])), albums: store.get('albums', {}), recent: store.get('recent', []),
    viewer: null,
  };
  const scrollKey = () => S.view + '|' + S.query + '|' + JSON.stringify(S.filters) + '|' + (S.collection || '') + '|' + (S.albumId || '');

  // ── Connection (BYO Mixpeek) ───────────────────────────────────────────
  async function connLoad(check) {
    S.conn = Object.assign({}, S.conn, { status: 'loading' });
    paintChrome();
    try {
      const r = await fetch(API + '/api/photos/mixpeek' + (check ? '?check=1' : ''), { headers: headers() });
      const d = await r.json();
      S.conn = d.configured ? d : { status: 'disconnected', configured: false };
      if (!r.ok && !d.configured) S.conn = { status: 'error', configured: false, message: d.message || ('HTTP ' + r.status) };
    } catch (e) {
      S.conn = { status: 'error', configured: false, message: 'amux did not answer: ' + e.message };
    }
    if (S.conn.configured) S.onboarding = false;
    render();
  }
  async function connSave(form) {
    const btn = form.querySelector('.ph-btn-primary');
    const msg = form.querySelector('.ph-form-msg');
    form.querySelectorAll('.ph-field').forEach((f) => f.classList.remove('bad'));
    btn.disabled = true; btn.textContent = 'Checking…'; msg.textContent = '';
    const body = { endpoint: form.endpoint.value, api_key: form.api_key.value, namespace: form.namespace.value };
    try {
      const r = await fetch(API + '/api/photos/mixpeek', { method: 'POST', headers: headers({ 'Content-Type': 'application/json' }), body: JSON.stringify(body) });
      const d = await r.json();
      if (r.ok && d.status === 'connected') {
        S.conn = d; S.onboarding = false; closeSheet();
        toast(d.message || 'Connected');
        render();
        return;
      }
      msg.textContent = d.message || ('HTTP ' + r.status);
      const f = form.querySelector('[data-field="' + (d.field || 'endpoint') + '"]');
      if (f) f.classList.add('bad');
    } catch (e) {
      msg.textContent = 'amux did not answer: ' + e.message;
    }
    btn.disabled = false; btn.textContent = S.conn.configured ? 'Save' : 'Connect';
  }
  async function connForget() {
    if (typeof window.showConfirm === 'function' && !(await window.showConfirm('Disconnect this Mixpeek namespace from Photos? Your media on Mixpeek is not touched.', 'Disconnect'))) return;
    await fetch(API + '/api/photos/mixpeek', { method: 'DELETE', headers: headers() }).catch((e) => toast('Could not disconnect: ' + e.message));
    S.conn = { status: 'disconnected', configured: false };
    closeSheet(); render();
  }

  // ── Rendering ──────────────────────────────────────────────────────────
  const ICON = {
    photos: '<svg viewBox="0 0 24 24"><rect x="3" y="4" width="18" height="16" rx="3"/><path d="M3 16l5-5 4 4 3-3 6 6"/><circle cx="16" cy="9" r="1.6"/></svg>',
    search: '<svg viewBox="0 0 24 24"><circle cx="11" cy="11" r="6.5"/><path d="M16 16l4.5 4.5"/></svg>',
    explore: '<svg viewBox="0 0 24 24"><rect x="3" y="3" width="8" height="8" rx="2"/><rect x="13" y="3" width="8" height="8" rx="2"/><rect x="3" y="13" width="8" height="8" rx="2"/><rect x="13" y="13" width="8" height="8" rx="2"/></svg>',
    favorites: '<svg viewBox="0 0 24 24"><path d="M12 20s-7-4.4-7-10a4 4 0 0 1 7-2.6A4 4 0 0 1 19 10c0 5.6-7 10-7 10z"/></svg>',
    albums: '<svg viewBox="0 0 24 24"><rect x="4" y="6" width="14" height="14" rx="2.5"/><path d="M8 3h11a2 2 0 0 1 2 2v11"/></svg>',
    gear: '<svg viewBox="0 0 24 24"><circle cx="12" cy="12" r="3"/><path d="M12 2v3M12 19v3M4.2 4.2l2.1 2.1M17.7 17.7l2.1 2.1M2 12h3M19 12h3M4.2 19.8l2.1-2.1M17.7 6.3l2.1-2.1"/></svg>',
    close: '<svg viewBox="0 0 24 24"><path d="M6 6l12 12M18 6L6 18"/></svg>',
    info: '<svg viewBox="0 0 24 24"><circle cx="12" cy="12" r="9"/><path d="M12 11v6M12 7.5v.5"/></svg>',
    left: '<svg viewBox="0 0 24 24"><path d="M15 5l-7 7 7 7"/></svg>',
    right: '<svg viewBox="0 0 24 24"><path d="M9 5l7 7-7 7"/></svg>',
    play: '<svg viewBox="0 0 24 24"><path d="M8 5v14l11-7z"/></svg>',
    check: '<svg viewBox="0 0 24 24"><path d="M5 12.5l4.5 4.5L19 7.5"/></svg>',
    plus: '<svg viewBox="0 0 24 24"><path d="M12 5v14M5 12h14"/></svg>',
  };
  const NAV = [['photos', 'Photos'], ['search', 'Search'], ['explore', 'Explore'], ['favorites', 'Favorites'], ['albums', 'Albums']];

  function mount(el) {
    if (!S.src) {
      const t0 = performance.now();
      S.src = SampleSource(10000);
      console.info('[photos] sample library ' + S.src.count + ' items in ' + Math.round(performance.now() - t0) + 'ms');
    }
    if (S.el !== el || !el.querySelector('.ph-app')) {
      S.el = el;
      el.innerHTML = '<div class="ph-app"><aside class="ph-side"></aside><div class="ph-main"><header class="ph-top"></header><div class="ph-body"></div></div><nav class="ph-tabs"></nav><div class="ph-layer"></div></div>';
      el.addEventListener('keydown', onKey);
      window.addEventListener('resize', onResize);
    }
    window.scrollTo(0, 0);
    fit();
    if (S.conn.status === 'loading' || !S.connChecked) { S.connChecked = true; connLoad(true); }
    else render();
  }
  function leave() { saveScroll(); if (S.viewer) closeViewer(); }
  // The view is a child of <body>, which is not a flex column, so it gets the
  // screen's height minus where it starts (the same fit Messages uses,
  // _cvFit). Without it the view grew to the grid's full height, the page
  // scrolled instead of the grid, and virtual scrolling and the pinned date
  // never saw a scroll.
  function fit() {
    if (!S.el) return;
    const top = Math.max(0, S.el.getBoundingClientRect().top + window.scrollY);
    S.el.style.setProperty('--ph-top', top + 'px');
  }

  function paintChrome() {
    const app = S.el && S.el.querySelector('.ph-app');
    if (!app) return;
    const navBtn = ([id, label], cls) => '<button class="' + cls + (S.view === id ? ' on' : '') + '" data-nav="' + id + '" aria-label="' + label + '">' + ICON[id] + '<span>' + label + '</span></button>';
    const c = S.conn;
    const pill = c.status === 'loading' ? '<span class="ph-pill">Checking…</span>'
      : c.configured && c.status === 'connected' ? '<button class="ph-pill ok" data-act="settings">' + esc(c.namespace_name || c.namespace) + '</button>'
      : c.configured ? '<button class="ph-pill bad" data-act="settings">Mixpeek unreachable</button>'
      : '<button class="ph-pill" data-act="connect">Sample library · Connect</button>';
    app.querySelector('.ph-side').innerHTML = '<div class="ph-brand">Photos</div>' + NAV.map((n) => navBtn(n, 'ph-nav')).join('')
      + '<div class="ph-side-foot">' + pill + '</div>';
    app.querySelector('.ph-tabs').innerHTML = NAV.filter((n) => n[0] !== 'favorites').map((n) => navBtn(n, 'ph-tab')).join('');
    const top = app.querySelector('.ph-top');
    const sel = S.selected.size;
    if (sel) {
      top.innerHTML = '<div class="ph-selbar"><button class="ph-icon" data-act="clearsel" aria-label="Clear selection">' + ICON.close + '</button><b>' + sel + ' selected</b><span class="ph-grow"></span>'
        + '<button class="ph-chipbtn" data-act="fav-sel">' + ICON.favorites + '<span>Favorite</span></button><button class="ph-chipbtn" data-act="album-sel">' + ICON.plus + '<span>Add to album</span></button></div>';
      return;
    }
    if (!top.querySelector('.ph-search')) {
      top.innerHTML = '<div class="ph-search"><span class="ph-search-ico">' + ICON.search + '</span><input type="search" placeholder="Search your photos and videos" aria-label="Search your photos and videos" autocomplete="off" enterkeyhint="search"><button class="ph-icon ph-search-x" data-act="clearq" aria-label="Clear search">' + ICON.close + '</button><div class="ph-suggest" hidden></div></div>'
        + '<span class="ph-top-pill"></span><button class="ph-icon ph-gear" data-act="settings" aria-label="Library settings">' + ICON.gear + '</button>';
      const inp = top.querySelector('input');
      inp.addEventListener('input', () => suggest(inp.value));
      inp.addEventListener('focus', () => suggest(inp.value));
      inp.addEventListener('keydown', (e) => {
        if (e.key === 'Enter') { e.preventDefault(); runSearch(inp.value); inp.blur(); }
        else if (e.key === 'Escape') { hideSuggest(); inp.blur(); }
      });
      inp.addEventListener('blur', () => setTimeout(hideSuggest, 150));
    }
    top.querySelector('input').value = S.query;
    top.querySelector('.ph-search').classList.toggle('has-q', !!S.query);
    top.querySelector('.ph-top-pill').innerHTML = pill;
  }

  function suggest(q) {
    const box = S.el.querySelector('.ph-suggest');
    if (!box) return;
    const list = q.trim() ? S.src.suggest(q) : S.recent.slice(0, 6);
    if (!list.length) { box.hidden = true; return; }
    const head = q.trim() ? '' : '<div class="ph-suggest-h">Recent</div>';
    const peek = q.trim() ? S.src.list({ query: list[0] }).slice(0, 3) : [];
    box.innerHTML = '<div class="ph-suggest-list">' + head + list.map((s) => '<button data-q="' + esc(s) + '">' + ICON.search + '<span>' + esc(s) + '</span></button>').join('') + '</div>'
      + (peek.length ? '<div class="ph-suggest-peek">' + peek.map((it) => '<img alt="" src="' + scene(it) + '">').join('') + '<div>Photos and videos of <b>' + esc(list[0]) + '</b></div></div>' : '');
    box.hidden = false;
  }
  function hideSuggest() { const b = S.el && S.el.querySelector('.ph-suggest'); if (b) b.hidden = true; }

  function runSearch(q) {
    q = String(q || '').trim();
    saveScroll();
    S.query = q; S.view = q ? 'search' : (S.view === 'search' ? 'search' : S.view); S.collection = null;
    if (q) { S.recent = [q, ...S.recent.filter((x) => x !== q)].slice(0, 12); store.set('recent', S.recent); }
    hideSuggest();
    render();
  }

  function setView(v) {
    saveScroll();
    S.view = v; S.collection = null; S.albumId = null;
    if (v !== 'search') S.query = '';
    S.selected.clear();
    render();
  }

  function render() {
    if (!S.el) return;
    paintChrome();
    const body = S.el.querySelector('.ph-body');
    if (S.onboarding && !S.conn.configured && S.conn.status !== 'loading') { body.innerHTML = welcome(); return; }
    if (S.conn.status === 'loading' && !S.src) { body.innerHTML = '<div class="ph-state">Loading…</div>'; return; }
    const banner = connBanner();
    if (S.view === 'explore' && !S.collection) { body.innerHTML = banner + explore(); restoreScroll(body); return; }
    if (S.view === 'albums' && !S.albumId) { body.innerHTML = banner + albums(); restoreScroll(body); return; }
    if (S.view === 'search' && !S.query && !S.collection) { body.innerHTML = banner + searchHome(); restoreScroll(body); return; }
    // A grid: the library, a search, a collection, favorites or an album.
    const f = Object.assign({}, S.filters);
    let title = '';
    if (S.collection) { Object.assign(f, S.collection.filter); title = S.collection.label; }
    if (S.view === 'favorites') { f.ids = S.favorites; title = 'Favorites'; }
    if (S.view === 'albums' && S.albumId) { const a = S.albums[S.albumId]; f.ids = new Set(a ? a.ids : []); title = a ? a.name : 'Album'; }
    S.list = S.src.list({ query: S.view === 'search' ? S.query : '', filters: f });
    const showFilters = S.view === 'search' || S.collection;
    body.innerHTML = banner + (title ? '<div class="ph-title"><button class="ph-icon" data-act="back" aria-label="Back">' + ICON.left + '</button><h2>' + esc(title) + '</h2><span class="ph-count">' + S.list.length.toLocaleString() + '</span></div>' : '')
      + (showFilters ? filterBar() : '')
      + (S.view === 'search' && S.query ? '<div class="ph-results">' + S.list.length.toLocaleString() + ' results for <b>' + esc(S.query) + '</b></div>' : '')
      + '<div class="ph-grid-wrap"><div class="ph-sticky" hidden></div><div class="ph-grid" role="grid" aria-label="' + esc(title || 'Library') + '"></div></div>';
    if (!S.list.length) {
      body.querySelector('.ph-grid-wrap').innerHTML = emptyState();
      return;
    }
    layoutGrid();
    restoreScroll(body);
    paintGrid();
  }

  function connBanner() {
    const c = S.conn;
    if (c.configured && c.status === 'connected' && !store.get('banner_seen_' + c.namespace, false)) {
      return '<div class="ph-banner"><b>Connected to ' + esc(c.namespace_name || c.namespace) + '.</b> Your library appears here once indexing is set up. Until then you are browsing sample photos.<button class="ph-link" data-act="banner-ok">Got it</button></div>';
    }
    if (c.configured && c.status !== 'connected' && c.status !== 'loading') {
      return '<div class="ph-banner bad"><b>Can’t reach your Mixpeek.</b> ' + esc(c.message || '') + ' <button class="ph-link" data-act="recheck">Try again</button><button class="ph-link" data-act="settings">Edit</button></div>';
    }
    if (c.status === 'error') return '<div class="ph-banner bad">' + esc(c.message || 'Photos could not read its connection.') + ' <button class="ph-link" data-act="recheck">Try again</button></div>';
    return '';
  }

  function welcome() {
    const pick = [0, 37, 120, 233, 401, 777, 1300, 2021].map((i) => S.src.list({}).at(i * 3)).filter(Boolean);
    return '<div class="ph-welcome"><div class="ph-welcome-mosaic">' + pick.map((it) => '<img alt="" src="' + scene(it) + '">').join('') + '</div>'
      + '<h1>Your photos and videos, searchable</h1><p>Photos runs on your own Mixpeek: search everything you have shot in plain words, find similar moments and rediscover places and themes. Your originals stay where they are.</p>'
      + '<div class="ph-welcome-actions"><button class="ph-btn ph-btn-primary" data-act="connect">Connect Mixpeek</button><a class="ph-btn" href="https://mixpeek.com" target="_blank" rel="noopener">Get started with Mixpeek</a></div>'
      + '<button class="ph-link" data-act="sample">Look around with sample photos</button></div>';
  }

  function searchHome() {
    const c = S.src.collections();
    const chips = ['Beach vacation', 'Sunsets', 'My dog', 'Videos of waterfalls', 'Snow in the mountains'];
    return '<div class="ph-page"><div class="ph-chips">' + chips.map((q) => '<button class="ph-chip" data-q="' + esc(q) + '">“' + esc(q) + '”</button>').join('') + '</div>'
      + (S.recent.length ? '<h3>Recent searches</h3><div class="ph-chips">' + S.recent.slice(0, 8).map((q) => '<button class="ph-chip" data-q="' + esc(q) + '">' + ICON.search + esc(q) + '</button>').join('') + '</div>' : '')
      + rail('Places', c.places.slice(0, 10), 'place') + rail('Things', c.things, 'theme') + rail('Landscapes', c.landscapes.slice(0, 10), 'theme') + '</div>';
  }
  function rail(label, list, key) {
    return '<h3>' + label + '</h3><div class="ph-rail">' + list.map((c) => '<button class="ph-card" data-col="' + key + '|' + esc(c.key) + '|' + esc(c.label) + '"><img alt="" src="' + scene(c.cover[0]) + '"><span>' + esc(c.label) + '</span></button>').join('') + '</div>';
  }
  function explore() {
    const c = S.src.collections();
    const mosaic = (label, list, key) => '<h3>' + label + '</h3><div class="ph-mosaics">' + list.map((col) => '<button class="ph-mosaic" data-col="' + key + '|' + esc(col.key) + '|' + esc(col.label) + '">'
      + '<div class="ph-mosaic-imgs">' + col.cover.slice(0, 3).map((it) => '<img alt="" src="' + scene(it) + '">').join('') + '</div>'
      + '<div class="ph-mosaic-cap"><b>' + esc(col.label) + '</b><span>' + col.count.toLocaleString() + '</span></div></button>').join('') + '</div>';
    return '<div class="ph-page">' + mosaic('Places', c.places, 'place') + mosaic('Landscapes', c.landscapes, 'theme') + mosaic('Things', c.things, 'theme') + '</div>';
  }
  function albums() {
    const list = Object.entries(S.albums);
    const favs = [...S.favorites].map((id) => S.src.item(id)).filter(Boolean);
    const card = (act, cover, name, n) => '<button class="ph-mosaic" ' + act + '><div class="ph-mosaic-imgs one">' + (cover ? '<img alt="" src="' + scene(cover) + '">' : '<div class="ph-empty-art">' + ICON.albums + '</div>') + '</div><div class="ph-mosaic-cap"><b>' + esc(name) + '</b><span>' + n + '</span></div></button>';
    return '<div class="ph-page"><div class="ph-title-row"><h2>Albums</h2><button class="ph-btn" data-act="new-album">' + ICON.plus + 'New album</button></div><div class="ph-mosaics">'
      + card('data-nav="favorites"', favs[0], 'Favorites', favs.length)
      + list.map(([id, a]) => card('data-album="' + esc(id) + '"', S.src.item(a.ids[0]), a.name, a.ids.length)).join('') + '</div>'
      + (list.length ? '' : '<p class="ph-hint">Select photos and choose <b>Add to album</b>, or make a new album here.</p>') + '</div>';
  }
  function filterBar() {
    const f = S.filters;
    const opt = (v, l, cur) => '<option value="' + esc(v) + '"' + (String(cur || '') === String(v) ? ' selected' : '') + '>' + esc(l) + '</option>';
    return '<div class="ph-filters">'
      + ['', 'photo', 'video'].map((t) => '<button class="ph-chip' + ((f.type || '') === t ? ' on' : '') + '" data-type="' + t + '">' + (t ? (t === 'photo' ? 'Photos' : 'Videos') : 'All') + '</button>').join('')
      + '<select class="ph-select" data-filter="year" aria-label="Year">' + opt('', 'Any date') + S.src.years().map((y) => opt(y, y, f.year)).join('') + '</select>'
      + '<select class="ph-select" data-filter="place" aria-label="Location">' + opt('', 'Any place') + S.src.places().map((p) => opt(p, p.split(',')[0], f.place)).join('') + '</select>'
      + '<select class="ph-select" data-filter="theme" aria-label="Things">' + opt('', 'Anything') + S.src.themes().map((t) => opt(t.id, t.label, f.theme)).join('') + '</select>'
      + (Object.values(f).some(Boolean) ? '<button class="ph-link" data-act="clearf">Clear</button>' : '') + '</div>';
  }
  function emptyState() {
    if (S.view === 'favorites') return '<div class="ph-state"><div class="ph-state-art">' + ICON.favorites + '</div><b>No favorites yet</b><p>Tap the heart on any photo to keep it here.</p></div>';
    if (S.view === 'albums') return '<div class="ph-state"><div class="ph-state-art">' + ICON.albums + '</div><b>This album is empty</b><p>Select photos in your library and choose Add to album.</p></div>';
    const ideas = S.src.suggest((S.query || '').split(/\s+/)[0] || 'sun').slice(1, 4);
    return '<div class="ph-state"><div class="ph-state-art">' + ICON.search + '</div><b>No matches' + (S.query ? ' for “' + esc(S.query) + '”' : '') + '</b><p>Try fewer words or a broader filter.</p><div class="ph-chips">' + ideas.map((q) => '<button class="ph-chip" data-q="' + esc(q) + '">' + esc(q) + '</button>').join('') + '</div></div>';
  }

  // ── The grid: justified rows per day, rendered only near the viewport ──
  function layoutGrid() {
    const grid = S.el.querySelector('.ph-grid');
    const W = Math.max(280, grid.clientWidth);
    const mobile = W < 600;
    const target = mobile ? 118 : (W > 1500 ? 230 : 196);
    const gap = mobile ? 2 : 4;
    const head = mobile ? 44 : 56;
    const rows = [];
    let y = 0;
    const groups = [];
    for (const it of S.list) {
      const day = new Date(it.taken).toDateString();
      const g = groups[groups.length - 1];
      if (g && g.day === day) g.items.push(it); else groups.push({ day, items: [it], place: it.place });
    }
    for (const g of groups) {
      rows.push({ kind: 'head', top: y, h: head, g });
      y += head;
      let row = [], sum = 0;
      const flush = (last) => {
        if (!row.length) return;
        const avail = W - gap * (row.length - 1);
        let h = avail / sum;
        if (last && h > target * 1.4) h = target;
        let x = 0;
        rows.push({ kind: 'row', top: y, h, cells: row.map((it) => { const w = h * it.aw / it.ah; const c = { it, x, w }; x += w + gap; return c; }) });
        y += h + gap; row = []; sum = 0;
      };
      for (const it of g.items) {
        row.push(it); sum += it.aw / it.ah;
        if ((W - gap * (row.length - 1)) / sum <= target) flush(false);
      }
      flush(true);
    }
    S.layout = { rows, height: y + 24, W, groups };
    grid.style.height = S.layout.height + 'px';
    grid.innerHTML = '';
    S.rendered = new Map();
  }
  const today = new Date();
  function dayLabel(g) {
    const d = new Date(g.items[0].taken);
    const diff = Math.round((new Date(today.toDateString()) - new Date(d.toDateString())) / 86400000);
    const label = diff === 0 ? 'Today' : diff === 1 ? 'Yesterday' : d.toLocaleDateString(undefined, { weekday: 'short', month: 'short', day: 'numeric', year: d.getFullYear() === today.getFullYear() ? undefined : 'numeric' });
    return { label, place: g.place };
  }
  function paintGrid() {
    const body = S.el.querySelector('.ph-body');
    const grid = S.el.querySelector('.ph-grid');
    if (!grid || !S.layout) return;
    const gridTop = grid.offsetTop;
    // The scroll box's height, capped by the window: a box with no height
    // limit would make every row "near the screen".
    const vh = Math.min(body.clientHeight, window.innerHeight);
    const top = body.scrollTop - gridTop - vh;
    const bottom = body.scrollTop - gridTop + vh * 2;
    const rows = S.layout.rows;
    // Binary search for the first row that may be visible.
    let lo = 0, hi = rows.length - 1;
    while (lo < hi) { const m = (lo + hi) >> 1; if (rows[m].top + rows[m].h < top) lo = m + 1; else hi = m; }
    const want = new Set();
    for (let i = lo; i < rows.length && rows[i].top < bottom; i++) want.add(i);
    for (const [i, node] of S.rendered) if (!want.has(i)) { node.remove(); S.rendered.delete(i); }
    for (const i of want) {
      if (S.rendered.has(i)) continue;
      const row = rows[i];
      const node = document.createElement('div');
      node.className = row.kind === 'head' ? 'ph-day' : 'ph-row';
      node.style.transform = 'translateY(' + row.top + 'px)';
      node.style.height = row.h + 'px';
      if (row.kind === 'head') {
        const { label, place } = dayLabel(row.g);
        node.innerHTML = '<span class="ph-day-check" role="button" tabindex="0" aria-label="Select this day" data-day="' + i + '">' + ICON.check + '</span><b>' + esc(label) + '</b>' + (place && place !== 'Home' ? '<span class="ph-day-place">' + esc(place) + '</span>' : '') + '<span class="ph-day-n">' + row.g.items.length + '</span>';
      } else {
        for (const c of row.cells) {
          const t = document.createElement('div');
          t.className = 'ph-tile' + (S.selected.has(c.it.id) ? ' sel' : '');
          t.tabIndex = 0;
          t.setAttribute('role', 'gridcell');
          t.dataset.id = c.it.id;
          t.style.left = c.x + 'px';
          t.style.width = c.w + 'px';
          t.setAttribute('aria-label', (c.it.type === 'video' ? 'Video, ' : 'Photo, ') + THEME[c.it.theme].label + ', ' + c.it.place + ', ' + new Date(c.it.taken).toLocaleDateString());
          t.innerHTML = '<img alt="" decoding="async" loading="lazy" src="' + scene(c.it) + '">'
            + (c.it.type === 'video' ? '<span class="ph-badge">' + ICON.play + fmtDuration(c.it.duration) + '</span>' : '')
            + (S.favorites.has(c.it.id) ? '<span class="ph-fav">' + ICON.favorites + '</span>' : '')
            + '<span class="ph-check" role="checkbox" aria-checked="' + S.selected.has(c.it.id) + '" aria-label="Select">' + ICON.check + '</span>';
          node.appendChild(t);
        }
      }
      grid.appendChild(node);
      S.rendered.set(i, node);
    }
    // The pinned date: the group whose header is above the top edge.
    const sticky = S.el.querySelector('.ph-sticky');
    const y = body.scrollTop - gridTop;
    let gi = -1;
    for (let i = lo; i >= 0; i--) if (rows[i].kind === 'head' && rows[i].top <= y) { gi = i; break; }
    if (sticky) {
      if (gi >= 0 && y > rows[gi].h) { const { label } = dayLabel(rows[gi].g); sticky.textContent = label; sticky.hidden = false; }
      else sticky.hidden = true;
    }
  }
  let rafPending = false;
  function onScroll() {
    if (rafPending) return;
    rafPending = true;
    requestAnimationFrame(() => { rafPending = false; paintGrid(); S.scroll[scrollKey()] = S.el.querySelector('.ph-body').scrollTop; });
  }
  let resizeT = null;
  function onResize() {
    if (!S.el || !S.el.offsetParent) return;
    fit();
    clearTimeout(resizeT);
    resizeT = setTimeout(() => {
      if (!S.layout) return;
      // Keep the same first visible item in view across a relayout.
      const body = S.el.querySelector('.ph-body');
      const anchor = firstVisibleId();
      layoutGrid();
      const row = S.layout.rows.find((r) => r.kind === 'row' && r.cells.some((c) => c.it.id === anchor));
      if (row) body.scrollTop = row.top + S.el.querySelector('.ph-grid').offsetTop - 60;
      paintGrid();
    }, 120);
  }
  function firstVisibleId() {
    const body = S.el.querySelector('.ph-body');
    const y = body.scrollTop - S.el.querySelector('.ph-grid').offsetTop;
    const row = S.layout.rows.find((r) => r.kind === 'row' && r.top + r.h > y);
    return row ? row.cells[0].it.id : null;
  }
  function saveScroll() { const b = S.el && S.el.querySelector('.ph-body'); if (b) S.scroll[scrollKey()] = b.scrollTop; }
  function restoreScroll(body) {
    body.onscroll = onScroll;
    body.scrollTop = S.scroll[scrollKey()] || 0;
  }

  // ── Selection, favorites, albums ───────────────────────────────────────
  function toggleSelect(id, range) {
    const idx = S.list.findIndex((i) => i.id === id);
    if (range && S.lastPick >= 0 && idx >= 0) {
      const [a, b] = [Math.min(S.lastPick, idx), Math.max(S.lastPick, idx)];
      for (let i = a; i <= b; i++) S.selected.add(S.list[i].id);
    } else if (S.selected.has(id)) S.selected.delete(id); else S.selected.add(id);
    S.lastPick = idx;
    refreshTiles();
  }
  function refreshTiles() {
    paintChrome();
    S.el.querySelectorAll('.ph-tile').forEach((t) => {
      const on = S.selected.has(t.dataset.id);
      t.classList.toggle('sel', on);
      const c = t.querySelector('.ph-check'); if (c) c.setAttribute('aria-checked', String(on));
    });
    S.el.querySelector('.ph-app').classList.toggle('selecting', S.selected.size > 0);
  }
  function toggleFav(ids, on) {
    for (const id of ids) { if (on === undefined ? !S.favorites.has(id) : on) S.favorites.add(id); else S.favorites.delete(id); }
    store.set('favorites', [...S.favorites]);
  }
  async function addToAlbum(ids) {
    const names = Object.entries(S.albums);
    const ask = typeof window.showPrompt === 'function'
      ? window.showPrompt('Add to album' + (names.length ? ' (existing: ' + names.map(([, a]) => a.name).join(', ') + ')' : ''), names.length ? names[0][1].name : 'New album')
      : Promise.resolve(window.prompt('Album name', 'New album'));
    const name = ((await ask) || '').trim();
    if (!name) return;
    let entry = names.find(([, a]) => a.name.toLowerCase() === name.toLowerCase());
    if (!entry) { const id = 'a' + Date.now().toString(36); S.albums[id] = { name, ids: [], created: Date.now() }; entry = [id, S.albums[id]]; }
    entry[1].ids = [...new Set([...entry[1].ids, ...ids])];
    store.set('albums', S.albums);
    toast(ids.length + ' added to ' + entry[1].name);
  }

  // ── Viewer ─────────────────────────────────────────────────────────────
  function openViewer(id) {
    const list = S.list.length ? S.list : [];
    S.viewer = { list, i: Math.max(0, list.findIndex((x) => x.id === id)), info: store.get('info_open', false), zoom: 1, px: 0, py: 0, from: document.activeElement };
    if (S.viewer.i < 0 || !list.length) { S.viewer.list = [S.src.item(id)]; S.viewer.i = 0; }
    paintViewer();
  }
  function closeViewer() {
    const from = S.viewer && S.viewer.from;
    const id = S.viewer && S.viewer.list[S.viewer.i] && S.viewer.list[S.viewer.i].id;
    // A playing sample video's timer would keep ticking on detached nodes.
    if (S.viewer && S.viewer.timer) clearInterval(S.viewer.timer);
    S.viewer = null;
    if (S.vroot) { S.vroot.remove(); S.vroot = null; }
    // Back where the person was: the tile they ended on, in view.
    const tile = id && S.el.querySelector('.ph-tile[data-id="' + id + '"]');
    if (tile) tile.focus({ preventScroll: false }); else if (from && from.focus) from.focus({ preventScroll: true });
  }
  function paintViewer() {
    const v = S.viewer;
    const it = v.list[v.i];
    // On <body>, not in the view: inside the view's layer it shared that
    // layer's stacking context and the dashboard's fixed tab strip covered its
    // close, favorite and Info buttons.
    if (!S.vroot) {
      S.vroot = document.createElement('div');
      S.vroot.className = 'ph-v-root';
      S.vroot.addEventListener('keydown', onKey);
      document.body.appendChild(S.vroot);
    }
    const layer = S.vroot;
    const md = metadata(it, S.src);
    const sim = S.src.similar(it.id, 18);
    const fav = S.favorites.has(it.id);
    layer.innerHTML = '<div class="ph-viewer' + (v.info ? ' info' : '') + '" role="dialog" aria-modal="true" aria-label="Viewer">'
      + '<div class="ph-v-stage"><div class="ph-v-top"><button class="ph-icon" data-act="v-close" aria-label="Close (Esc)">' + ICON.close + '</button><span class="ph-grow"></span>'
      + '<button class="ph-icon' + (fav ? ' on' : '') + '" data-act="v-fav" aria-label="Favorite (F)">' + ICON.favorites + '</button>'
      + '<button class="ph-icon' + (v.info ? ' on' : '') + '" data-act="v-info" aria-label="Info (I)">' + ICON.info + '</button></div>'
      + (v.i > 0 ? '<button class="ph-v-nav prev" data-act="v-prev" aria-label="Previous">' + ICON.left + '</button>' : '')
      + (v.i < v.list.length - 1 ? '<button class="ph-v-nav next" data-act="v-next" aria-label="Next">' + ICON.right + '</button>' : '')
      + '<div class="ph-v-media"><img class="ph-v-img" alt="' + esc(THEME[it.theme].label + ' at ' + it.place) + '" src="' + scene(it) + '" draggable="false">'
      + (it.type === 'video' ? '<div class="ph-v-video"><button class="ph-v-play" data-act="v-play" aria-label="Play">' + ICON.play + '</button><div class="ph-v-bar"><i></i></div><span class="ph-v-time">0:00 / ' + fmtDuration(it.duration) + '</span></div>' : '')
      + '</div>'
      + '<div class="ph-v-similar"><b>Similar</b><div class="ph-v-strip">' + sim.map((s) => '<button data-open="' + s.id + '"><img alt="" src="' + scene(s) + '"></button>').join('') + '</div>'
      + '<div class="ph-v-related"><button class="ph-chip" data-col="theme|' + it.theme + '|' + esc(THEME[it.theme].label) + '">' + esc(THEME[it.theme].label) + '</button><button class="ph-chip" data-col="place|' + esc(it.place) + '|' + esc(it.place.split(',')[0]) + '">' + esc(it.place.split(',')[0]) + '</button></div></div>'
      + '</div>' + (v.info ? infoPanel(it, md) : '') + '</div>';
    bindZoom(layer.querySelector('.ph-v-media'));
    layer.querySelector('.ph-viewer').focus && layer.querySelector('.ph-viewer').setAttribute('tabindex', '-1');
    layer.querySelector('.ph-viewer').focus();
  }
  function move(d) {
    const v = S.viewer;
    const n = v.i + d;
    if (n < 0 || n >= v.list.length) return;
    v.i = n; v.zoom = 1; v.px = v.py = 0;
    if (v.timer) { clearInterval(v.timer); v.timer = null; }
    paintViewer();
  }
  function bindZoom(media) {
    const img = media.querySelector('.ph-v-img');
    const v = S.viewer;
    const apply = () => { img.style.transform = 'translate(' + v.px + 'px,' + v.py + 'px) scale(' + v.zoom + ')'; media.classList.toggle('zoomed', v.zoom > 1); };
    apply();
    media.addEventListener('dblclick', (e) => { v.zoom = v.zoom > 1 ? 1 : 2.5; if (v.zoom === 1) v.px = v.py = 0; else { const r = media.getBoundingClientRect(); v.px = (r.width / 2 - (e.clientX - r.left)) * 1.5; v.py = (r.height / 2 - (e.clientY - r.top)) * 1.5; } apply(); });
    media.addEventListener('wheel', (e) => { if (!e.ctrlKey && !e.metaKey) return; e.preventDefault(); v.zoom = Math.min(6, Math.max(1, v.zoom * (e.deltaY < 0 ? 1.12 : 0.89))); if (v.zoom === 1) v.px = v.py = 0; apply(); }, { passive: false });
    // One finger: swipe to the next item, or pan when zoomed.
    let start = null;
    media.addEventListener('pointerdown', (e) => { if (e.target.closest('button')) return; start = { x: e.clientX, y: e.clientY, px: v.px, py: v.py }; media.setPointerCapture(e.pointerId); });
    media.addEventListener('pointermove', (e) => { if (!start || v.zoom === 1) return; v.px = start.px + e.clientX - start.x; v.py = start.py + e.clientY - start.y; apply(); });
    media.addEventListener('pointerup', (e) => {
      if (!start) return;
      const dx = e.clientX - start.x, dy = e.clientY - start.y;
      start = null;
      if (v.zoom === 1 && Math.abs(dx) > 60 && Math.abs(dx) > Math.abs(dy)) move(dx < 0 ? 1 : -1);
      else if (v.zoom === 1 && dy > 120 && Math.abs(dy) > Math.abs(dx)) closeViewer();
    });
  }
  function playSample() {
    const v = S.viewer;
    const it = v.list[v.i];
    const bar = S.vroot.querySelector('.ph-v-bar i'), time = S.vroot.querySelector('.ph-v-time'), btn = S.vroot.querySelector('.ph-v-play');
    if (!bar) return;
    if (v.timer) { clearInterval(v.timer); v.timer = null; btn.classList.remove('playing'); return; }
    let t = v.t || 0;
    btn.classList.add('playing');
    v.timer = setInterval(() => {
      t = Math.min(it.duration, t + 0.25); v.t = t;
      bar.style.width = (t / it.duration * 100) + '%';
      time.textContent = fmtDuration(t) + ' / ' + fmtDuration(it.duration);
      if (t >= it.duration) { clearInterval(v.timer); v.timer = null; v.t = 0; btn.classList.remove('playing'); }
    }, 250);
  }
  function infoPanel(it, md) {
    const badge = { embedded: 'From the file', file: 'File system', mixpeek: 'Mixpeek' };
    const rows = (list) => list.map(([k, val, src]) => '<div class="ph-kv"><span>' + esc(k) + '</span><b class="' + (val == null || val === '' ? 'na' : '') + '">' + esc(val == null || val === '' ? 'Not available' : val) + '</b><i class="ph-src ' + src + '" title="' + badge[src] + '"></i></div>').join('');
    const sec = (title, body, src) => '<section><h4>' + title + (src ? '<em class="ph-src-l ' + src + '">' + badge[src] + '</em>' : '') + '</h4>' + body + '</section>';
    const L = md.location;
    const conn = S.conn.configured ? (S.conn.namespace_name || S.conn.namespace) : null;
    md.mixpeek[0][1] = conn;
    const map = L && L.lat != null ? mapPreview(L.lat, L.lon) : '';
    return '<aside class="ph-info" aria-label="Info"><div class="ph-info-head"><b>Info</b><button class="ph-icon" data-act="v-info" aria-label="Close info">' + ICON.close + '</button></div>'
      + sec('General', rows(md.general), 'embedded')
      + sec('Camera', rows(md.camera), 'embedded')
      + sec('Location', L && L.lat != null ? map + rows([['Place', L.place, 'mixpeek'], ['Coordinates', L.lat.toFixed(5) + ', ' + L.lon.toFixed(5), 'embedded'], ['Altitude', L.alt != null ? L.alt + ' m' : null, 'embedded']]) : rows([['Coordinates', null, 'embedded']]))
      + (md.video ? sec('Video', rows(md.video), 'embedded') : '')
      + sec('File', rows(md.file), 'file')
      + sec('Mixpeek', rows(md.mixpeek) + (conn ? '' : '<p class="ph-hint">Shown for a sample item. Connect Mixpeek to see your own documents, features and clusters.</p>'), 'mixpeek')
      + '<section><details><summary>All metadata as read (' + Object.keys(md.raw).length + ' groups)</summary><pre>' + esc(JSON.stringify(md.raw, null, 2)) + '</pre></details></section></aside>';
  }
  function mapPreview(lat, lon) {
    // A schematic tile: a graticule and a pin, drawn locally so the panel never
    // calls a map provider; "Open in Maps" hands off to the real one.
    const x = ((lon + 180) / 360) * 300, y = ((90 - lat) / 180) * 150;
    const lines = [...Array(13)].map((_, i) => '<path d="M' + i * 25 + ' 0V150" />').join('') + [...Array(7)].map((_, i) => '<path d="M0 ' + i * 25 + 'H300" />').join('');
    const vb = (x - 40) + ' ' + (y - 22) + ' 80 44';
    return '<a class="ph-map" href="https://maps.apple.com/?ll=' + lat.toFixed(5) + ',' + lon.toFixed(5) + '" target="_blank" rel="noopener" aria-label="Open in Maps"><svg viewBox="' + vb + '" preserveAspectRatio="xMidYMid slice"><rect x="-10" y="-10" width="320" height="170" class="ph-map-sea"/><g class="ph-map-grid">' + lines + '</g><circle cx="' + x + '" cy="' + y + '" r="2.4" class="ph-map-pin"/><circle cx="' + x + '" cy="' + y + '" r="6" class="ph-map-halo"/></svg><span>Open in Maps</span></a>';
  }

  // ── Events ─────────────────────────────────────────────────────────────
  function onClick(e) {
    const t = e.target;
    const q = t.closest('[data-q]'); if (q && S.el.contains(q)) { runSearch(q.dataset.q); return; }
    const nav = t.closest('[data-nav]'); if (nav) { setView(nav.dataset.nav); return; }
    const col = t.closest('[data-col]');
    if (col) {
      const [key, val, label] = col.dataset.col.split('|');
      if (S.viewer) closeViewer();
      saveScroll();
      S.view = S.view === 'search' || S.view === 'explore' ? S.view : 'explore';
      S.collection = { label, filter: key === 'place' ? { place: val } : { theme: val } };
      render(); return;
    }
    const alb = t.closest('[data-album]'); if (alb) { saveScroll(); S.albumId = alb.dataset.album; render(); return; }
    const open = t.closest('[data-open]'); if (open) { const v = S.viewer; const i = v.list.findIndex((x) => x.id === open.dataset.open); if (i >= 0) { v.i = i; paintViewer(); } else { v.list = [S.src.item(open.dataset.open), ...S.src.similar(open.dataset.open, 30)]; v.i = 0; paintViewer(); } return; }
    const day = t.closest('[data-day]');
    if (day) { const row = S.layout.rows[Number(day.dataset.day)]; const all = row.g.items.every((i) => S.selected.has(i.id)); row.g.items.forEach((i) => all ? S.selected.delete(i.id) : S.selected.add(i.id)); refreshTiles(); return; }
    const tile = t.closest('.ph-tile');
    if (tile) {
      if (t.closest('.ph-check') || S.selected.size || e.shiftKey || e.metaKey || e.ctrlKey) { toggleSelect(tile.dataset.id, e.shiftKey); return; }
      openViewer(tile.dataset.id); return;
    }
    const typ = t.closest('[data-type]'); if (typ) { S.filters.type = typ.dataset.type || undefined; render(); return; }
    const act = t.closest('[data-act]'); if (!act) return;
    const a = act.dataset.act;
    if (a === 'connect' || a === 'settings') openSheet();
    else if (a === 'sample') { S.onboarding = false; store.set('sample_ok', true); render(); }
    else if (a === 'clearq') { S.query = ''; render(); }
    else if (a === 'clearf') { S.filters = {}; render(); }
    else if (a === 'back') { saveScroll(); if (S.collection) S.collection = null; else S.albumId = null; render(); }
    else if (a === 'clearsel') { S.selected.clear(); refreshTiles(); }
    else if (a === 'fav-sel') { toggleFav([...S.selected], true); toast(S.selected.size + ' added to Favorites'); S.selected.clear(); render(); }
    else if (a === 'album-sel') { const ids = [...S.selected]; addToAlbum(ids).then(() => { S.selected.clear(); render(); }); }
    else if (a === 'new-album') addToAlbum([]).then(render);
    else if (a === 'banner-ok') { store.set('banner_seen_' + S.conn.namespace, true); render(); }
    else if (a === 'recheck') connLoad(true);
    else if (a === 'v-close') closeViewer();
    else if (a === 'v-prev') move(-1);
    else if (a === 'v-next') move(1);
    else if (a === 'v-fav') { toggleFav([S.viewer.list[S.viewer.i].id]); paintViewer(); }
    else if (a === 'v-info') { S.viewer.info = !S.viewer.info; store.set('info_open', S.viewer.info); paintViewer(); }
    else if (a === 'v-play') playSample();
    else if (a === 'sheet-close') closeSheet();
    else if (a === 'forget') connForget();
  }
  function onChange(e) {
    const s = e.target.closest('[data-filter]');
    if (!s) return;
    S.filters[s.dataset.filter] = s.value || undefined;
    render();
  }
  function onKey(e) {
    if (S.viewer) {
      if (e.key === 'Escape') { e.preventDefault(); e.stopPropagation(); closeViewer(); }
      else if (e.key === 'ArrowRight') { e.preventDefault(); move(1); }
      else if (e.key === 'ArrowLeft') { e.preventDefault(); move(-1); }
      else if (e.key === 'i' || e.key === 'I') { S.viewer.info = !S.viewer.info; store.set('info_open', S.viewer.info); paintViewer(); }
      else if (e.key === 'f' || e.key === 'F') { toggleFav([S.viewer.list[S.viewer.i].id]); paintViewer(); }
      else if (e.key === ' ' && S.viewer.list[S.viewer.i].type === 'video') { e.preventDefault(); playSample(); }
      return;
    }
    if (e.target.matches && e.target.matches('input, select, textarea')) return;
    if (e.key === '/') { e.preventDefault(); S.el.querySelector('.ph-search input').focus(); return; }
    if (e.key === 'Escape' && S.selected.size) { S.selected.clear(); refreshTiles(); return; }
    const tile = e.target.closest && e.target.closest('.ph-tile');
    if (!tile) return;
    if (e.key === 'Enter') { e.preventDefault(); openViewer(tile.dataset.id); return; }
    if (e.key === ' ') { e.preventDefault(); toggleSelect(tile.dataset.id, e.shiftKey); return; }
    const arrows = { ArrowRight: 1, ArrowLeft: -1, ArrowDown: 'down', ArrowUp: 'up' };
    if (!(e.key in arrows)) return;
    e.preventDefault();
    const idx = S.list.findIndex((x) => x.id === tile.dataset.id);
    let next = idx;
    const d = arrows[e.key];
    if (typeof d === 'number') next = idx + d;
    else {
      // Up/down: the nearest tile in the row above or below by x.
      const r = tile.getBoundingClientRect();
      const probe = document.elementFromPoint(r.left + r.width / 2, d === 'down' ? r.bottom + r.height / 2 + 8 : r.top - r.height / 2 - 8);
      const t2 = probe && probe.closest && probe.closest('.ph-tile');
      if (t2) { t2.focus(); t2.scrollIntoView({ block: 'nearest' }); return; }
      next = idx + (d === 'down' ? 4 : -4);
    }
    next = Math.max(0, Math.min(S.list.length - 1, next));
    focusItem(S.list[next].id);
  }
  function focusItem(id) {
    let t = S.el.querySelector('.ph-tile[data-id="' + id + '"]');
    if (!t) {
      const row = S.layout.rows.find((r) => r.kind === 'row' && r.cells.some((c) => c.it.id === id));
      if (row) { S.el.querySelector('.ph-body').scrollTop = row.top + S.el.querySelector('.ph-grid').offsetTop - 80; paintGrid(); }
      t = S.el.querySelector('.ph-tile[data-id="' + id + '"]');
    }
    if (t) { t.focus(); t.scrollIntoView({ block: 'nearest' }); }
  }

  // ── Connection sheet ───────────────────────────────────────────────────
  function openSheet() {
    const c = S.conn;
    const layer = S.el.querySelector('.ph-layer');
    const status = c.configured ? '<div class="ph-conn-status ' + (c.status === 'connected' ? 'ok' : 'bad') + '"><i></i>' + (c.status === 'connected' ? 'Connected to <b>' + esc(c.namespace_name || c.namespace) + '</b>' : esc(c.message || 'Not reachable')) + '</div>' : '';
    layer.innerHTML = '<div class="ph-sheet-back" data-act="sheet-close"></div><form class="ph-sheet" autocomplete="off"><div class="ph-sheet-head"><b>' + (c.configured ? 'Your Mixpeek' : 'Connect Mixpeek') + '</b><button type="button" class="ph-icon" data-act="sheet-close" aria-label="Close">' + ICON.close + '</button></div>'
      + '<p class="ph-hint">Photos reads one Mixpeek namespace as your library. Use a Mixpeek you run yourself or a hosted one. The key stays on this amux; it is never sent to your browser.</p>' + status
      + '<label class="ph-field" data-field="endpoint"><span>API endpoint</span><input name="endpoint" class="input" inputmode="url" placeholder="https://api.mixpeek.com or http://localhost:8000" value="' + esc(c.endpoint || '') + '" required></label>'
      + '<label class="ph-field" data-field="api_key"><span>API key <em>optional for a local instance</em></span><input name="api_key" class="input" type="password" placeholder="' + (c.key_set ? 'Saved. Leave blank to keep it.' : 'mxp_sk_…') + '"></label>'
      + '<label class="ph-field" data-field="namespace"><span>Namespace ID</span><input name="namespace" class="input" placeholder="ns_…" value="' + esc(c.namespace || '') + '" required></label>'
      + '<div class="ph-form-msg" role="alert"></div><div class="ph-sheet-actions">' + (c.configured ? '<button type="button" class="ph-btn" data-act="forget">Disconnect</button>' : '<a class="ph-btn" href="https://mixpeek.com" target="_blank" rel="noopener">Get started with Mixpeek</a>')
      + '<span class="ph-grow"></span><button class="ph-btn ph-btn-primary" type="submit">' + (c.configured ? 'Save' : 'Connect') + '</button></div></form>';
    const form = layer.querySelector('form');
    form.addEventListener('submit', (e) => { e.preventDefault(); connSave(form); });
    // At once: a delayed focus pulled the cursor back into this field while
    // someone was already typing in the next one.
    form.endpoint.focus();
  }
  function closeSheet() { const l = S.el && S.el.querySelector('.ph-layer'); if (l && l.querySelector('.ph-sheet')) l.innerHTML = ''; }

  const ours = (t) => (S.el && S.el.contains(t)) || (S.vroot && S.vroot.contains(t));
  document.addEventListener('click', (e) => { if (ours(e.target)) onClick(e); });
  document.addEventListener('change', (e) => { if (S.el && S.el.contains(e.target)) onChange(e); });

  window.AmuxPhotos = { mount, leave, _state: S, _source: () => S.src, _metadata: (id) => metadata(S.src.item(id), S.src) };
})();
