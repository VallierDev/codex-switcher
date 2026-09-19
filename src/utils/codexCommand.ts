export const isWindows = typeof navigator !== 'undefined' && navigator.platform.startsWith('Win');

export function codexLaunchCommand(baseUrl: string, windows = isWindows): string {
    if (windows) return `$env:OPENAI_BASE_URL='${baseUrl.replace(/'/g, "''")}'; codex`;
    return `OPENAI_BASE_URL=${baseUrl} codex`;
}
