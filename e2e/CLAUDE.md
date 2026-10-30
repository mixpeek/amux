<!-- rules: parent=../CLAUDE.md owner=amux-vision enforces=A4 -->
# e2e (Playwright)

- Run against this checkout's dashboard and a server you built:
  `AMUX_DASHBOARD_DIR=$PWD/crates/amux-dashboard/static AMUX_E2E_SERVER_BIN=<binary>
  npx playwright test -c e2e/playwright.config.ts <specs> --project=desktop --project=mobile
  --project=ios-safari`.
- `AMUX_E2E_PORT_OFFSET` ports are shared by every lane on this Mac: an `AddrInUse` can be a
  peer's run. Change the offset; never kill a process you did not start.
- A spec that routes an endpoint it may not call marks it `allowUnusedRoute`. Fixtures are
  removed through `cleanup()` (`teardown.ts`), and a spec that seeds server data names it per
  project, since projects run concurrently against one server.
- A beacon sent through the service worker never reaches `page.route`; read the server's
  `/api/client-debug` when a spec needs it.
- Test what the person sees: the control, both themes, the phone at 375px, and after a
  re-render (`crates/amux-dashboard/CLAUDE.md`).
