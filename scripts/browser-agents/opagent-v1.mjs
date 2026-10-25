// OpAgent single-model adapter for the amux browser route `goal` verb.
//
// Ported from codefuse-ai/OpAgent (Apache-2.0),
// opagent_single_model/model_interface_llama_cpp.py: the same tool list,
// prompt and reply parsing, so a model served for OpAgent behaves here as it
// does in OpAgent's own loop. https://github.com/codefuse-ai/OpAgent
//
// An adapter is two pure functions over plain data, so it can be tested
// without a browser or a model:
//   request(step) -> the JSON body for the agent's endpoint
//   parse(reply, step) -> {think, action} | {think, done: {answer}} | {error}
// `step` carries {task, screenshot_b64, image: {w, h}, url, history, agent}.
// Actions use amux's neutral vocabulary in SCREENSHOT pixels; the shared loop
// scales them to the page:
//   click{x,y} double_click{x,y} hover{x,y} type{text,enter,x?,y?}
//   key{key,x?,y?} scroll{dx,dy} goto{url} back forward wait{seconds}
export const protocol = 'opagent-v1';

export const TOOL_LIST = [
  {
    "name": "click",
    "description": "Click on an element with coordinates on the screenshot of the webpage.",
    "parameters": {
      "type": "object",
      "properties": {
        "coords": {
          "type": "list",
          "description": "The coordinates of the element in the image to click: [x,y]"
        }
      },
      "required": [
        "coords"
      ]
    }
  },
  {
    "name": "type",
    "description": "Type content into a field with a specific id.",
    "parameters": {
      "type": "object",
      "properties": {
        "coords": {
          "type": "list",
          "description": "The coordinates of the element in the image to click: [x,y]"
        },
        "content": {
          "type": "string",
          "description": "Text to be typed"
        },
        "press_enter_after": {
          "type": "integer",
          "description": "Whether to press Enter after typing (1 by default, 0 to disable)",
          "default": 0
        }
      },
      "required": [
        "coords",
        "content"
      ]
    }
  },
  {
    "name": "hover",
    "description": "Hover over an element with the coordinates",
    "parameters": {
      "type": "object",
      "properties": {
        "coords": {
          "type": "list",
          "description": "The coordinates of the element in the image to hover: [x,y]"
        }
      },
      "required": [
        "coords"
      ]
    }
  },
  {
    "name": "press",
    "description": "Simulate pressing a key or a key combination",
    "parameters": {
      "type": "object",
      "properties": {
        "coords": {
          "type": "list",
          "description": "The coordinates of the element in the image to hover: [x,y]"
        },
        "key": {
          "type": "string",
          "description": "a key or a key combination to press (e.g., 'ctrl+v' or 'enter')"
        }
      },
      "required": [
        "key"
      ]
    }
  },
  {
    "name": "scroll",
    "description": "Scroll the page up or down",
    "parameters": {
      "type": "object",
      "properties": {
        "direction": {
          "type": "string",
          "enum": [
            "up",
            "down"
          ],
          "description": "Direction to scroll"
        },
        "distance": {
          "type": "integer",
          "description": "The scroll distance"
        }
      },
      "required": [
        "direction",
        "distance"
      ]
    }
  },
  {
    "name": "hscroll",
    "description": "Scroll the page horizontally",
    "parameters": {
      "type": "object",
      "properties": {
        "direction": {
          "type": "string",
          "enum": [
            "left",
            "right"
          ],
          "description": "Direction to scroll horizontally"
        },
        "distance": {
          "type": "integer",
          "description": "The scroll distance"
        }
      },
      "required": [
        "direction",
        "distance"
      ]
    }
  },
  {
    "name": "new_tab",
    "description": "Open a new, empty browser tab",
    "parameters": {
      "type": "object",
      "properties": {},
      "required": []
    }
  },
  {
    "name": "tab_focus",
    "description": "Switch browser focus to a specific tab",
    "parameters": {
      "type": "object",
      "properties": {
        "tab_index": {
          "type": "integer",
          "description": "Index of the tab to focus"
        }
      },
      "required": [
        "tab_index"
      ]
    }
  },
  {
    "name": "go_back",
    "description": "Navigate to the previously viewed page",
    "parameters": {
      "type": "object",
      "properties": {},
      "required": []
    }
  },
  {
    "name": "go_forward",
    "description": "Navigate to the next page after a 'go_back' action",
    "parameters": {
      "type": "object",
      "properties": {},
      "required": []
    }
  },
  {
    "name": "move_to",
    "description": "Move the cursor to a specific location without clicking",
    "parameters": {
      "type": "object",
      "properties": {
        "coords": {
          "type": "list",
          "description": "The coordinates to move the cursor to: [x,y]"
        }
      },
      "required": [
        "coords"
      ]
    }
  },
  {
    "name": "double_click",
    "description": "Perform a double click at a specific location",
    "parameters": {
      "type": "object",
      "properties": {
        "coords": {
          "type": "list",
          "description": "The coordinates to double click: [x,y]"
        }
      },
      "required": [
        "coords"
      ]
    }
  },
  {
    "name": "goto",
    "description": "Navigate to a specific URL",
    "parameters": {
      "type": "object",
      "properties": {
        "url": {
          "type": "string",
          "description": "The URL to navigate to"
        }
      },
      "required": [
        "url"
      ]
    }
  },
  {
    "name": "wait",
    "description": "Wait for the change to happen",
    "parameters": {
      "type": "object",
      "properties": {
        "seconds": {
          "type": "integer",
          "description": "The seconds to wait"
        }
      },
      "required": [
        "seconds"
      ]
    }
  },
  {
    "name": "answer",
    "description": "Task is completed, provide the final answer",
    "parameters": {
      "type": "object",
      "properties": {
        "content": {
          "type": "string",
          "description": "The final answer or result of the task"
        }
      },
      "required": [
        "content"
      ]
    }
  }
];

const TERMINAL = new Set(['answer', 'stop', 'finish', 'done']);

// Python's json.dumps(ensure_ascii=False) spacing (", " and ": "), which is
// what OpAgent's own loop puts in the history the model was trained on.
export function pyJson(v) {
  if (Array.isArray(v)) return '[' + v.map(pyJson).join(', ') + ']';
  if (v && typeof v === 'object') return '{' + Object.entries(v).map(([k, x]) => JSON.stringify(k) + ': ' + pyJson(x)).join(', ') + '}';
  return JSON.stringify(v);
}

function historyBlock(item, i, image) {
  const a = item.action || {};
  const args = {};
  if (Number.isFinite(a.x) && Number.isFinite(a.y) && image.w && image.h)
    args.coords = [Math.trunc(a.x / image.w * 1000), Math.trunc(a.y / image.h * 1000)];
  if (a.kind === 'type') { args.content = a.text || ''; args.press_enter_after = a.enter ? 1 : 0; }
  else if (a.kind === 'scroll') { args.direction = a.dy < 0 ? 'up' : a.dy > 0 ? 'down' : a.dx < 0 ? 'left' : 'right'; args.distance = Math.abs(a.dy || a.dx || 300); }
  else if (a.kind === 'key') args.key = a.key || 'Enter';
  else if (a.kind === 'wait') args.seconds = a.seconds || 2;
  else if (a.kind === 'goto') args.url = a.url || '';
  const name = item.tool || ({back: 'go_back', forward: 'go_forward', key: 'press'}[a.kind] || a.kind || '');
  return `Step ${i + 1}:\n<think>Executing ${name} action</think><tool_call>${pyJson({name, arguments: args})}</tool_call>`;
}

export function buildPrompt(step) {
  const n = Number.isInteger(step.agent?.history_steps) ? step.agent.history_steps : 2;
  const hist = (step.history || []).slice(-n).map((h, i) => historyBlock(h, i, step.image || {}));
  return [
    "You are an excellent web agent. Based on the web screen shot and content, your need call the single, most appropriate tool for the current step to make progress on the user's request.",
    'Output the thinking process in <think> </think> tags, and for each function call, return a json object with function name and arguments within <tool_call></tool_call> XML tags as follows:',
    '<think> ... </think><tool_call>{"name": <function-name>, "arguments": <args-json-object>}</tool_call>',
    'You are provided with function signatures within <tools></tools> XML tags:',
    `<tools>\n${JSON.stringify(TOOL_LIST, null, 2)}\n</tools>`,
    '<image>',
    'Please generate the next move according to the UI screenshot, instruction and previous actions.',
    `Instruction: '${step.task}'.`,
    ...(hist.length ? hist : ['None']),
    `Step ${hist.length + 1}:`,
    "NOTES: 1): If you see a search bar, simply use the type tool to input your query. There's no need to click the search bar first.",
  ].join('\n');
}

export function request(step) {
  const agent = step.agent || {};
  return {
    model: agent.model || 'qwen-vl',
    messages: [{ role: 'user', content: [
      { type: 'text', text: buildPrompt(step) },
      { type: 'image_url', image_url: { url: `data:image/png;base64,${step.screenshot_b64}` } },
    ] }],
    temperature: agent.temperature ?? 0,
    max_tokens: agent.max_tokens ?? 512,
  };
}

function balancedObjects(text) {
  const out = []; let depth = 0, start = -1;
  for (let i = 0; i < text.length; i++) {
    if (text[i] === '{') { if (depth === 0) start = i; depth++; }
    else if (text[i] === '}') { depth--; if (depth === 0 && start !== -1) { out.push(text.slice(start, i + 1)); start = -1; } }
  }
  return out;
}

function toAction(call, image) {
  const name = String(call.name || '').trim();
  const args = call.arguments || {};
  if (TERMINAL.has(name)) return { done: { answer: String(args.content ?? '') } };
  let x, y;
  const c = args.coords;
  if (Array.isArray(c) && c.length >= 2 && Number.isFinite(+c[0]) && Number.isFinite(+c[1])) {
    const cx = +c[0], cy = +c[1];
    // OpAgent normalises to a 0-1000 grid over the screenshot; out-of-range
    // values are treated as pixels, as the original parser does.
    if (cx >= 0 && cx <= 1000 && cy >= 0 && cy <= 1000) { x = Math.round(cx / 1000 * image.w); y = Math.round(cy / 1000 * image.h); }
    else { x = Math.round(cx); y = Math.round(cy); }
  }
  const at = (o) => (x === undefined ? o : { ...o, x, y });
  switch (name) {
    case 'click': return x === undefined ? { error: 'click without coords' } : { action: { kind: 'click', x, y } };
    case 'double_click': return x === undefined ? { error: 'double_click without coords' } : { action: { kind: 'double_click', x, y } };
    case 'hover': case 'move_to': return x === undefined ? { error: name + ' without coords' } : { action: { kind: 'hover', x, y } };
    case 'type': return { action: at({ kind: 'type', text: String(args.content ?? ''), enter: +args.press_enter_after === 1 || args.press_enter_after === true }) };
    case 'press': return { action: at({ kind: 'key', key: String(args.key || 'Enter') }) };
    case 'scroll': case 'hscroll': {
      const d = Math.abs(+args.distance || +args.pixel_amount || 300); const dir = String(args.direction || 'down');
      return { action: { kind: 'scroll', dx: dir === 'right' ? d : dir === 'left' ? -d : 0, dy: dir === 'down' ? d : dir === 'up' ? -d : 0 } };
    }
    case 'goto': return args.url ? { action: { kind: 'goto', url: String(args.url) } } : { error: 'goto without url' };
    case 'go_back': return { action: { kind: 'back' } };
    case 'go_forward': return { action: { kind: 'forward' } };
    case 'wait': return { action: { kind: 'wait', seconds: Math.min(30, Math.max(0, +args.seconds || 2)) } };
    case 'new_tab': case 'tab_focus': return { error: `${name} is not supported on an amux route (one tab per session)` };
    default: return { error: `unknown tool ${JSON.stringify(name)}` };
  }
}

export function parse(reply, step) {
  const image = step.image || { w: 1280, h: 800 };
  const text = typeof reply === 'string' ? reply : (reply?.choices?.[0]?.message?.content ?? '');
  const clean = String(text).replace(/<\|im_start\|>\w+\n?|<\|im_end\|>/g, '').trim();
  let think = null;
  for (const re of [/<think>([\s\S]*?)<\/think>/, /\[Start thinking\]([\s\S]*?)\[End thinking\]/]) {
    const m = clean.match(re); if (m) { think = m[1].trim(); break; }
  }
  const candidates = [];
  const tc = clean.match(/<tool_call>([\s\S]*?)<\/tool_call>/);
  if (tc) candidates.push(tc[1].trim());
  candidates.push(...balancedObjects(clean));
  candidates.push(...(clean.match(/\{[^\n]+\}/g) || []));
  for (const cand of candidates) {
    let call; try { call = JSON.parse(cand); } catch { continue; }
    if (!call || typeof call !== 'object' || !call.name) continue;
    return { think, tool: String(call.name), ...toAction(call, image) };
  }
  return { think, error: 'no tool call in the model reply' };
}
