// Routines (formerly "Tasks") — placeholder page.
//
// The cron-style scheduled-automation feature isn't built yet. The
// sidebar surfaces "Routines" as a top-level destination per the
// locked Phase-6 wireframe (MIGRATION_PLAN §8.2), so a click needs to
// land somewhere honest instead of a dead stub. This page sets the
// expectation: the feature is coming, here's roughly what it looks
// like, and here's where you'll find it once shipped.

export function RoutinesPage() {
    return (
        <div data-testid="settings-routines">
            <h3 className="h6 mb-1">Routines</h3>
            <p className="execlaw-muted small mb-3">
                Recurring automations — cron-shaped jobs the agent runs on
                its own schedule (e.g. "every weekday at 8am, summarise
                overnight emails into the control thread").
            </p>

            <div className="execlaw-card">
                <div className="execlaw-card__title mb-2">
                    <i className="bi bi-cone-striped me-2" aria-hidden />
                    Coming soon
                </div>
                <p className="small mb-2">
                    Routines aren't built yet — they're on the roadmap right
                    after the Phase-8 backend + runner work lands. When they
                    ship you'll be able to:
                </p>
                <ul className="small mb-0">
                    <li>
                        Define a routine in plain language ("every Monday at 9am,
                        check the Stripe dashboard and post a summary").
                    </li>
                    <li>
                        Pick a schedule (cron, every N minutes, or a one-shot
                        future time) and a target thread.
                    </li>
                    <li>
                        See run history, last status, and pause / edit /
                        delete each routine.
                    </li>
                    <li>
                        Routines respect the same trust-class + tool-allowlist
                        rules as a normal turn — no quiet privilege escalation.
                    </li>
                </ul>
            </div>
        </div>
    );
}
