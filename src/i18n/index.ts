import { russianLocale } from './ru';
import { installUiLocale, type UiLocale } from './runtime';

export interface AppLocaleDefinition {
  code: string;
  languagePrefixes: readonly string[];
  nativeName: string;
  flag: string;
  translation?: UiLocale;
  contributors?: readonly LocaleContributor[];
}

export interface LocaleContributor {
  name: string;
  url?: string;
}

export const SOURCE_LOCALE_CODE = 'zh-CN';
export const AUTO_LOCALE = 'auto';

export const appLocales: readonly AppLocaleDefinition[] = [
  {
    code: SOURCE_LOCALE_CODE,
    languagePrefixes: ['zh'],
    nativeName: '中文',
    flag: '🇨🇳',
  },
  {
    code: 'ru',
    languagePrefixes: ['ru'],
    nativeName: 'Русский',
    flag: '🇷🇺',
    translation: russianLocale,
    contributors: [
      { name: 'Ivan4537', url: 'https://github.com/Ivan4537' },
    ],
  },
];

export type AppLocaleCode = string;
export type LocalePreference = typeof AUTO_LOCALE | AppLocaleCode;

const localePreferenceKey = 'codex-switcher.locale';
let activeLocaleCode = SOURCE_LOCALE_CODE;

function requestedLocale(): string {
  return (navigator.language || SOURCE_LOCALE_CODE).toLowerCase();
}

function findLocale(code: string): AppLocaleDefinition | undefined {
  return appLocales.find(locale => locale.code.toLowerCase() === code.toLowerCase());
}

export function resolveAppLocale(
  language: string,
  preference: LocalePreference = AUTO_LOCALE,
): AppLocaleCode {
  if (preference !== AUTO_LOCALE) {
    const preferred = findLocale(preference);
    if (preferred) return preferred.code;
  }
  const normalized = language.toLowerCase();
  return appLocales.find(locale => (
    locale.languagePrefixes.some(prefix => normalized.startsWith(prefix.toLowerCase()))
  ))?.code ?? SOURCE_LOCALE_CODE;
}

export function getLocalePreference(): LocalePreference {
  try {
    const stored = localStorage.getItem(localePreferenceKey);
    return stored && (stored === AUTO_LOCALE || findLocale(stored)) ? stored : AUTO_LOCALE;
  } catch {
    return AUTO_LOCALE;
  }
}

export function setLocalePreference(preference: LocalePreference) {
  if (preference !== AUTO_LOCALE && !findLocale(preference)) return;
  localStorage.setItem(localePreferenceKey, preference);
  window.location.reload();
}

export function getActiveLocale(): AppLocaleDefinition {
  return findLocale(activeLocaleCode) ?? appLocales[0];
}

// Chinese remains the source language and the fallback for unsupported locales.
export function installAppLocale(systemLocale?: string): AppLocaleCode {
  activeLocaleCode = resolveAppLocale(systemLocale ?? requestedLocale(), getLocalePreference());
  const locale = getActiveLocale();
  if (locale.translation) {
    installUiLocale(locale.translation);
  } else {
    document.documentElement.lang = locale.code;
    document.title = 'Codex Switcher';
  }
  return locale.code;
}
