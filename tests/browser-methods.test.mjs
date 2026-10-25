// Browser methods: the registry, the ladder and the OpAgent adapter.
//
// The adapter fixtures were produced by OpAgent's own Python
// (opagent_single_model/model_interface_llama_cpp.py, build_prompt and
// parse_model_response) for the inputs below, so these tests fail if the port
// drifts from what the published model was trained to read and write.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';
import * as opagent from '../scripts/browser-agents/opagent-v1.mjs';
import { ladder, pickAgent } from '../scripts/browser-route-driver.mjs';

const here = dirname(fileURLToPath(import.meta.url));
const fixture = (n) => readFileSync(join(here, 'fixtures', n), 'utf8');
const image = { w: 1280, h: 800 };

test('the OpAgent prompt is byte-identical to the published Python', () => {
  const prompt = opagent.buildPrompt({
    task: 'Find the price of the blue mug', image,
    history: [
      { tool: 'click', action: { kind: 'click', x: 640, y: 400 } },
      { tool: 'type', action: { kind: 'type', text: 'blue mug', enter: true, x: 320, y: 80 } },
    ],
  });
  assert.equal(prompt, fixture('opagent-v1-prompt.txt'));
});

test('the OpAgent parser agrees with the published Python on every case', () => {
  const replies = [
    '<think>open search</think><tool_call>{"name": "click", "arguments": {"coords": [500, 250]}}</tool_call>',
    '<think>t</think><tool_call>{"name": "type", "arguments": {"coords": [100, 50], "content": "mug", "press_enter_after": 1}}</tool_call>',
    '{"name": "scroll", "arguments": {"direction": "up", "distance": 450}}',
    '<tool_call>{"name": "answer", "arguments": {"content": "$12"}}</tool_call>',
    'garbage',
  ];
  const py = JSON.parse(fixture('opagent-v1-parse.json'));
  replies.forEach((r, i) => {
    const js = opagent.parse(r, { image });
    const [pyThink, pyCall] = py[i];
    assert.equal(js.think, pyThink, `think, case ${i}`);
    if (pyCall === null) { assert.ok(js.error, `case ${i} has no action`); return; }
    if (pyCall.action_type === 'answer') { assert.deepEqual(js.done, { answer: pyCall.content }); return; }
    const a = js.action;
    if ('x' in pyCall) { assert.equal(a.x, pyCall.x, `x, case ${i}`); assert.equal(a.y, pyCall.y, `y, case ${i}`); }
    if (pyCall.action_type === 'type') { assert.equal(a.kind, 'type'); assert.equal(a.text, pyCall.text); assert.equal(a.enter, pyCall.press_enter); }
    if (pyCall.action_type === 'scroll') { assert.equal(a.kind, 'scroll'); assert.equal(a.dy, pyCall.direction === 'up' ? -pyCall.distance : pyCall.distance); }
  });
});

test('the request is an OpenAI-compatible chat call carrying the screenshot', () => {
  const body = opagent.request({ task: 't', screenshot_b64: 'AAAA', image, history: [], agent: { model: 'OpAgent-32B-Q4', max_tokens: 64 } });
  assert.equal(body.model, 'OpAgent-32B-Q4');
  assert.equal(body.max_tokens, 64);
  const parts = body.messages[0].content;
  assert.equal(parts[0].type, 'text');
  assert.equal(parts[1].image_url.url, 'data:image/png;base64,AAAA');
});

test('an OpenAI response object parses the same as its text', () => {
  const text = '<tool_call>{"name": "go_back", "arguments": {}}</tool_call>';
  assert.deepEqual(opagent.parse({ choices: [{ message: { content: text } }] }, { image }).action, { kind: 'back' });
});

test('the ladder follows the registry, drops unknown backends and keeps CUA behind its switch', () => {
  assert.deepEqual(ladder({ methods: { ladder: ['amux', 'cdp', 'cua'] }, config: { allow_cua: false } }), ['amux', 'cdp']);
  assert.deepEqual(ladder({ methods: { ladder: ['amux', 'cdp', 'cua'] }, config: { allow_cua: true } }), ['amux', 'cdp', 'cua']);
  assert.deepEqual(ladder({ methods: { ladder: ['amux', 'cua', 'nope', 'amux'] }, config: { allow_cua: true } }), ['amux', 'cua'], 'an owner can skip CDP; unknown and repeated names drop');
  assert.deepEqual(ladder({ methods: {}, config: { allow_cua: false } }), ['amux', 'cdp'], 'no registry keeps the historical order');
});

test('an agent is chosen by name, refused when disabled, and the first enabled one is the default', () => {
  const ctx = { methods: { agents: [{ id: 'opagent', enabled: false }, { id: 'other', enabled: true }] } };
  assert.equal(pickAgent(ctx).id, 'other');
  assert.throws(() => pickAgent(ctx, 'opagent'), /disabled/);
  assert.throws(() => pickAgent(ctx, 'missing'), /no browser agent/);
  assert.throws(() => pickAgent({ methods: { agents: [{ id: 'opagent', enabled: false }] } }), /no browser agent is enabled/);
});
