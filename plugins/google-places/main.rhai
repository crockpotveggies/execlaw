// google-places — Google Places (New) API provider, fully script-tier.
//
// Three agent-callable tools: text search, nearby search, place
// details. Plus four admin routes for config / status / test.
// API-key only — no OAuth, no sidecar. Requests go to
// `https://places.googleapis.com/v1/places:*` via the host's
// `http_post` / `http_get` 4-arg overloads (which pass custom
// headers — Places (New) requires `X-Goog-Api-Key` header auth,
// not Bearer; field selection is via `X-Goog-FieldMask`).
//
// Wire format:
//
//   POST https://places.googleapis.com/v1/places:searchText
//     Headers: X-Goog-Api-Key, X-Goog-FieldMask
//     Body:    { textQuery, maxResultCount, openNow?, includedType? }
//
//   POST https://places.googleapis.com/v1/places:searchNearby
//     Body:    { includedTypes?, maxResultCount, locationRestriction.circle }
//
//   GET  https://places.googleapis.com/v1/places/{placeId}
//     Headers: X-Goog-Api-Key, X-Goog-FieldMask
//
// Cost-tier knob (essentials | pro): essentials returns minimal
// fields per place (cheaper); pro adds hours, phone, website. Default
// pro because the agent's typical use ("find me coffee with hours
// and phone for a 10pm reservation") needs the richer fields.
//
// Mirrors selfhosted-claw's google-places integration tool surface
// 1:1 so agent prompts that worked there work here unchanged.

// Constants are inlined at every call site below — Rhai's
// module-level `const` declarations aren't visible inside
// function bodies, so inlining is the simplest workaround.
//
// Kept as a reference comment:
//   SEARCH_TEXT_URL    = "https://places.googleapis.com/v1/places:searchText"
//   SEARCH_NEARBY_URL  = "https://places.googleapis.com/v1/places:searchNearby"
//   PLACE_DETAILS_BASE = "https://places.googleapis.com/v1/places/"
//   DEFAULT_MAX_RESULTS         = 5
//   DEFAULT_NEARBY_MAX_RESULTS  = 10
//   MAX_RESULTS_LIMIT           = 20
//   MAX_RADIUS_METERS           = 50000

// =====================================================================
// AGENT TOOL DISPATCH
// =====================================================================

fn tool_call(name, args, oauth) {
    if name == "google_places.search"  { return search_impl(args); }
    if name == "google_places.nearby"  { return nearby_impl(args); }
    if name == "google_places.details" { return details_impl(args); }
    throw "unknown tool '" + name + "'";
}

// ---- google_places.search -----------------------------------------

fn search_impl(args) {
    let query = if args.query != () { trim(args.query) } else { "" };
    if query == "" { throw "google_places.search requires `query`"; }
    let api_key = require_api_key();

    let max_results = clamp_max_results(args.max_results, default_max_results());
    let body = #{
        "textQuery": query,
        "maxResultCount": max_results,
    };
    if args.open_now == true { body.openNow = true; }
    if args.type != () && args.type != "" {
        body.includedType = trim(args.type);
    }

    let headers = search_headers(api_key);
    let resp = http_post("https://places.googleapis.com/v1/places:searchText", body, "", headers);
    let raw_places = if resp != () && resp.places != () && type_of(resp.places) == "array" {
        resp.places
    } else { [] };

    let min_rating = parse_min_rating(args.min_rating);
    let filtered = filter_by_min_rating(raw_places, min_rating, max_results);

    #{
        "query": query,
        "requested_max_results": max_results,
        "returned_count": filtered.len(),
        "results": map_summaries(filtered),
    }
}

// ---- google_places.nearby -----------------------------------------

fn nearby_impl(args) {
    let lat = require_coordinate(args.latitude, "latitude");
    let lng = require_coordinate(args.longitude, "longitude");
    let radius = require_radius(args.radius);
    let api_key = require_api_key();

    // Nearby honours the same `default_max_results` setting as
    // search; if the operator hasn't set one, fall back to 10
    // (Google's old default) instead of 5 since "nearby" queries
    // typically want a slightly wider net.
    let nearby_default = default_max_results();
    if nearby_default == 5 { nearby_default = 10; }
    let max_results = clamp_max_results(args.max_results, nearby_default);
    let body = #{
        "maxResultCount": max_results,
        "locationRestriction": #{
            "circle": #{
                "center": #{
                    "latitude": lat,
                    "longitude": lng,
                },
                "radius": radius,
            },
        },
    };
    if args.type != () && args.type != "" {
        body.includedTypes = [trim(args.type)];
    }

    let headers = search_headers(api_key);
    let resp = http_post("https://places.googleapis.com/v1/places:searchNearby", body, "", headers);
    let raw_places = if resp != () && resp.places != () && type_of(resp.places) == "array" {
        resp.places
    } else { [] };

    let min_rating = parse_min_rating(args.min_rating);
    let filtered = filter_by_min_rating(raw_places, min_rating, max_results);

    #{
        "center": #{ "latitude": lat, "longitude": lng },
        "radius": radius,
        "returned_count": filtered.len(),
        "results": map_summaries(filtered),
    }
}

// ---- google_places.details ----------------------------------------

fn details_impl(args) {
    let pid = if args.place_id != () { trim(args.place_id) } else { "" };
    if pid == "" { throw "google_places.details requires `place_id`"; }
    let api_key = require_api_key();

    let headers = #{
        "X-Goog-Api-Key": api_key,
        "X-Goog-FieldMask": details_field_mask(),
    };
    let url = "https://places.googleapis.com/v1/places/" + url_encode(pid);
    let place = http_get(url, #{}, "", headers);
    if place == () {
        throw "google_places: no response for place_id=" + pid;
    }

    map_details(place, pid)
}

// =====================================================================
// HELPERS
// =====================================================================

fn require_api_key() {
    let key = vault_get("api_key");
    if key == () || key == "" {
        throw "google-places not configured — set api_key in Settings → Plugins → Google Places";
    }
    key
}

fn search_headers(api_key) {
    #{
        "X-Goog-Api-Key": api_key,
        "X-Goog-FieldMask": search_field_mask(),
    }
}

// Field masks: which fields the API returns. Smaller mask = lower
// per-call cost (Places New is priced per-field-class).
//   essentials → name + address + rating + price + types + maps URI
//   pro        → adds opening hours, website, phone
fn search_field_mask() {
    let tier = cost_tier();
    if tier == "essentials" {
        return "places.id,places.displayName,places.formattedAddress,places.rating,places.userRatingCount,places.priceLevel,places.types,places.googleMapsUri";
    }
    "places.id,places.displayName,places.formattedAddress,places.rating,places.userRatingCount,places.priceLevel,places.types,places.regularOpeningHours.weekdayDescriptions,places.websiteUri,places.nationalPhoneNumber,places.googleMapsUri"
}

fn details_field_mask() {
    "id,displayName,formattedAddress,rating,userRatingCount,priceLevel,types,regularOpeningHours.weekdayDescriptions,currentOpeningHours.openNow,currentOpeningHours.weekdayDescriptions,websiteUri,nationalPhoneNumber,googleMapsUri,editorialSummary,reviews.rating,reviews.relativePublishTimeDescription,reviews.publishTime"
}

fn cost_tier() {
    let raw = vault_get("cost_tier");
    if raw == "essentials" { return "essentials"; }
    if raw == "pro" { return "pro"; }
    // Unknown / Unit / empty / hand-edited garbage — default to
    // pro (the more useful tier) and log so the operator notices
    // a stale/malformed vault row instead of silently getting
    // unexpected behaviour.
    if raw != () && raw != "" {
        log_warn("google-places: vault has cost_tier='" + raw + "' which is neither 'pro' nor 'essentials'; using pro");
    }
    "pro"
}

fn default_max_results() {
    let raw = vault_get("default_max_results");
    if raw == () || raw == "" { return 5; }
    let n = host_parse_int(raw);
    if n == () { return 5; }
    if n < 1 { return 5; }
    if n > 20 { return 20; }
    n
}

fn clamp_max_results(arg, fallback) {
    if arg == () { return fallback; }
    let n = arg;
    if type_of(n) == "string" {
        let parsed = host_parse_int(n);
        if parsed == () { return fallback; }
        n = parsed;
    }
    if type_of(n) != "i64" && type_of(n) != "int" { return fallback; }
    if n < 1 { return 1; }
    if n > 20 { return 20; }
    n
}

fn parse_min_rating(arg) {
    if arg == () { return (); }
    let nf = coerce_to_float(arg);
    if nf == () { return (); }
    if nf < 0.0 || nf > 5.0 {
        throw "min_rating must be between 0 and 5";
    }
    nf
}

fn require_coordinate(arg, label) {
    if arg == () { throw label + " is required"; }
    let nf = coerce_to_float(arg);
    if nf == () { throw label + " must be a number"; }
    nf
}

fn require_radius(arg) {
    if arg == () { throw "radius is required"; }
    let nf = coerce_to_float(arg);
    if nf == () { throw "radius must be a number"; }
    if nf <= 0.0 { throw "radius must be positive"; }
    if nf > 50000.0 { return 50000.0; }
    nf
}

// Best-effort number coercion: i64 / f64 / int-string / float-string
// → f64; everything else → (). Used for lat/lng/radius/rating where
// the agent may serialise a number as a JSON string.
fn coerce_to_float(arg) {
    if arg == () { return (); }
    let t = type_of(arg);
    if t == "f64" || t == "float" { return arg; }
    if t == "i64" || t == "int" { return arg.to_float(); }
    if t == "string" {
        let s = trim(arg);
        if s == "" { return (); }
        // Try float first (covers "49.2827"), fall back to int
        // (covers "5") so a stringified integer also works.
        let f = host_parse_float(s);
        if f != () { return f; }
        let n = host_parse_int(s);
        if n == () { return (); }
        return n.to_float();
    }
    ()
}

// Drop places below `min_rating` (when set) and cap to max_results.
fn filter_by_min_rating(places, min_rating, max_results) {
    let out = [];
    for p in places {
        if min_rating != () {
            let r = if p.rating != () { p.rating } else { 0 };
            let rf = if type_of(r) == "f64" || type_of(r) == "float" { r }
                     else if type_of(r) == "i64" || type_of(r) == "int" { r.to_float() }
                     else { 0.0 };
            if rf < min_rating { continue; }
        }
        out.push(p);
        if out.len() >= max_results { break; }
    }
    out
}

// Place → flat summary map for search/nearby results. Field names
// snake_case (host convention) — selfhosted-claw used camelCase
// but the agent doesn't care; consistency with our other plugins
// matters more.
fn map_summaries(places) {
    let out = [];
    for p in places {
        out.push(summary_of(p));
    }
    out
}

fn summary_of(p) {
    let name = "";
    if p.displayName != () && p.displayName.text != () {
        name = p.displayName.text;
    }
    #{
        "place_id":             if p.id != () { p.id } else { "" },
        "name":                 name,
        "formatted_address":    if p.formattedAddress != () { p.formattedAddress } else { "" },
        "rating":               if p.rating != () { p.rating } else { () },
        "user_rating_count":    if p.userRatingCount != () { p.userRatingCount } else { () },
        "price_level":          if p.priceLevel != () { p.priceLevel } else { () },
        "types":                if p.types != () { p.types } else { [] },
        "hours":                hours_array(p.regularOpeningHours),
        "website_uri":          if p.websiteUri != () { p.websiteUri } else { () },
        "national_phone_number": if p.nationalPhoneNumber != () { p.nationalPhoneNumber } else { () },
        "google_maps_uri":      if p.googleMapsUri != () { p.googleMapsUri } else { () },
    }
}

fn hours_array(hours_obj) {
    if hours_obj == () { return []; }
    if hours_obj.weekdayDescriptions == () { return []; }
    if type_of(hours_obj.weekdayDescriptions) != "array" { return []; }
    hours_obj.weekdayDescriptions
}

// Place → full details map for the details tool. Keeps the
// reviews summary lean (count + per-review rating + relative
// timestamp); we deliberately don't pass review BODY back to the
// agent — the editorial summary is usually enough and reviews
// would explode token cost.
fn map_details(p, fallback_pid) {
    let name = "";
    if p.displayName != () && p.displayName.text != () {
        name = p.displayName.text;
    }
    let editorial = ();
    if p.editorialSummary != () && p.editorialSummary.text != () {
        editorial = p.editorialSummary.text;
    }
    let regular_hours = hours_array(p.regularOpeningHours);
    let current_hours = hours_array(p.currentOpeningHours);
    let open_now = ();
    if p.currentOpeningHours != () && p.currentOpeningHours.openNow != () {
        open_now = p.currentOpeningHours.openNow;
    }

    let reviews_out = [];
    if p.reviews != () && type_of(p.reviews) == "array" {
        for r in p.reviews {
            reviews_out.push(#{
                "rating":                          if r.rating != () { r.rating } else { () },
                "relative_publish_time_description": if r.relativePublishTimeDescription != () { r.relativePublishTimeDescription } else { () },
                "publish_time":                    if r.publishTime != () { r.publishTime } else { () },
            });
        }
    }

    #{
        "place_id":             if p.id != () { p.id } else { fallback_pid },
        "name":                 name,
        "formatted_address":    if p.formattedAddress != () { p.formattedAddress } else { "" },
        "rating":               if p.rating != () { p.rating } else { () },
        "user_rating_count":    if p.userRatingCount != () { p.userRatingCount } else { () },
        "price_level":          if p.priceLevel != () { p.priceLevel } else { () },
        "types":                if p.types != () { p.types } else { [] },
        "regular_hours":        regular_hours,
        "current_hours": #{
            "open_now": open_now,
            "weekday_descriptions": current_hours,
        },
        "website_uri":          if p.websiteUri != () { p.websiteUri } else { () },
        "national_phone_number": if p.nationalPhoneNumber != () { p.nationalPhoneNumber } else { () },
        "google_maps_uri":      if p.googleMapsUri != () { p.googleMapsUri } else { () },
        "editorial_summary":    editorial,
        "review_summary": #{
            "review_count": reviews_out.len(),
            "reviews":      reviews_out,
        },
    }
}

// =====================================================================
// ADMIN ROUTES
// =====================================================================

fn admin_status(req) {
    let api_key = vault_get("api_key");
    let configured = api_key != () && api_key != "";
    let validated_at = vault_get("validated_at");
    let validation_error = vault_get("validation_error");
    let state = "unconfigured";
    if configured {
        if validation_error != () && validation_error != "" {
            state = "degraded";
        } else if validated_at != () && validated_at != "" {
            state = "online";
        } else {
            state = "configured";
        }
    }
    #{
        "state": state,
        "configured": configured,
        "cost_tier": cost_tier(),
        "default_max_results": default_max_results(),
        "validated_at": if validated_at != () { validated_at } else { "" },
        "validation_error": if validation_error != () { validation_error } else { "" },
    }
}

fn admin_get_config(req) {
    let api_key = vault_get("api_key");
    let validated_at = vault_get("validated_at");
    let validation_error = vault_get("validation_error");
    #{
        "api_key_set": api_key != () && api_key != "",
        "api_key_masked": mask(api_key),
        "cost_tier": cost_tier(),
        "default_max_results": default_max_results(),
        "validated_at": if validated_at != () { validated_at } else { "" },
        "validation_error": if validation_error != () { validation_error } else { "" },
    }
}

fn admin_set_config(req) {
    if req.body == () { throw "missing request body"; }
    let raw_key = if req.body.api_key != () { req.body.api_key } else { "" };
    let raw_tier = if req.body.cost_tier != () { req.body.cost_tier } else { "" };
    let raw_max = if req.body.default_max_results != () { req.body.default_max_results } else { "" };
    let api_key = trim(raw_key);
    let tier = trim(raw_tier).to_lower();
    let max_str = trim(if type_of(raw_max) == "string" { raw_max } else { raw_max.to_string() });

    // Validate ALL inputs before mutating any vault row. A bad
    // request must leave the previous config intact — the worst
    // possible outcome would be "operator pasted a key with a
    // typo, plugin half-applied, now agent calls fail and
    // operator can't tell whether the old or new key was kept."
    if tier != "" && tier != "pro" && tier != "essentials" {
        throw "cost_tier must be 'pro' or 'essentials'";
    }
    let parsed_max = ();
    if max_str != "" && max_str != "0" {
        let n = host_parse_int(max_str);
        if n == () { throw "default_max_results must be an integer"; }
        if n < 1 || n > 20 {
            throw "default_max_results must be between 1 and 20";
        }
        parsed_max = n;
    }

    // Validate the api_key BEFORE any vault writes. Issues a
    // 1-result `coffee` search; on any failure we throw without
    // touching vault rows — the previous config keeps working.
    let validation_error = "";
    if api_key != "" {
        validation_error = validate_api_key(api_key);
        if validation_error != "" {
            // Don't even attempt the rotation. Operator's typo
            // didn't clobber the working key.
            throw validation_error;
        }
    }

    // All validation passed — commit the writes atomically (well,
    // as atomic as multiple vault_put calls can be; they're
    // separate SQLite txns but in practice run sub-millisecond
    // and the only consumer that reads them is this same plugin's
    // tool dispatch which serialises against this admin call).
    if api_key == "" {
        vault_delete("api_key");
        vault_delete("validated_at");
        vault_delete("validation_error");
    } else {
        vault_put("api_key", api_key);
        vault_put("validated_at", unix_to_rfc3339(now()));
        vault_delete("validation_error");
    }
    if tier == "" {
        vault_delete("cost_tier");
    } else {
        vault_put("cost_tier", tier);
    }
    if parsed_max == () {
        vault_delete("default_max_results");
    } else {
        vault_put("default_max_results", parsed_max.to_string());
    }
    log_info("google-places: config updated");
    #{ "ok": true }
}

fn admin_test(req) {
    let api_key = vault_get("api_key");
    if api_key == () || api_key == "" {
        throw "api_key not configured";
    }
    // Defaults to "coffee" if the operator didn't supply a query
    // or supplied empty/whitespace.
    let q = "coffee";
    if req.body != () && req.body.query != () {
        let trimmed = trim(req.body.query);
        if trimmed != "" { q = trimmed; }
    }
    let body = #{ "textQuery": q, "maxResultCount": 3 };
    let headers = search_headers(api_key);
    let resp = http_post("https://places.googleapis.com/v1/places:searchText", body, "", headers);
    let count = if resp != () && resp.places != () && type_of(resp.places) == "array" {
        resp.places.len()
    } else { 0 };
    #{
        "ok": true,
        "query": q,
        "returned_count": count,
        "first_result_name": if count > 0 && resp.places[0].displayName != ()
                                && resp.places[0].displayName.text != () {
            resp.places[0].displayName.text
        } else { "" },
    }
}

// Returns "" on success, an error message on failure. Used by
// admin_set_config to validate-before-persist (sort of — actually
// validate-after-persist; we re-delete on failure inside the
// handler).
fn validate_api_key(api_key) {
    let body = #{ "textQuery": "coffee", "maxResultCount": 1 };
    let headers = #{
        "X-Goog-Api-Key": api_key,
        "X-Goog-FieldMask": "places.id",
    };
    try {
        let resp = http_post("https://places.googleapis.com/v1/places:searchText", body, "", headers);
        if resp == () {
            return "Google Places API returned an empty response.";
        }
        return "";
    } catch (e) {
        return "Google Places API key validation failed: " + e;
    }
}

// =====================================================================
// SHARED UTILS
// =====================================================================

fn mask(s) {
    if s == () || s == "" { return ""; }
    let n = s.len();
    if n <= 4 { return repeat_char("*", n); }
    repeat_char("*", n - 4) + s.sub_string(n - 4)
}

fn repeat_char(ch, n) {
    let out = "";
    let i = 0;
    while i < n {
        out += ch;
        i += 1;
    }
    out
}

// =====================================================================
// TEST-ONLY HELPERS
// =====================================================================
//
// Pure helpers exercised by `crates/script/tests/google_places_plugin.rs`.
// `_test_summary` round-trips a fake Place map through `summary_of`
// without needing a live API key.

fn _test_summary(json_str) {
    let p;
    try { p = parse_json(json_str); } catch (e) { return (); }
    if p == () { return (); }
    summary_of(p)
}

fn _test_clamp_max_results(arg, fallback) {
    clamp_max_results(arg, fallback)
}

fn _test_parse_min_rating(arg) {
    parse_min_rating(arg)
}

fn _test_coerce_to_float(arg) {
    coerce_to_float(arg)
}

fn _test_filter_by_min_rating(places_json, min_rating_or_unit, max_results) {
    let places;
    try { places = parse_json(places_json); } catch (e) { return []; }
    if type_of(places) != "array" { return []; }
    let mr = if min_rating_or_unit == "" { () } else {
        let parsed = parse_min_rating(min_rating_or_unit);
        parsed
    };
    filter_by_min_rating(places, mr, max_results)
}
