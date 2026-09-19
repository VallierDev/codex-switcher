const platform = typeof navigator === 'undefined' ? '' : navigator.platform;
export const isMacOS = platform.startsWith('Mac');
