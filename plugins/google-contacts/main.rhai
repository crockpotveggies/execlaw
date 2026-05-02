// google-contacts — Google People-API-backed identity provider.
//
// Two host-dispatched entry points:
//
//   identity_resolve(transport, handle, oauth)
//     -> #{ "match": IdentityMatch | () }
//
//   tool_call(name, args, oauth)
//     -> #{ "contacts": [...], ... } | error string for unknown tool
//
// `oauth` is a map of {account_name: access_token} the host
// already injected from state_oauth_tokens. We never see the
// refresh_token or client_secret — those stay on the server,
// rotated by the OAuth sweeper.
//
// All HTTP responses are cached for 5 minutes per (url, query,
// bearer) tuple via the host's http_get_cached primitive — a token
// rotation invalidates the cache automatically because the bearer
// is part of the cache key.
//
// Note on Rhai scope: declared functions can't capture top-level
// `const` declarations from the surrounding module, so the URL +
// cache TTL + personFields constants are inlined inside
// `fetch_connections` rather than declared at the top.

// ---------------------------------------------------------------------------
// Public dispatch surface (the host calls these by name).

fn identity_resolve(transport, handle, oauth) {
    let token = oauth["controller"];
    if token == () {
        // Not connected — return null so the host's identity-
        // provider chain falls through to the next provider.
        return #{ "match": () };
    }
    if transport != "email" && transport != "phone" {
        // Manifest declares only email + phone; defensive null.
        return #{ "match": () };
    }
    let target = if transport == "email" {
        lower(trim(handle))
    } else {
        digits_only(handle)
    };
    let connections = fetch_connections(token);
    if connections == () {
        return #{ "match": () };
    }
    for person in connections {
        if person_matches(person, transport, target) {
            return #{ "match": build_match(person) };
        }
    }
    #{ "match": () }
}

fn tool_call(name, args, oauth) {
    if name != "contacts.list" {
        // The host's dispatch layer would normally reject unknown
        // tool names before reaching us, but be defensive.
        throw "unknown tool '" + name + "'";
    }
    let token = oauth["controller"];
    if token == () {
        throw "Google account not connected — visit Settings → Google Contacts to configure";
    }
    let limit = if args.len() > 0 && args["limit"] != () {
        args["limit"]
    } else {
        200
    };
    let connections = fetch_connections(token);
    if connections == () {
        return #{ "contacts": [], "total": 0, "limit": limit };
    }
    let total = connections.len();
    let out = [];
    let n = if total < limit { total } else { limit };
    for i in 0..n {
        let p = connections[i];
        out.push(summarize_person(p));
    }
    #{ "contacts": out, "total": total, "limit": limit }
}

// ---------------------------------------------------------------------------
// Helpers.

// Returns the connections array from the People API, or () on
// fetch error / missing field. The host caches the raw response
// for 5 minutes (a token rotation invalidates the cache because
// the bearer is part of the cache key).
fn fetch_connections(token) {
    let resp = http_get_cached(
        "https://people.googleapis.com/v1/people/me/connections",
        #{ "personFields": "names,emailAddresses,phoneNumbers", "pageSize": "1000" },
        token,
        300,
    );
    if resp == () || resp["connections"] == () {
        return ();
    }
    resp["connections"]
}

// True when the contact has an identifier matching the target.
// `target` is already normalised (lowercased email or digits-only
// phone); we normalise the contact-side values the same way for
// comparison.
fn person_matches(person, transport, target) {
    if transport == "email" {
        let emails = person["emailAddresses"];
        if emails == () {
            return false;
        }
        for e in emails {
            if e["value"] == () {
                continue;
            }
            if lower(trim(e["value"])) == target {
                return true;
            }
        }
        return false;
    }
    // phone
    let phones = person["phoneNumbers"];
    if phones == () {
        return false;
    }
    for p in phones {
        if p["value"] == () {
            continue;
        }
        if digits_only(p["value"]) == target {
            return true;
        }
    }
    false
}

fn build_match(person) {
    let display = "";
    if person["names"] != () && person["names"].len() > 0 {
        let n = person["names"][0];
        if n["displayName"] != () {
            display = n["displayName"];
        }
    }
    let resource = if person["resourceName"] != () {
        person["resourceName"]
    } else {
        // Synthetic stable id — rare path, only when Google omits
        // resourceName (essentially never in practice).
        "anon-" + display
    };
    #{
        "stable_principal_id": "pri_google_" + hash(resource),
        "confidence": 0.95,
        "trust_hint": "Contact",
        "display_name": display,
        "tags": ["google_contacts"],
        "resolved_by": "google-contacts",
    }
}

fn summarize_person(person) {
    let display = ();
    if person["names"] != () && person["names"].len() > 0 {
        let n = person["names"][0];
        if n["displayName"] != () {
            display = n["displayName"];
        }
    }
    let emails = [];
    if person["emailAddresses"] != () {
        for e in person["emailAddresses"] {
            if e["value"] != () {
                emails.push(e["value"]);
            }
        }
    }
    let phones = [];
    if person["phoneNumbers"] != () {
        for p in person["phoneNumbers"] {
            if p["value"] != () {
                phones.push(p["value"]);
            }
        }
    }
    let resource = if person["resourceName"] != () {
        person["resourceName"]
    } else {
        "anon-" + display
    };
    #{
        "principal_id": "pri_google_" + hash(resource),
        "display_name": display,
        "emails": emails,
        "phones": phones,
    }
}
