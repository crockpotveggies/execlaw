// Contacts page — the curated address book view of /api/admin/principals.
// Shows the trust classes the controller actually thinks of as "people I
// message": KnownTrusted (your circle), KnownLimited (rationed access),
// and UnknownPending (cold contacts waiting for an introduction). System
// principals (controllers, delegated bots, blocked senders) live on the
// Principals page instead.

import { isContact, PrincipalList } from "./PrincipalList";

export function ContactsPage() {
    return (
        <PrincipalList
            testId="settings-contacts"
            heading="Contacts"
            subhead="Your address book — trusted people, limited senders, and pending introductions. Use the Principals page to see system identities and revoked senders."
            emptyHint="No contacts yet. New senders appear here once a transport plugin sees them, and the controller can promote them from the approvals queue."
            filter={isContact}
        />
    );
}
