import { Check } from 'lucide-react';
import { useEffect, useRef, useState } from 'react';
import {
  AUTO_LOCALE,
  appLocales,
  getActiveLocale,
  getLocalePreference,
  setLocalePreference,
} from '../i18n';

export function LanguagePicker() {
  const [open, setOpen] = useState(false);
  const pickerRef = useRef<HTMLDivElement>(null);
  const triggerRef = useRef<HTMLButtonElement>(null);
  const activeLocale = getActiveLocale();
  const preference = getLocalePreference();

  useEffect(() => {
    if (!open) return undefined;

    const handlePointerDown = (event: PointerEvent) => {
      if (pickerRef.current && !pickerRef.current.contains(event.target as Node)) {
        setOpen(false);
      }
    };
    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key === 'Escape') {
        setOpen(false);
        triggerRef.current?.focus();
      }
    };

    document.addEventListener('pointerdown', handlePointerDown);
    document.addEventListener('keydown', handleKeyDown);
    return () => {
      document.removeEventListener('pointerdown', handlePointerDown);
      document.removeEventListener('keydown', handleKeyDown);
    };
  }, [open]);

  return (
    <div ref={pickerRef} className={`language-picker${open ? ' open' : ''}`}>
      <button
        ref={triggerRef}
        type="button"
        className="language-picker-trigger"
        title="切换语言"
        aria-label="切换语言"
        aria-expanded={open}
        aria-haspopup="menu"
        onClick={() => setOpen(value => !value)}
      >
        <span aria-hidden="true">{activeLocale.flag}</span>
      </button>
      {open && <div className="language-picker-menu" role="menu">
        {appLocales.map(locale => {
          const isSelected = preference === locale.code || (
            preference === AUTO_LOCALE && activeLocale.code === locale.code
          );
          return (
            <button
              type="button"
              key={locale.code}
              className={isSelected ? 'selected' : ''}
              role="menuitem"
              onClick={() => setLocalePreference(locale.code)}
            >
              <span aria-hidden="true">{locale.flag}</span>
              <span lang={locale.code}>{locale.nativeName}</span>
              {isSelected && <Check size={14} aria-hidden="true" />}
            </button>
          );
        })}
      </div>}
    </div>
  );
}
