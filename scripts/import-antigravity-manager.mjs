// Read-only by default. --apply imports missing Google accounts into the configured
// Mini Server using its authenticated API; never writes credentials to the Client.
import { readFile } from 'node:fs/promises';
import { homedir } from 'node:os';
import { join } from 'node:path';

const apply = process.argv.includes('--apply');
const root = join(homedir(), '.antigravity_tools');
const local = JSON.parse(await readFile(join(homedir(), '.codex-switcher/accounts.json'), 'utf8'));
if (local.settings.remote_mode !== 'client') throw new Error('Import requires Client mode with a configured Mini Server');
const base = local.settings.remote_server_url;
const endpoint = new URL(base);
if (!['192.168.2.14', '172.26.96.198'].includes(endpoint.hostname)) throw new Error('Unexpected Mini Server destination');
const headers = { 'X-Auth-Token': local.settings.remote_shared_secret, 'Content-Type': 'application/json' };
async function api(path, body) {
    const response = await fetch(`${base.replace(/\/$/, '')}${path}`, {
        method: body === undefined ? 'GET' : 'POST', headers,
        body: body === undefined ? undefined : JSON.stringify(body), signal: AbortSignal.timeout(60000),
    });
    // Never dump server responses: account endpoints can contain credentials.
    if (!response.ok) throw new Error(`${path} returned HTTP ${response.status}`);
    return response.json();
}
const index = JSON.parse(await readFile(join(root, 'accounts.json'), 'utf8'));
const source = await Promise.all(index.accounts.map(async item => {
    if (!/^[0-9a-f-]{36}$/i.test(item.id)) throw new Error('Invalid source account ID');
    return JSON.parse(await readFile(join(root, 'accounts', `${item.id}.json`), 'utf8'));
}));
const remote = (await api('/accounts')).accounts;
const googleEmail = account => (account.auth_json?.email || account.name || '').trim().toLowerCase();
const existing = new Set(remote.filter(a => a.kind === 'antigravity_oauth').map(googleEmail));
const ids = new Set(remote.map(a => a.id));
const results = [];
for (const item of source) {
    const email = item.email?.trim();
    if (!email?.includes('@')) throw new Error('Source account has no valid email');
    if (existing.has(email.toLowerCase())) { results.push({ email, result: 'already_present' }); continue; }
    if (ids.has(item.id)) throw new Error(`Account ID collision for ${email}`);
    if (!item.token?.refresh_token || !item.token?.project_id) throw new Error(`Missing Google credentials/project for ${email}`);
    if (item.token.oauth_client_key !== 'antigravity_enterprise') throw new Error(`Unverified OAuth client for ${email}`);
    if (!apply) { results.push({ email, result: 'would_import' }); continue; }
    const account = {
        id: item.id, name: email, kind: 'antigravity_oauth',
        created_at: new Date().toISOString(), last_used: null,
        notes: 'Imported from local Antigravity-Manager; original account retained.',
        refresh_token: item.token.refresh_token,
        keepalive: { inactive_refresh_enabled: false, last_attempt_at: null, last_success_at: null, last_error: null },
        is_banned: !!(item.disabled || item.proxy_disabled || item.validation_blocked || item.quota?.is_forbidden),
        is_logged_out: false, is_token_invalid: false,
        auth_json: {
            provider: 'antigravity', email, project_id: item.token.project_id,
            source: 'antigravity-manager', oauth_client_key: item.token.oauth_client_key,
            subscription_tier: item.quota?.subscription_tier || null,
            tokens: {
                access_token: item.token.access_token || '', refresh_token: item.token.refresh_token,
                expires_at: new Date((item.token.expiry_timestamp || 0) * 1000).toISOString(),
            },
        },
    };
    const created = await api('/accounts', { account });
    if (!created.ok || created.upserted !== 'created' || created.id !== item.id) throw new Error(`Unexpected import outcome for ${email}`);
    existing.add(email.toLowerCase()); ids.add(item.id);
    let modelCount = 0;
    let quotaStatus = account.is_banned ? 'source_disabled' : 'pending';
    if (!account.is_banned) {
        try {
            const quota = await api(`/accounts/${item.id}/antigravity-quota`, {});
            modelCount = Object.keys(quota.model_quotas || {}).length;
            quotaStatus = modelCount ? 'verified' : 'empty';
        } catch (error) { quotaStatus = error.message; }
    }
    const result = { email, result: 'imported', modelCount, quotaStatus };
    results.push(result); console.log(JSON.stringify(result));
}
const after = (await api('/accounts')).accounts;
console.log(JSON.stringify({ apply, sourceCount: source.length, results,
    googleAccountsAfter: after.filter(a => a.kind === 'antigravity_oauth').length,
    codexAccountIdsUnchanged: JSON.stringify(remote.filter(a => a.kind !== 'antigravity_oauth').map(a => a.id).sort()) === JSON.stringify(after.filter(a => a.kind !== 'antigravity_oauth').map(a => a.id).sort()),
}));
