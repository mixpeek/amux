// GE-1121: the page script `amux order` runs (crates/amux-server/src/api/
// vault_fill.js, the exact bytes the server sends) driven in a real Chromium
// against FIXTURE checkout forms. Test values only; no vault, no server.
//
// The fixture copies the shape gtm-engine hit on 2026-10-17: fields named only
// by id (name-card, card-number, expiration-month, expiration-year, cvv-number,
// address-one, add-city, add-state, add-zip), a month select whose labels read
// "1 - January", and a card input whose own script keeps only 5 characters of a
// value written in one go.
//
// Run: node --test e2e/vault-fill-script.test.mjs
import {test} from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import {chromium} from 'playwright';

const js = fs.readFileSync('crates/amux-server/src/api/vault_fill.js', 'utf8');
const VALUES = {
  number: '4242424242424242', exp: '03/29', exp_month: '03', exp_year: '2029', cvc: '123',
  name: 'Test Buyer', address: '1 Test St', city: 'Testville', state: 'NY', zip: '10001',
  country: 'US', email: 'buyer@example.test',
};
const script = (values) => js.replace('__AMUX_FILL_VALUES__', JSON.stringify(values));

const MONTHS = ['January','February','March','April','May','June','July','August','September','October','November','December'];
const ID_ONLY_FORM = `<form>
  <input id="name-card">
  <input id="card-number">
  <select id="expiration-month"><option value="">Month</option>${MONTHS.map((m, i) => `<option value="${i + 1}">${i + 1} - ${m}</option>`).join('')}</select>
  <select id="expiration-year"><option value="">Year</option>${[2026,2027,2028,2029,2030].map(y => `<option>${y}</option>`).join('')}</select>
  <input id="cvv-number">
  <input id="address-one"><input id="add-city">
  <select id="add-state"><option value="">State</option><option value="CA">California</option><option value="NY">New York</option></select>
  <input id="add-zip">
</form>
<script>
  // A card mask like the one that kept 5 characters: a whole value written at
  // once is cut to 5, typed characters are kept and grouped in fours.
  const cn = document.getElementById('card-number');
  cn.addEventListener('input', (e) => {
    const d = cn.value.replace(/\\D/g, '');
    cn.value = (e.inputType === 'insertText' ? d : d.slice(0, 5)).replace(/(\\d{4})(?=\\d)/g, '$1 ');
  });
</script>`;

async function run(html, values) {
  const browser = await chromium.launch();
  try {
    const page = await browser.newPage();
    await page.setContent(html);
    const r = await page.evaluate(script(values));
    const fields = await page.evaluate(() => Object.fromEntries([...document.querySelectorAll('input,select')].map(el =>
      [el.id || el.name, el.tagName === 'SELECT' ? { index: el.selectedIndex, text: el.options[el.selectedIndex]?.text } : el.value])));
    return { r, fields };
  } finally { await browser.close(); }
}

test('id-only fields, a "1 - January" month select and a masked card number all take their values', async () => {
  const { r, fields } = await run(ID_ONLY_FORM, VALUES);
  assert.deepEqual(r.mismatched, [], JSON.stringify(r.mismatched));
  for (const k of ['number', 'exp_month', 'exp_year', 'cvc', 'name', 'address', 'city', 'state', 'zip']) {
    assert.ok(r.filled.includes(k), `${k} not filled: ${JSON.stringify(r)}`);
  }
  assert.equal(fields['card-number'].replace(/\D/g, ''), VALUES.number, 'the full card number survives the mask');
  assert.equal(fields['expiration-month'].text, '3 - March');
  assert.equal(fields['expiration-year'].text, '2029');
  assert.equal(fields['add-state'].text, 'New York');
  assert.equal(fields['name-card'], 'Test Buyer');
  assert.equal(fields['add-zip'], '10001');
});

test('a page with no payment fields fills nothing, and says so', async () => {
  const { r } = await run('<form><input id="search"><button>Go</button></form>', VALUES);
  assert.deepEqual(r.filled, []);
  assert.ok(r.not_found.includes('number'), JSON.stringify(r));
});

test('a field that does not keep its value is reported with lengths, never the value', async () => {
  // An input that throws away everything, however it is written.
  const html = `<input id="card-number"><script>
    const el = document.getElementById('card-number');
    el.addEventListener('input', () => { el.value = el.value.slice(0, 2); });
  </script><select id="expiration-month"><option value="">Month</option><option value="x">Soon</option></select>`;
  const { r } = await run(html, { number: VALUES.number, exp_month: '03', exp_year: '' });
  const keys = r.mismatched.map(m => m.key).sort();
  assert.deepEqual(keys, ['exp_month', 'number'], JSON.stringify(r));
  const num = r.mismatched.find(m => m.key === 'number');
  assert.equal(num.expected_len, 16);
  assert.equal(num.actual_len, 2);
  assert.equal(r.mismatched.find(m => m.key === 'exp_month').selected_index, 0);
  assert.ok(!JSON.stringify(r).includes(VALUES.number), 'the card number never comes back');
  assert.ok(!r.filled.includes('number'));
});
