#!/usr/bin/env bash
# The dashboard's send-time stamp must be applied EXACTLY ONCE (AF-360).
#
# WHY THIS EXISTS. `sendToSession` prefixed every non-slash message with
# `[HH:MM AM]` and had no guard against text that already carried one. Measured
# live on MSG-35221, whose stored text is:
#
#   "[10:00 AM] [11:32 PM] where are we at with all of the items from the ingestion MD?"
#
# Re-sending is ordinary — Ethan re-asks by pasting the text back out of the
# transcript, and the offline queue replays an already-built payload — so this
# was reachable on the normal path, not an edge case.
#
# The consequence is not cosmetic. The reader strips a SINGLE leading stamp
# (`strip_context_wrapper`, crates/amux-server/src/opencode/events.rs), so the
# survivor is a 10.5-hour-stale timestamp that reaches the model looking exactly
# like a real one. The lane is told a time that is not when the message was sent
# and has no way to tell which of the two is now.
#
# It drives the SHIPPED functions extracted from app.js, never a retyped copy:
# a copy passes forever while the real one rots, which is the failure this file
# exists to prevent (ethos rule 7).
set -uo pipefail
cd "$(dirname "$0")/.."
APP=crates/amux-dashboard/static/app.js
PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); echo "  ok   $1"; }
bad() { FAIL=$((FAIL+1)); echo "  FAIL $1"; }

command -v node >/dev/null 2>&1 || { echo "SKIP: node not installed"; exit 2; }

echo "== send-time stamp cells =="

# Extract the two shipped functions by name. If either is renamed or deleted this
# extraction fails loudly rather than silently testing nothing.
if ! node -e '
const fs = require("fs");
const src = fs.readFileSync("'"$APP"'", "utf8");
function grab(name) {
  const i = src.indexOf("function " + name + "(");
  if (i < 0) throw new Error("cannot find function " + name + " in app.js");
  // brace-match from the first { after the signature
  let j = src.indexOf("{", i), depth = 0, k = j;
  for (; k < src.length; k++) {
    if (src[k] === "{") depth++;
    else if (src[k] === "}") { depth--; if (depth === 0) break; }
  }
  return src.slice(i, k + 1);
}
eval(grab("_hasSendTimeStamp"));
eval(grab("_stampSendTime"));

const NOW = new Date(2026, 7, 31, 10, 0, 0);          // 10:00 AM local
const fail = [];
const t = (name, cond) => { if (!cond) fail.push(name); };

// 1. THE BUG. Text already carrying a stamp must come back UNCHANGED, not
//    restamped: the first stamp is the one that was true when a human typed it.
const already = "[11:32 PM] where are we at with all of the items from the ingestion MD?";
t("already-stamped text is returned unchanged", _stampSendTime(already, NOW, null) === already);

// 2. THE EXACT MEASURED SPECIMEN must not be reproducible.
t("MSG-35221 nesting cannot recur",
  !/^\[\d{1,2}:\d{2}[^\]]*\]\s*\[\d{1,2}:\d{2}/.test(_stampSendTime(already, NOW, null)));

// 3. THE CONTROL, and it is load-bearing: a plain message must still BE
//    stamped. Without this, a guard that returned every input unchanged — i.e.
//    a stamp that never fires — passes cells 1 and 2 completely clean.
const plain = "where are we at with all of the items from the ingestion MD?";
t("an unstamped message still gets stamped", /^\[\d{1,2}:\d{2}\s*[AP]M\]\s/i.test(_stampSendTime(plain, NOW, null)));
t("the stamped message still contains the original text", _stampSendTime(plain, NOW, null).endsWith(plain));

// 4. A BRACKET IS NOT A STAMP. "[urgent] ship this" is an ordinary message and
//    must still be stamped — the discriminator is a clock time, not a bracket.
//    A guard keyed on "starts with [" would silently stop stamping these.
t("a non-time bracket is not mistaken for a stamp", !_hasSendTimeStamp("[urgent] ship this"));
t("a non-time bracket message is still stamped", _stampSendTime("[urgent] ship this", NOW, null) !== "[urgent] ship this");

// 5. THE AUTHOR VARIANT. Cloud sends stamp `[HH:MM email]`, so the detector has
//    to recognise the shape this client actually writes, not just the bare time.
const withAuthor = _stampSendTime(plain, NOW, "someone@example.com");
t("author is included when present", withAuthor.indexOf("someone@example.com") > 0);
t("an author-stamped message is recognised as stamped", _hasSendTimeStamp(withAuthor));
t("an author-stamped message is not restamped", _stampSendTime(withAuthor, NOW, "someone@example.com") === withAuthor);

// 6. 24-HOUR LOCALE. toLocaleTimeString drops AM/PM there, and a stamp has to be
//    recognised in the same locale that wrote it or the guard silently stops
//    working for those users only.
t("a 24-hour stamp is recognised", _hasSendTimeStamp("[22:04] do the thing"));

if (fail.length) { console.log("FAILED:" + fail.join("|")); process.exit(1); }
console.log("ALLOK");
' > /tmp/_sts.out 2>&1; then
  echo "  extraction or assertion error:"; sed 's/^/    /' /tmp/_sts.out; FAIL=$((FAIL+1))
else
  out=$(cat /tmp/_sts.out)
  if [ "$out" = "ALLOK" ]; then
    for c in "already-stamped text is returned unchanged" \
             "MSG-35221 nesting cannot recur" \
             "an unstamped message still gets stamped" \
             "the stamped message still contains the original text" \
             "a non-time bracket is not mistaken for a stamp" \
             "a non-time bracket message is still stamped" \
             "author is included when present" \
             "an author-stamped message is recognised as stamped" \
             "an author-stamped message is not restamped" \
             "a 24-hour stamp is recognised"; do ok "$c"; done
  else
    echo "$out" | sed 's/^/    /'; FAIL=$((FAIL+1))
  fi
fi

echo
echo "  $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
