// i18n core — shape mirrors business-website's main.js block at
// `const SUPPORTED_LANGUAGES`. English is implicit: defaults live
// inline in the React code via `t("key.path", "Default English")`,
// and `getTranslatedValue` falls back to the default when the
// language is `en` or the key is missing in the resource bundle.
//
// Locale files (`./locales/<lang>.json`) are lazily code-split via
// dynamic import — only the active language's bundle is fetched.

import i18next from "i18next";
import { useSyncExternalStore } from "react";

export const SUPPORTED_LANGUAGES = [
    "en",
    "es",
    "fr",
    "de",
    "it",
    "nl",
    "pl",
    "pt",
] as const;
export type SupportedLanguage = (typeof SUPPORTED_LANGUAGES)[number];

export const DEFAULT_LANGUAGE: SupportedLanguage = "en";
export const LANGUAGE_STORAGE_KEY = "execlaw.preferred-language";

const localeLoaders: Record<
    Exclude<SupportedLanguage, "en">,
    () => Promise<{ default: Record<string, unknown> }>
> = {
    es: () => import("../locales/es.json"),
    fr: () => import("../locales/fr.json"),
    de: () => import("../locales/de.json"),
    it: () => import("../locales/it.json"),
    nl: () => import("../locales/nl.json"),
    pl: () => import("../locales/pl.json"),
    pt: () => import("../locales/pt.json"),
};

function isSupported(lang: string): lang is SupportedLanguage {
    return (SUPPORTED_LANGUAGES as readonly string[]).includes(lang);
}

export function getInitialLanguage(): SupportedLanguage {
    try {
        const stored = window.localStorage.getItem(LANGUAGE_STORAGE_KEY);
        if (stored && isSupported(stored)) return stored;
    } catch {
        /* localStorage may throw under strict privacy modes */
    }

    const browser = (navigator.language || "").split("-")[0];
    if (browser && isSupported(browser)) return browser;

    return DEFAULT_LANGUAGE;
}

async function loadLanguageResources(language: SupportedLanguage): Promise<void> {
    if (language === DEFAULT_LANGUAGE) return;
    if (i18next.hasResourceBundle(language, "translation")) return;
    // `language !== "en"` is established above, so the indexed access
    // is safe — narrow explicitly so TS doesn't still see "en" in the
    // union after the equality guard.
    const loader = localeLoaders[language as Exclude<SupportedLanguage, "en">];
    if (!loader) return;
    const mod = await loader();
    i18next.addResourceBundle(language, "translation", mod.default, true, true);
}

let bootPromise: Promise<void> | null = null;

export function initializeI18n(): Promise<void> {
    if (bootPromise) return bootPromise;
    bootPromise = (async () => {
        await i18next.init({
            lng: DEFAULT_LANGUAGE,
            fallbackLng: false,
            interpolation: { escapeValue: false },
        });
        await setLanguage(getInitialLanguage());
    })();
    return bootPromise;
}

export async function setLanguage(language: string): Promise<void> {
    const next: SupportedLanguage = isSupported(language)
        ? language
        : DEFAULT_LANGUAGE;
    await loadLanguageResources(next);
    await i18next.changeLanguage(next);
    try {
        window.localStorage.setItem(LANGUAGE_STORAGE_KEY, next);
    } catch {
        /* ignore — same fallback rule as getInitialLanguage */
    }
    if (typeof document !== "undefined") {
        document.documentElement.lang = next;
    }
}

// Pure lookup: returns `defaultValue` when the current language is
// English or when the key is missing. Mirrors the business-website
// `getTranslatedValue` rule so English defaults written inline in the
// JSX are the single source of truth.
export function t(
    key: string,
    defaultValue: string,
    options?: Record<string, unknown>,
): string {
    if (i18next.language === DEFAULT_LANGUAGE) {
        return interpolateDefault(defaultValue, options);
    }
    if (!i18next.exists(key)) {
        return interpolateDefault(defaultValue, options);
    }
    return i18next.t(key, { defaultValue, ...options }) as string;
}

// Minimal {{var}} interpolation for the English-default path, so
// callers can pass the same options to t() regardless of language.
function interpolateDefault(
    template: string,
    options?: Record<string, unknown>,
): string {
    if (!options) return template;
    return template.replace(/\{\{\s*(\w+)\s*\}\}/g, (_, name: string) => {
        const v = options[name];
        return v === undefined || v === null ? "" : String(v);
    });
}

// React glue — useSyncExternalStore wired to i18next's `languageChanged`
// event so any component calling useT() re-renders when the language
// flips.
function subscribeLanguage(cb: () => void): () => void {
    i18next.on("languageChanged", cb);
    return () => {
        i18next.off("languageChanged", cb);
    };
}

function getLanguageSnapshot(): string {
    return i18next.language || DEFAULT_LANGUAGE;
}

export function useCurrentLanguage(): string {
    return useSyncExternalStore(
        subscribeLanguage,
        getLanguageSnapshot,
        getLanguageSnapshot,
    );
}

export function useT(): typeof t {
    useCurrentLanguage();
    return t;
}
