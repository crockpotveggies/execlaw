// Principals page — the "everything else" view of /api/admin/principals
// that complements the Contacts page. Lists controllers, delegated bots,
// blocked senders, and any principal whose trust class doesn't fit the
// curated address book.

import { isPrincipalOnly, PrincipalList } from "./PrincipalList";

export function PrincipalsPage() {
    return (
        <PrincipalList
            testId="settings-principals"
            heading="Principals"
            subhead="System-level identities and anyone outside the address book — controllers, delegated bots, and revoked senders. Use the Contacts page for trusted humans and pending introductions."
            emptyHint="No system principals on file yet. They'll appear here as the controller and delegated bots register, or as senders are revoked."
            filter={isPrincipalOnly}
        />
    );
}
