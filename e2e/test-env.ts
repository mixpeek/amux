// Shared, deliberately non-secret process prerequisites for the isolated E2E
// servers. Keep values here so a scenario that temporarily overwrites one can
// restore the exact harness baseline instead of leaving later specs in a
// different global state.
export const E2E_ANTHROPIC_KEY = 'sk-ant-e2e-configured-install-not-a-secret';
// The Gemini stub port for the e2e server listening on `serverPort`. Each target
// gets its own, so specs running in parallel projects never share a stub.
export const geminiStubPort = (serverPort: number) => serverPort + 5000;
