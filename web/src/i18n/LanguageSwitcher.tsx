// Compact language dropdown. Visually pairs with the bootstrap-icons
// globe glyph (matching business-website's `.language-switcher`
// pattern) and floats top-right of the surrounding shell — see
// `.execlaw-language-switcher` in `theme.scss` for positioning.
// Used by the pre-auth surfaces (Login + SetupWizard); the in-app
// equivalent lives inline in Settings → General.

import {
    useCurrentLanguage,
    setLanguage,
    LANGUAGE_OPTIONS,
} from "./index";

export function LanguageSwitcher({ className }: { className?: string }) {
    const current = useCurrentLanguage();
    const short =
        LANGUAGE_OPTIONS.find((o) => o.value === current)?.short ??
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
                {LANGUAGE_OPTIONS.map((o) => (
                    <option key={o.value} value={o.value} data-short={o.short}>
                        {o.label}
                    </option>
                ))}
            </select>
        </div>
    );
}
