import {test, expect} from './fixtures';

// The browser-test server runs its own tmux server in its throwaway home
// (e2e/serve-head.sh). Inheriting a worker pane's $TMUX made it read every real
// worker's pane and create test sessions in the fleet's tmux server (cpu RCA
// 20261026-100223: 274% CPU in pane captures while e2e runs were going).
test('the e2e server talks to a private tmux server, never the fleet', async ({request}) => {
  // /api/debug/tmux reports the server's tmux environment once tmux answers; on
  // a loaded host the 3s probe can time out, and that answer omits it.
  let d: any = {};
  await expect.poll(async () => {
    d = await (await request.get('/api/debug/tmux')).json();
    return 'env_tmux' in d;
  }, {timeout: 30_000}).toBe(true);
  expect(d.env_tmux, 'no inherited $TMUX').toBeNull();
  expect(d.env_tmux_tmpdir || '', 'TMUX_TMPDIR is inside the throwaway home').toMatch(/amux-e2e-[^/]+\/tmux$/);
});
