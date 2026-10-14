// Model sign-in through the coding CLIs' own logins (docs/design/cloud-self-service.md 8.4).
//
// The dashboard runs a CLI's login command in a server terminal (/api/terminal)
// and reads its output. This module turns that raw terminal output into what a
// person needs: the sign-in link, a device code to type elsewhere, whether the
// CLI is waiting for a pasted code, and (Claude) the token it printed.
//
// Each adapter is checked against recorded output of the CLI version the cloud
// image pins (e2e/fixtures/cli-login, e2e/cli-login.test.mjs), so a CLI update
// that changes its prompts fails a test before it reaches a customer.

const GEMINI_SETTINGS = '{"security":{"auth":{"selectedType":"oauth-personal"}}}';

export const CLI_LOGINS = {
  claude: {
    label: 'Claude',
    // setup-token prints a one-year subscription token instead of storing it; amux saves it.
    command: 'BROWSER=false claude setup-token',
    url: /https:\/\/claude\.(?:com|ai)\/\S*?oauth\/authorize\S*/,
    codePrompt: /Paste\s*code\s*here/i,
    token: /sk-ant-oat01-[A-Za-z0-9_-]{20,}/,
    saveEnv: 'CLAUDE_CODE_OAUTH_TOKEN',
    done: (a) => !!(a && a.claude && (a.claude.subscription_token || a.claude.oauth)),
  },
  codex: {
    label: 'ChatGPT',
    command: 'codex login --device-auth',
    url: /https:\/\/auth\.openai\.com\/codex\/device(?![\w/])/,
    code: /(?<![A-Z0-9])[A-Z0-9]{4}-[A-Z0-9]{4,6}(?![A-Za-z0-9])/,
    done: (a) => !!(a && a.codex && a.codex.signed_in),
  },
  gemini: {
    label: 'Google (Gemini)',
    // Preselect Google login so the CLI prints its link instead of a menu.
    command: `mkdir -p ~/.gemini && ([ -s ~/.gemini/settings.json ] || printf '%s' '${GEMINI_SETTINGS}' > ~/.gemini/settings.json) && NO_BROWSER=true gemini`,
    url: /https:\/\/accounts\.google\.com\/o\/oauth2\/\S+/,
    codePrompt: /Enter\s*the\s*authorization\s*code/i,
    done: (a) => !!(a && a.gemini && a.gemini.signed_in),
  },
};

const OSC8 = /\x1b\]8;[^;\x07\x1b]*;([^\x07\x1b]+)(?:\x07|\x1b\\)/g;
const ANSI = /\x1b\[[0-9;?<>=]*[ -\/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b[@-Z\\-_]/g;

export function stripAnsi(raw) {
  return String(raw || '').replace(ANSI, '').replace(/\r/g, '');
}

// A link the terminal wrapped onto several lines: join while the next line
// continues it (no spaces, no blank line between).
function joinWrapped(text, start) {
  let out = '';
  for (let i = start; i < text.length; i++) {
    const c = text[i];
    if (c === '\n') {
      const next = text[i + 1];
      if (!next || /\s/.test(next)) break;
      continue;
    }
    if (/\s/.test(c)) break;
    out += c;
  }
  return out;
}

export function parseLogin(provider, raw) {
  const a = CLI_LOGINS[provider];
  if (!a) throw new Error('unknown provider ' + provider);
  const text = stripAnsi(raw);
  let url = null;
  for (const m of String(raw || '').matchAll(OSC8)) {        // hyperlinks carry the whole URL in one piece
    if (a.url.test(m[1])) { url = m[1]; break; }
  }
  if (!url) {
    const m = a.url.exec(text);
    if (m) url = joinWrapped(text, m.index);
  }
  const code = a.code ? ((a.code.exec(text) || [])[0] || null) : null;
  const token = a.token ? ((a.token.exec(text) || [])[0] || null) : null;
  return { url, code, needsCode: !!(a.codePrompt && a.codePrompt.test(text.replace(/\s+/g, ' ')) && !token), token };
}
