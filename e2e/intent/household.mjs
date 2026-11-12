// A seeded household for the intent-first acceptance set
// (docs/design/intent-first.md). Ethan's real inbox has no kids, camp or
// school mail, so the nine use cases run on this family instead: Dana, three
// kids (Rafi, Ezra, Noa), a camp waitlist, a school application, conferences,
// a car rental split with Tova, and open enrollment. Dates are relative to
// `now`, so the fixture never goes stale.
//
// The mailbox is a file mailbox (crates/amux-server/src/integrations/local_mailbox.rs):
// read-only, and nothing can be sent from it.
export const ACCOUNT = 'dana@household.test';

const day = 86400000;
const rfc = (d) => new Date(d).toUTCString().replace('GMT', '+0000');
const iso = (d) => new Date(d).toISOString().slice(0, 10);
const at = (d, hh, mm = 0) => { const x = new Date(d); x.setHours(hh, mm, 0, 0); return x; };
const localIso = (d) => { const x = new Date(d); const p = (n) => String(n).padStart(2, '0');
  return `${x.getFullYear()}-${p(x.getMonth() + 1)}-${p(x.getDate())}T${p(x.getHours())}:${p(x.getMinutes())}:00`; };

export function buildHousehold(now = Date.now()) {
  const ago = (n) => now - n * day;
  const ahead = (n) => now + n * day;
  let n = 0;
  const msg = (daysAgo, from, subject, body, thread) => ({
    message_id: `<hh-${++n}@household.test>`, thread_id: thread || `t-${n}`,
    from, to: `Dana Levin <${ACCOUNT}>`, subject, date: rfc(ago(daysAgo)), body, labels: ['INBOX'],
  });
  const messages = [
    // 1. The camp waitlist thread: Dana nudged once, no answer yet.
    msg(24, 'Marcy Feld, Camp Kaylie Director <marcy@campkaylie.test>', 'Rafi Levin - Session 2 waitlist',
      'Hi Dana,\n\nThanks for applying for Rafi for Session 2 (July 6 to July 31). Session 2 is full right now and Rafi is number 4 on the waitlist. Spots usually open up by the end of the month. I will write as soon as I know more.\n\nMarcy Feld\nDirector, Camp Kaylie\n(845) 555-0142', 'camp'),
    { ...msg(12, `Dana Levin <${ACCOUNT}>`, 'Re: Rafi Levin - Session 2 waitlist',
      'Hi Marcy, just checking whether anything has opened up for Rafi in Session 2. Thanks! Dana', 'camp'), to: 'marcy@campkaylie.test', labels: ['SENT'] },
    // 2. Ezra's school application: the deadline mail exists, the confirmation does not.
    msg(20, 'Admissions, Hudson Prep <admissions@hudsonprep.test>', 'Ezra Levin: your 7th grade application is started',
      'Dear Dana,\n\nYou have started an application for Ezra Levin for 7th grade. Applications are complete once the parent statement and the teacher recommendation are submitted. The deadline is ' + iso(ahead(9)) + '. You will receive a confirmation email titled "Application received" once it is complete.\n\nHudson Prep Admissions', 'hp'),
    msg(3, 'Admissions, Hudson Prep <admissions@hudsonprep.test>', 'Reminder: application deadline ' + iso(ahead(9)),
      'This is a reminder that 7th grade applications are due ' + iso(ahead(9)) + '. Our records show Ezra Levin\'s application is missing: parent statement. The teacher recommendation was received.', 'hp'),
    // 3. School forms.
    msg(2, 'Ms. Ortiz, Maple Elementary <ortiz@maple.test>', 'Field trip permission slip due Friday',
      'Dear families, our 3rd grade trip to the Bronx Zoo is next week. Please sign and return the permission slip and $15 by Friday. Rafi still needs his slip.', 'fs'),
    msg(4, 'Maple Elementary PTA <pta@maple.test>', 'Picture day order form',
      'Picture day is ' + iso(ahead(6)) + '. Order forms are due the morning of picture day. Order online or send the paper form.', 'pd'),
    msg(6, 'Lincoln Middle School <office@lincoln.test>', 'Ezra: emergency contact form needs updating',
      'Please update Ezra\'s emergency contact form in the parent portal by ' + iso(ahead(4)) + '.', 'ec'),
    // 5. Conferences: registration, grades, due work.
    msg(5, 'Lincoln Middle School <office@lincoln.test>', 'Parent-teacher conference sign-ups open ' + iso(ahead(2)) + ' at 7pm',
      'Conference sign-ups open ' + iso(ahead(2)) + ' at 7:00 pm and fill fast. Each family may book up to four 10-minute slots. Teachers: Mr. Brandt (Math), Ms. Okafor (English), Mr. Ruiz (Science), Ms. Hale (Social Studies).', 'ptc'),
    msg(9, 'Lincoln Middle School <grades@lincoln.test>', 'Progress report: Ezra Levin',
      'Ezra Levin, grade 6, progress report.\nMath (Mr. Brandt): C+, 3 missing assignments (4.2, 4.3, 4.5).\nEnglish (Ms. Okafor): A-.\nScience (Mr. Ruiz): B, lab report due ' + iso(ahead(3)) + ' not submitted.\nSocial Studies (Ms. Hale): A.', 'grades-e'),
    msg(9, 'Maple Elementary <grades@maple.test>', 'Progress report: Rafi Levin',
      'Rafi Levin, grade 3.\nReading (Ms. Ortiz): approaching grade level, needs support with fluency.\nMath (Ms. Ortiz): meets expectations.\nRafi has 2 reading logs not turned in.', 'grades-r'),
    msg(1, 'Google Classroom <no-reply@classroom.test>', 'Missing work: Ezra Levin - Math 4.5',
      'Ezra has missing work in Math (Mr. Brandt): Worksheet 4.5, due 4 days ago.', 'gc'),
    // 6. The car rental with Tova.
    msg(6, 'Hertz <receipts@hertz.test>', 'Your Hertz receipt: Burlington, VT',
      'Rental agreement 88213. Pickup ' + iso(ago(8)) + ' 10:00 Burlington Airport, return ' + iso(ago(6)) + ' 16:00. Base rate $612.00, taxes and fees $112.40, fuel service $52.00. Total charged to Visa ending 4411: $776.40.', 'hertz'),
    msg(4, 'Venmo <venmo@venmo.test>', 'Tova Ben-David paid you $388.00',
      'Tova Ben-David paid you $388.00. Note: "car rental vermont, half". Deposited to your Venmo balance.', 'venmo'),
    // 8. Open enrollment.
    msg(7, 'Acme HR Benefits <benefits@acme.test>', 'Open enrollment closes ' + iso(ahead(18)),
      'Open enrollment for medical, dental and dependent care FSA is open now and closes ' + iso(ahead(18)) + ' at 5pm. If you do nothing, your current medical plan rolls over but your FSA election does not.', 'oe'),
    // 4. Dinners.
    msg(1, 'Hudson Valley CSA <box@hvcsa.test>', 'This week\'s CSA box',
      'This week: butternut squash, kale, apples, carrots, onions, a dozen eggs. Pickup Thursday.', 'csa'),
    msg(10, 'Noa Levin <noa.levin@household.test>', 'no more meat pls',
      'mom i decided im vegetarian now. fish is ok sometimes', 'noa'),
    // Noise.
    msg(1, 'The Daily Brief <news@brief.test>', 'Your morning brief', 'Markets, weather and three things to know today.', 'news'),
    msg(3, 'Amazon <ship@amazon.test>', 'Your package was delivered', 'Your order of kids\' rain boots was delivered.', 'amz'),
  ];
  const events = [
    { title: 'Rafi soccer practice', start: localIso(at(ahead(1), 9)), end: localIso(at(ahead(1), 10, 30)), location: 'Riverside Park field 3' },
    { title: 'Ezra dentist (Dr. Shah)', start: localIso(at(ahead(3), 15, 30)), end: localIso(at(ahead(3), 16, 15)), location: '120 W 72nd St', description: 'Bring the new insurance card' },
    { title: 'Picture day (Maple)', start: iso(ahead(6)), all_day: 1 },
    { title: 'Conference sign-ups open (Lincoln)', start: localIso(at(ahead(2), 19)), end: localIso(at(ahead(2), 19, 30)) },
    { title: 'Noa piano recital', start: localIso(at(ahead(5), 18)), end: localIso(at(ahead(5), 19, 30)), location: 'Turtle Bay Music School' },
    { title: 'Vermont ski weekend with Tova and Ben', start: iso(ago(8)), end: iso(ago(6)), all_day: 1, description: 'Split the car rental with Tova' },
    { title: 'Hudson Prep application due', start: iso(ahead(9)), all_day: 1 },
  ];
  return { mailbox: { account: ACCOUNT, messages }, events };
}
