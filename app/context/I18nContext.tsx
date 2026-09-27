import React, { createContext, useContext, useEffect, useState, ReactNode } from 'react';
import { translations, translationsMusic3, Language, TranslationKey } from '../i18n/translations';

interface I18nContextType {
  language: Language;
  setLanguage: (lang: Language) => void;
  t: (key: TranslationKey) => string;
  /** "1 song", "3 песни", "61 песня": the form each language asks for. */
  songCount: (count: number) => string;
  /** Which engine answers: everything the studio shows of itself follows from it. */
  engine: 'yue2' | 'mm3';
}

// One context object for the life of the page: when a strings file changes in
// development, this module is evaluated again, and a second context would leave
// every consumer outside the provider that is already mounted.
const I18nContext: React.Context<I18nContextType | undefined> =
  import.meta.hot?.data.i18nContext ?? createContext<I18nContextType | undefined>(undefined);
if (import.meta.hot) import.meta.hot.data.i18nContext = I18nContext;

export const I18nProvider: React.FC<{ children: ReactNode }> = ({ children }) => {
  const [language, setLanguage] = useState<Language>(() => {
    const stored = localStorage.getItem('language') as Language;
    if (stored === 'zh' || stored === 'en' || stored === 'ja' || stored === 'ko' || stored === 'ru') return stored;
    if (typeof navigator !== 'undefined' && navigator.language?.startsWith('ru')) return 'ru';
    return 'en';
  });

  const handleSetLanguage = (lang: Language) => {
    setLanguage(lang);
    localStorage.setItem('language', lang);
  };

  // The strings follow the engine, because the engine decides which studio
  // this window is: the service names itself on /setup/status, and while it is
  // silent the YuE2 wording stays. A window without a service shows the same
  // either way; a window whose engine was switched is reloaded and asks again.
  const [engine, setEngine] = useState<'yue2' | 'mm3'>('yue2');
  const [engineStrings, setEngineStrings] = useState(translations);
  useEffect(() => {
    let cancelled = false;
    void fetch('/setup/status')
      .then((response) => (response.ok ? response.json() : null))
      .then((status: { engine_id?: string } | null) => {
        if (cancelled) return;
        const music3 = status?.engine_id === 'minimaxmusic-cpp';
        setEngine(music3 ? 'mm3' : 'yue2');
        setEngineStrings(music3 ? translationsMusic3 : translations);
      })
      .catch(() => {});
    return () => { cancelled = true; };
  }, []);

  const t = (key: TranslationKey): string => {
    return engineStrings[language][key] || key;
  };

  const songCount = (count: number): string =>
    t(`songCount_${new Intl.PluralRules(language).select(count)}` as TranslationKey).replace('{count}', String(count));

  return (
    <I18nContext.Provider value={{ language, setLanguage: handleSetLanguage, t, songCount, engine }}>
      {children}
    </I18nContext.Provider>
  );
};

export const useI18n = () => {
  const context = useContext(I18nContext);
  if (!context) {
    throw new Error('useI18n must be used within I18nProvider');
  }
  return context;
};
