// Loaded only by oauth-link-smoke.mjs; no real account, clipboard or OAuth request.
const responses = { get_accounts: [], get_current_account_id: null, get_settings: {}, check_codex_login: false };
const state = window.__compat = { calls: [], denied: [], clipboard: null, copyFailure: false, loginDelay: 0,
  url: 'https://example.invalid/authorize?response_type=code&state=unchanged&code_challenge=' + 'a'.repeat(1800) + '&extra=%25%26%22%3C' };
export async function invoke(command, args = {}) {
  state.calls.push({ command, args });
  if (command === 'start_oauth_login' || command === 'start_antigravity_oauth_login') {
    await new Promise(resolve => setTimeout(resolve, state.loginDelay));
    return state.url;
  }
  if (command === 'copy_to_clipboard') {
    if (state.copyFailure) throw 'Mock clipboard unavailable';
    state.clipboard = args.text;
    return null;
  }
  if (Object.hasOwn(responses, command)) return structuredClone(responses[command]);
  state.denied.push(command);
  throw new Error('MOCK_DENIED: ' + command);
}
const handlers = new Map();
export async function listen(name, callback) {
  const callbacks = handlers.get(name) ?? new Set();
  callbacks.add(callback); handlers.set(name, callbacks);
  return () => callbacks.delete(callback);
}
export async function emit(name, payload) { for (const callback of handlers.get(name) ?? []) callback({ payload }); }
export const open = async () => { throw new Error('Fixture native dialog denied'); };
export const readFile = async () => { throw new Error('Fixture filesystem access denied'); };
