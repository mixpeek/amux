// The page script `amux order` runs to type a card into the current checkout
// (api/vault.rs `fill_script` puts the values in, as JSON, at the one
// placeholder in the first line of code below). Values travel only to the page. It returns WHICH fields it
// filled, and for a field that did not take, its key and lengths, never a value.
//
// GE-1121 (2026-10-17), three ways this used to report success on a form it
// had not filled:
// - fields named only by id (`card-number`, `expiration-month`, `add-city`)
//   matched nothing, because every selector looked at `name` or autocomplete;
// - a month select whose options read "1 - January" got selectedIndex -1, since
//   setting `value` to "01" selects nothing when no option has that value;
// - a masked card input kept 5 characters, and nothing read the field back.
// Every write is now read back; a field whose value did not take is reported in
// `mismatched`, and the caller treats it as not filled.
(async () => {
  const v = __AMUX_FILL_VALUES__;
  const visible = (el) => el && !el.disabled && !el.readOnly && (el.offsetParent !== null || el.getClientRects().length > 0);
  const pick = (sels) => {
    for (const s of sels) {
      for (const el of document.querySelectorAll(s)) if (visible(el)) return el;
    }
    return null;
  };
  // Each key lists attribute fragments; each fragment is tried against
  // autocomplete first, then name, then id.
  const by = (tags, frags) => {
    const out = [];
    for (const f of frags) for (const t of tags) out.push(t + '[name*="' + f + '" i]', t + '[id*="' + f + '" i]');
    return out;
  };
  const INP = ['input'], SEL = ['select'], ANY = ['input', 'select'];
  const F = {
    number: ['[autocomplete="cc-number"]'].concat(by(INP, ['cardnumber', 'card_number', 'card-number', 'ccnumber', 'cc-number', 'cc_number'])),
    exp: ['[autocomplete="cc-exp"]', 'input[name*="expiry" i]:not([name*="month" i]):not([name*="year" i])', 'input[id*="expiry" i]:not([id*="month" i]):not([id*="year" i])', 'input[name="exp" i]', 'input[id="exp" i]'],
    exp_month: ['[autocomplete="cc-exp-month"]'].concat(by(ANY, ['exp_month', 'exp-month', 'expiration-month', 'expiration_month', 'expmonth', 'cc-month', 'card-month']), by(SEL, ['month'])),
    exp_year: ['[autocomplete="cc-exp-year"]'].concat(by(ANY, ['exp_year', 'exp-year', 'expiration-year', 'expiration_year', 'expyear', 'cc-year', 'card-year']), by(SEL, ['year'])),
    cvc: ['[autocomplete="cc-csc"]'].concat(by(INP, ['cvc', 'cvv', 'csc', 'security-code', 'securitycode', 'security_code'])),
    name: ['[autocomplete="cc-name"]'].concat(by(INP, ['cardholder', 'nameoncard', 'name-on-card', 'name_on_card', 'card_name', 'card-name', 'name-card', 'name_card', 'cc-name'])),
    address: ['[autocomplete="address-line1"]', '[autocomplete="street-address"]'].concat(by(INP, ['address1', 'address-1', 'address_1', 'address-one', 'address_one', 'line1', 'street'])),
    city: ['[autocomplete="address-level2"]'].concat(by(INP, ['city'])),
    state: ['[autocomplete="address-level1"]'].concat(by(SEL, ['state', 'region', 'province']), by(INP, ['state', 'region', 'province'])),
    zip: ['[autocomplete="postal-code"]'].concat(by(INP, ['zip', 'postal', 'postcode'])),
    country: ['[autocomplete="country"]', '[autocomplete="country-name"]'].concat(by(SEL, ['country'])),
    email: ['[autocomplete="email"]', 'input[type="email"]'],
  };
  const MONTHS = ['january', 'february', 'march', 'april', 'may', 'june', 'july', 'august', 'september', 'october', 'november', 'december'];
  const norm = (x) => String(x == null ? '' : x).trim().toLowerCase();
  const digits = (x) => String(x == null ? '' : x).replace(/\D/g, '');
  // The option a select should take for `val`: an exact value or label, else
  // the same number however it is written ("1", "01", "1 - January"), else a
  // month or state named in the label.
  const optionFor = (el, key, val) => {
    const opts = [...el.options].filter(o => !o.disabled);
    const want = norm(val);
    let o = opts.find(o => norm(o.value) === want) || opts.find(o => norm(o.text) === want);
    if (o) return o;
    const n = parseInt(digits(val), 10);
    if (!isNaN(n)) {
      const lead = (t) => { const m = norm(t).match(/^(\d+)/); return m ? parseInt(m[1], 10) : NaN; };
      const yearish = key === 'exp_year';
      const same = (x) => x === n || (yearish && (x % 100) === (n % 100) && (x > 99 || n > 99 || x === n));
      o = opts.find(o => same(lead(o.value))) || opts.find(o => same(lead(o.text)));
      if (o) return o;
      if (key === 'exp_month' && n >= 1 && n <= 12) {
        o = opts.find(o => norm(o.text).includes(MONTHS[n - 1]) || norm(o.text).includes(MONTHS[n - 1].slice(0, 3)));
        if (o) return o;
      }
    }
    if (want.length > 1) o = opts.find(o => norm(o.text).startsWith(want) || norm(o.value).startsWith(want));
    return o || null;
  };
  const fire = (el, type) => el.dispatchEvent(new Event(type, { bubbles: true }));
  const setNative = (el, val) => {
    const proto = el.tagName === 'SELECT' ? HTMLSelectElement.prototype : (el.tagName === 'TEXTAREA' ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype);
    Object.getOwnPropertyDescriptor(proto, 'value').set.call(el, val);
  };
  // Types one character at a time, the way a person does, for inputs whose
  // own script reformats or truncates a value written in one go.
  const typeIn = async (el, val) => {
    el.focus();
    setNative(el, '');
    fire(el, 'input');
    for (const ch of String(val)) {
      el.dispatchEvent(new KeyboardEvent('keydown', { key: ch, bubbles: true }));
      el.dispatchEvent(new KeyboardEvent('keypress', { key: ch, bubbles: true }));
      const cur = el.value;
      setNative(el, cur + ch);
      el.dispatchEvent(new InputEvent('input', { bubbles: true, inputType: 'insertText', data: ch }));
      el.dispatchEvent(new KeyboardEvent('keyup', { key: ch, bubbles: true }));
      await new Promise(r => setTimeout(r, 0));
    }
    fire(el, 'change');
    el.blur();
  };
  // Did the field take the value? Card-like keys compare digits, so a mask
  // that adds spaces or slashes still counts; text compares case-insensitively.
  const DIGIT_KEYS = { number: 1, cvc: 1, zip: 1, exp: 1 };
  const took = (el, key, val) => {
    if (el.tagName === 'SELECT') return el.selectedIndex >= 0 && !!el.options[el.selectedIndex] && el.options[el.selectedIndex].value !== '';
    if (DIGIT_KEYS[key]) return digits(el.value) === digits(val) && digits(val) !== '';
    return norm(el.value) === norm(val);
  };
  // Two single-field modes for real typing (api/vault.rs, payment iframes): a
  // field that rejects script-set values (Stripe, Braintree) is focused and
  // cleared here, typed into by Chrome itself (Input.insertText), then read back.
  if (v.__focus) {
    const el = pick(F[v.__focus] || []);
    if (!el) return { focused: false };
    el.focus(); setNative(el, ''); fire(el, 'input');
    return { focused: document.activeElement === el };
  }
  if (v.__check) {
    const el = pick(F[v.__check.key] || []);
    return { took: !!el && took(el, v.__check.key, String(v.__check.val)) };
  }
  const filled = [], not_found = [], mismatched = [], used = new Set();
  const wantsSplit = !pick(F.exp);
  for (const [k, sels] of Object.entries(F)) {
    if (v[k] == null || v[k] === '') continue;
    if (k === 'exp' && wantsSplit) continue;
    if ((k === 'exp_month' || k === 'exp_year') && !wantsSplit) continue;
    const el = pick(sels.map(s => s));
    if (!el || used.has(el)) { not_found.push(k); continue; }
    used.add(el);
    const val = String(v[k]);
    if (el.tagName === 'SELECT') {
      const o = optionFor(el, k, val);
      if (o) { el.focus(); el.selectedIndex = o.index; fire(el, 'input'); fire(el, 'change'); el.blur(); }
    } else {
      el.focus(); setNative(el, val); fire(el, 'input'); fire(el, 'change'); el.blur();
      if (!took(el, k, val)) await typeIn(el, val);
    }
    if (took(el, k, val)) filled.push(k);
    else mismatched.push({ key: k, field: el.id || el.name || el.tagName.toLowerCase(),
      tag: el.tagName.toLowerCase(), expected_len: val.length, actual_len: el.tagName === 'SELECT' ? null : String(el.value).length,
      selected_index: el.tagName === 'SELECT' ? el.selectedIndex : undefined });
  }
  const iframes = [...document.querySelectorAll('iframe')].filter(f => /stripe|braintree|adyen|checkout|payment|card/i.test(f.src || f.name || '')).length;
  return { filled, not_found, mismatched, payment_iframes: iframes, url: location.href };
})()
