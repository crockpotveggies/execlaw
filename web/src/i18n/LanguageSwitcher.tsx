// Compact language dropdown shown on the pre-auth surfaces (Login +
// SetupWizard). The in-app equivalent lives inline in Settings →
// General. Uses react-bootstrap `Dropdown` so the menu inherits the
// global `.dropdown-menu` theme tokens (`bg-surface`, `shadow-soft`)
// defined in `theme.scss` instead of the native OS dropdown chrome.

import Dropdown from "react-bootstrap/Dropdown";
import {
    useCurrentLanguage,
    setLanguage,
    LANGUAGE_OPTIONS,
} from "./index";

export function LanguageSwitcher({ className }: { className?: string }) {
    const current = useCurrentLanguage();
    const currentOpt = LANGUAGE_OPTIONS.find((o) => o.value === current);
    const short = currentOpt?.short ?? current.toUpperCase();

    return (
        <Dropdown
            align="end"
            className={
                "execlaw-language-switcher" +
                (className ? " " + className : "")
            }
            data-testid="language-switcher"
        >
            <Dropdown.Toggle
                variant="link"
                className="execlaw-language-switcher__toggle"
                aria-label="Language"
                data-testid="language-switcher-toggle"
            >
                <i
                    className="bi bi-globe2 execlaw-language-switcher__icon"
                    aria-hidden
                />
                <span className="execlaw-language-switcher__value">
                    {short}
                </span>
            </Dropdown.Toggle>
            <Dropdown.Menu
                className="execlaw-language-switcher__menu"
                data-testid="language-switcher-menu"
            >
                {LANGUAGE_OPTIONS.map((o) => (
                    <Dropdown.Item
                        key={o.value}
                        active={o.value === current}
                        onClick={() => {
                            void setLanguage(o.value);
                        }}
                        data-testid={`language-switcher-item-${o.value}`}
                    >
                        <span className="execlaw-language-switcher__item-short">
                            {o.short}
                        </span>
                        <span className="execlaw-language-switcher__item-label">
                            {o.label}
                        </span>
                    </Dropdown.Item>
                ))}
            </Dropdown.Menu>
        </Dropdown>
    );
}
