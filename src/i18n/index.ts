import { russianLocale } from './ru';
import { installUiLocale } from './runtime';

const locales = new Map([
  ['ru', russianLocale],
]);

export type AppLocaleCode = 'ru' | 'zh-CN';

function requestedLocale(): string {
  return (navigator.language || 'zh-CN').toLowerCase();
}

export function resolveAppLocale(language: string): AppLocaleCode {
  const normalized = language.toLowerCase();
  return [...locales.keys()].some(code => normalized.startsWith(code)) ? 'ru' : 'zh-CN';
}

// Chinese remains the source language and the fallback for unsupported locales.
export function installAppLocale(systemLocale?: string): AppLocaleCode {
  const language = systemLocale ?? requestedLocale();
  const localeCode = resolveAppLocale(language);
  const locale = localeCode === 'ru' ? russianLocale : undefined;
  if (locale) {
    installUiLocale(locale);
    return localeCode;
  } else {
    document.documentElement.lang = 'zh-CN';
    document.title = 'Codex Switcher';
    return 'zh-CN';
  }
}
