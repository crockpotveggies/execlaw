// Compact language dropdown. Visually pairs with the bootstrap-icons
// globe glyph (matching business-website's `.language-switcher`
// pattern) and floats top-right of the surrounding shell — see
// `.execlaw-language-switcher` in `theme.scss` for positioning.

import { useCurrentLanguage, setLanguage, SUPPORTED_LANGUAGES } from "./index";

interface Option {
    value: (typeof SUPPORTED_LANGUAGES)[number];
    label: string;
    short: string;
}

const OPTIONS: Option[] = [
    { value: "en", label: "English", short: "EN" },
    { value: "es", label: "Español", short: "ES" },
    { value: "fr", label: "Français", short: "FR" },
    { value: "de", label: "Deutsch", short: "DE" },
    { value: "it", label: "Italiano", short: "IT" },
    { value: "nl", label: "Nederlands", short: "NL" },
    { value: "pl", label: "Polski", short: "PL" },
    { value: "pt", label: "Português", short: "PT" },
];

export function LanguageSwitcher({ className }: { className?: string }) {
    const current = useCurrentLanguage();
    const short =
        OPTIONS.find((o) => o.value === current)?.short ??
        current.toUpperCase();

    return (
        <div
            className={
                "execlaw-language-switcher" +
                (className ? " " + className : "")
            }
            data-testid="language-switcher"
        >
            <span className="execlaw-language-switcher__value" aria-hidden>
                {short}
            </span>
            <i
                className="bi bi-globe2 execlaw-language-switcher__icon"
                aria-hidden
            />
            <select
                className="form-select execlaw-language-switcher__select"
                aria-label="Language"
                value={current}
                onChange={(e) => {
                    void setLanguage(e.target.value);
                }}
                data-testid="language-switcher-select"
            >
                {OPTIONS.map((o) => (
                    <option key={o.value} value={o.value} data-short={o.short}>
                        {o.label}
                    </option>
                ))}
            </select>
        </div>
    );
}
