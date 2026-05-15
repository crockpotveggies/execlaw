// finance-yahoo — quotes, indices, crypto, forex, candles, search, news,
// quote-summary (profile/financials) via Yahoo Finance's public
// undocumented endpoints. Personal-use plugin (per Yahoo ToS); execlaw
// runs single-operator so each install is one user. No API key, no
// OAuth, no sidecar — pure script-tier HTTP/JSON.
//
// Endpoint surface (cookie+crumb NOT required):
//   GET https://query1.finance.yahoo.com/v8/finance/chart/{symbol}
//        ?interval=1d&range=1d
//   GET https://query2.finance.yahoo.com/v1/finance/search?q=…&newsCount=N
//
// Endpoint surface (cookie+crumb REQUIRED — managed by ensure_session):
//   GET https://query2.finance.yahoo.com/v10/finance/quoteSummary/{symbol}
//
// Cookie+crumb flow:
//   1. GET https://fc.yahoo.com via http_get_envelope — read the A1/A3
//      Set-Cookie header. Yahoo's fc endpoint returns 404 with cookies
//      on most days; that's fine, we just need the Set-Cookie.
//   2. GET https://query2.finance.yahoo.com/v1/test/getcrumb with the
//      captured cookie via http_get_envelope — body_text is the crumb.
//   3. Cache { cookie, crumb, fetched_at } in the vault under the
//      "session" key. Subsequent protected calls reuse for ~24h.
//   4. On 401/403/Unauthorized response, invalidate the session and
//      retry once.
//
// Caching strategy via http_get_cached. Cache key = sha256(url + query
// + bearer), so per-symbol granularity is automatic.
//   quote / index / crypto / fx  15 s
//   intraday candles            300 s
//   daily-and-up candles      86 400 s
//   symbol_search              3 600 s
//   market_news                 300 s
//
// Resilience: every response shape is checked before being returned.
// On a shape mismatch the tool returns `{ error: "upstream_shape_changed",
// got: <truncated sample> }` rather than crashing — Yahoo redesigns
// periodically and we want the operator to see a clear alert rather
// than a stack trace or silent empty card.


// =====================================================================
// AGENT TOOL DISPATCH
// =====================================================================

fn tool_call(name, args, oauth) {
    // Rate-limit every tool call against a single shared budget so
    // the agent can't accidentally torch our minute window by calling
    // ten tools in a row. throttle_acquire throws on overflow; the
    // agent surfaces the throw and retries on the next turn.
    throttle_acquire(name);

    if name == "yahoo_finance.quote"              { return quote_impl(args); }
    if name == "yahoo_finance.index_quote"        { return index_quote_impl(args); }
    if name == "yahoo_finance.crypto_quote"       { return crypto_quote_impl(args); }
    if name == "yahoo_finance.fx_quote"           { return fx_quote_impl(args); }
    if name == "yahoo_finance.historical_candles" { return historical_candles_impl(args); }
    if name == "yahoo_finance.symbol_search"      { return symbol_search_impl(args); }
    if name == "yahoo_finance.market_news"        { return market_news_impl(args); }
    if name == "yahoo_finance.quote_summary"      { return quote_summary_impl(args); }
    if name == "yahoo_finance.health"             { return health_impl(args); }
    throw "unknown tool '" + name + "'";
}


// =====================================================================
// yahoo_finance.quote
// =====================================================================

fn quote_impl(args) {
    let raw = require_string(args["symbol"], "symbol");
    let symbol = normalize_symbol(raw, "equity");
    let q = build_chart(symbol, "1d", "1d");
    let body = chart_call(q["url"], q["params"], 15);
    extract_quote(body, symbol, "equity")
}


// =====================================================================
// yahoo_finance.index_quote
// =====================================================================

fn index_quote_impl(args) {
    let raw = require_string(args["symbol"], "symbol");
    let symbol = normalize_symbol(raw, "index");
    let q = build_chart(symbol, "1d", "1d");
    let body = chart_call(q["url"], q["params"], 15);
    let out = extract_quote(body, symbol, "index");
    if out["error"] == () {
        out["index_symbol"] = symbol;
    }
    out
}


// =====================================================================
// yahoo_finance.crypto_quote
// =====================================================================

fn crypto_quote_impl(args) {
    let raw = require_string(args["symbol"], "symbol");
    let symbol = normalize_symbol(raw, "crypto");
    let q = build_chart(symbol, "1d", "1d");
    let body = chart_call(q["url"], q["params"], 15);
    extract_quote(body, symbol, "crypto")
}


// =====================================================================
// yahoo_finance.fx_quote
// =====================================================================

fn fx_quote_impl(args) {
    let raw = require_string(args["symbol"], "symbol");
    let symbol = normalize_symbol(raw, "fx");
    let q = build_chart(symbol, "1d", "1d");
    let body = chart_call(q["url"], q["params"], 15);
    extract_quote(body, symbol, "fx")
}


// =====================================================================
// yahoo_finance.historical_candles
// =====================================================================

fn historical_candles_impl(args) {
    let raw = require_string(args["symbol"], "symbol");
    // Use a permissive class so the normalizer doesn't fight a caller
    // who types `^DJI` or `BTC-USD` straight in.
    let symbol = normalize_symbol(raw, "any");
    let interval = if args["interval"] != () && args["interval"] != "" {
        args["interval"]
    } else { "1d" };
    let range = if args["range"] != () && args["range"] != "" {
        args["range"]
    } else { "1mo" };
    if !is_valid_interval(interval) {
        throw "yahoo_finance.historical_candles: interval `" + interval +
              "` is not one of 1m/5m/15m/30m/1h/1d/1wk/1mo";
    }
    if !is_valid_range(range) {
        throw "yahoo_finance.historical_candles: range `" + range +
              "` is not one of 1d/5d/1mo/3mo/6mo/1y/2y/5y/10y/ytd/max";
    }
    let q = build_chart(symbol, interval, range);
    let ttl = if interval == "1d" || interval == "1wk" || interval == "1mo" {
        86400
    } else { 300 };
    let body = chart_call(q["url"], q["params"], ttl);
    extract_candles(body, symbol, interval)
}


// =====================================================================
// yahoo_finance.symbol_search
// =====================================================================

fn symbol_search_impl(args) {
    let query = require_string(args["query"], "query");
    let limit = clamp_int(args["limit"], 1, 20, 10);
    let q = #{
        "q":          query,
        "quotesCount": "" + limit,
        "newsCount":   "0",
        "lang":        "en-US",
        "region":      "US",
    };
    let body = search_call(q, 3600);
    extract_search(body, query)
}


// =====================================================================
// yahoo_finance.market_news
// =====================================================================

fn market_news_impl(args) {
    let query = require_string(args["query"], "query");
    let limit = clamp_int(args["limit"], 1, 20, 10);
    let q = #{
        "q":           query,
        "quotesCount": "0",
        "newsCount":   "" + limit,
        "lang":        "en-US",
        "region":      "US",
    };
    let body = search_call(q, 300);
    extract_news(body, query)
}


// =====================================================================
// yahoo_finance.quote_summary
// =====================================================================

fn quote_summary_impl(args) {
    let raw = require_string(args["symbol"], "symbol");
    let symbol = normalize_symbol(raw, "any");
    let modules = if args["modules"] != () && type_of(args["modules"]) == "array"
                     && args["modules"].len() > 0 {
        csv(args["modules"])
    } else {
        "summaryDetail,assetProfile,price,defaultKeyStatistics"
    };

    // First attempt with the cached session. On 401/403 / "Unauthorized"
    // / "Invalid Crumb" we drop the session and retry exactly once —
    // Yahoo periodically rotates the crumb and our copy goes stale.
    let body = quote_summary_fetch(symbol, modules, false);
    if body != () && type_of(body) == "map" && body["__retry"] == true {
        let retry_body = quote_summary_fetch(symbol, modules, true);
        return extract_quote_summary_safely(retry_body, symbol);
    }
    extract_quote_summary_safely(body, symbol)
}

fn quote_summary_fetch(symbol, modules_csv, force_refresh) {
    let session = ensure_session(force_refresh);
    if session == () || session["error"] != () {
        // Couldn't get a session at all — propagate as a shape error
        // so the canary picks it up.
        return #{
            "__retry": false,
            "shape_error": "session_unavailable",
            "detail":      if session != () { session["error"] } else { () },
        };
    }
    let url = "https://query2.finance.yahoo.com/v10/finance/quoteSummary/" + url_encode(symbol);
    let q = #{
        "modules": modules_csv,
        "crumb":   session["crumb"],
        "lang":    "en-US",
        "region":  "US",
    };
    let h = #{
        "Cookie":     session["cookie"],
        "User-Agent": "Mozilla/5.0 (X11; Linux x86_64; rv:124.0) Gecko/20100101 Firefox/124.0",
        "Accept":     "application/json",
    };
    let env = http_get_envelope(url, q, "", h);
    let status = if env != () && env["status"] != () { env["status"] } else { 0 };
    if status == 401 || status == 403 {
        // Stale crumb — caller will retry once with force_refresh.
        if force_refresh == false {
            return #{ "__retry": true };
        }
        return #{
            "shape_error": "auth_after_retry",
            "status":      status,
        };
    }
    if status < 200 || status >= 300 {
        return #{
            "shape_error": "non_2xx_status",
            "status":      status,
            "body":        if env != () { truncate_string(env["body_text"], 240) } else { () },
        };
    }
    if env == () { return #{ "shape_error": "empty_envelope" }; }
    env["body"]
}

fn extract_quote_summary_safely(body, symbol) {
    if body != () && type_of(body) == "map" && body["shape_error"] != () {
        return #{
            "error":   "upstream_shape_changed",
            "reason":  body["shape_error"],
            "symbol":  symbol,
            "status":  body["status"],
            "got":     body["body"],
            "hint":    "Quote summary endpoint failed — Yahoo may have rotated the crumb format or changed the response schema. See Settings → Plugins → Yahoo Finance.",
        };
    }
    extract_quote_summary(body, symbol)
}


// =====================================================================
// yahoo_finance.health  —  canary across the live endpoint surface
// =====================================================================

fn health_impl(args) {
    let checks = [];
    let any_fail = false;

    // Each check runs a single tool against its live upstream and
    // tags ok/fail. Done with inline try/catch (no closure helper)
    // because Rhai FnPtr semantics inside try-blocks are awkward
    // and inlining keeps the trace readable when a check fails.

    // 1. Public chart endpoint (no crumb required)
    let detail = ();
    let ok = false;
    try {
        let r = quote_impl(#{ "symbol": "AAPL" });
        if r == () || r["error"] != () { throw "quote(AAPL) failed: " + r; }
        ok = true;
    } catch (e) { detail = #{ "throw": "" + e }; }
    checks.push(#{ "name": "chart_endpoint", "ok": ok, "detail": detail });
    if !ok { any_fail = true; }

    // 2. Index quote (chart endpoint with caret symbol)
    detail = ();
    ok = false;
    try {
        let r = index_quote_impl(#{ "symbol": "^DJI" });
        if r == () || r["error"] != () { throw "index_quote(^DJI) failed: " + r; }
        ok = true;
    } catch (e) { detail = #{ "throw": "" + e }; }
    checks.push(#{ "name": "index_quote", "ok": ok, "detail": detail });
    if !ok { any_fail = true; }

    // 3. Search endpoint
    detail = ();
    ok = false;
    try {
        let r = symbol_search_impl(#{ "query": "apple", "limit": 3 });
        if r == () || r["error"] != () { throw "search failed: " + r; }
        if r["results"] == () || r["results"].len() == 0 { throw "search returned no results"; }
        ok = true;
    } catch (e) { detail = #{ "throw": "" + e }; }
    checks.push(#{ "name": "search_endpoint", "ok": ok, "detail": detail });
    if !ok { any_fail = true; }

    // 4. Crumb session
    let sess = ();
    try {
        sess = ensure_session(false);
    } catch (e) {
        sess = #{ "error": "" + e };
    }
    let session_ok = sess != () && sess["error"] == () && sess["crumb"] != () && sess["crumb"] != "";
    let session_age = if session_ok && sess["fetched_at"] != () { now() - sess["fetched_at"] } else { () };
    checks.push(#{
        "name":   "crumb_session",
        "ok":     session_ok,
        "detail": if session_ok { #{ "age_secs": session_age } } else { #{ "error": if sess != () { sess["error"] } else { "no session" } } },
    });
    if !session_ok { any_fail = true; }

    // 5. Protected endpoint (quote_summary on a stable symbol)
    detail = ();
    ok = false;
    try {
        let r = quote_summary_impl(#{ "symbol": "AAPL", "modules": ["price"] });
        if r == () || r["error"] != () { throw "quote_summary(AAPL) failed: " + r; }
        ok = true;
    } catch (e) { detail = #{ "throw": "" + e }; }
    checks.push(#{ "name": "quote_summary_endpoint", "ok": ok, "detail": detail });
    if !ok { any_fail = true; }

    #{
        "ok":     !any_fail,
        "checks": checks,
        "as_of":  now(),
    }
}


// =====================================================================
// CRUMB SESSION  —  cookie + crumb pair, cached in vault under "session"
// =====================================================================

// NOTE on the constants below — Rhai's top-level `const` declarations
// aren't visible inside `fn` bodies (same gotcha the slack plugin hit
// in v0.3.1), so the values are inlined at each call site. Centralised
// here as a doc-comment for grep + diff readability.
//   YAHOO_UA          = "Mozilla/5.0 (X11; Linux x86_64; rv:124.0) Gecko/20100101 Firefox/124.0"
//   SESSION_TTL_SECS  = 43200    (12 h — Yahoo's A1+crumb stays valid
//                                 ~24 h+; we refresh proactively at 12 h
//                                 so a hot operator session never eats
//                                 the cost mid-call)
//   THROTTLE_MAX_PER_MIN = 50    (Yahoo 429s aggressively above this
//                                 from a single source)

// Read the cached session from vault (or fetch fresh). Returns a Map:
//   { cookie: string, crumb: string, fetched_at: i64 }
// or { error: string } on failure.
fn ensure_session(force_refresh) {
    if !force_refresh {
        let cached = read_session();
        if cached != () && cached["cookie"] != () && cached["crumb"] != ()
           && cached["fetched_at"] != ()
           && (now() - cached["fetched_at"]) < 43200 {
            return cached;
        }
    }
    let fresh = fetch_session();
    if fresh != () && fresh["error"] == () {
        write_session(fresh);
    }
    fresh
}

fn fetch_session() {
    let ua = "Mozilla/5.0 (X11; Linux x86_64; rv:124.0) Gecko/20100101 Firefox/124.0";
    // Step 1: GET fc.yahoo.com to receive the Set-Cookie. ureq's
    // default agent follows redirects but Yahoo's fc endpoint usually
    // returns 200 or 404 directly with the cookie on the response —
    // either way http_get_envelope captures them.
    let cookie_env = ();
    try {
        cookie_env = http_get_envelope(
            "https://fc.yahoo.com/",
            #{},
            "",
            #{ "User-Agent": ua },
        );
    } catch (e) {
        return #{ "error": "fc_yahoo_fetch: " + e };
    }
    if cookie_env == () {
        return #{ "error": "fc_yahoo_fetch: empty envelope" };
    }
    let raw_cookies = cookie_env["headers"]["set-cookie"];
    let cookie = extract_session_cookie(raw_cookies);
    if cookie == "" {
        return #{ "error": "fc_yahoo_fetch: no usable Set-Cookie in response" };
    }

    // Step 2: GET /v1/test/getcrumb with the cookie. Plain-text body.
    let crumb_env = ();
    try {
        crumb_env = http_get_envelope(
            "https://query2.finance.yahoo.com/v1/test/getcrumb",
            #{},
            "",
            #{
                "Cookie":     cookie,
                "User-Agent": ua,
                "Accept":     "text/plain,*/*;q=0.5",
            },
        );
    } catch (e) {
        return #{ "error": "getcrumb_fetch: " + e };
    }
    if crumb_env == () { return #{ "error": "getcrumb_fetch: empty envelope" }; }
    let status = crumb_env["status"];
    if status < 200 || status >= 300 {
        return #{ "error": "getcrumb_fetch: status " + status };
    }
    // body_text is the raw response (the crumb is a short opaque
    // token like "ibG1c1O0pSI"). body may already be the same string
    // if it doesn't parse as JSON.
    let crumb = "" + crumb_env["body_text"];
    crumb = trim(crumb);
    if crumb == "" { return #{ "error": "getcrumb_fetch: empty crumb body" }; }

    #{
        "cookie":     cookie,
        "crumb":      crumb,
        "fetched_at": now(),
    }
}

// Pull A1=… (or A3=…) cookies out of one or more Set-Cookie lines.
// Yahoo sometimes returns the cookie spread across multiple headers
// (Array) or in a single header (String) depending on edge / region.
fn extract_session_cookie(raw) {
    if raw == () { return ""; }
    let lines = [];
    let t = type_of(raw);
    if t == "string" {
        lines = [raw];
    } else if t == "array" {
        for v in raw { lines.push("" + v); }
    } else {
        lines = ["" + raw];
    }
    let parts = [];
    for line in lines {
        // Each Set-Cookie line looks like `A1=foo=bar; Domain=…; …`.
        // We want just the `name=value` first segment, and only for
        // the auth-relevant names. Other cookies (B, GUC, …) Yahoo
        // sets are not required and inflate the request header.
        let semi = line.index_of(";");
        let kv = if semi >= 0 { line.sub_string(0, semi) } else { line };
        kv = trim(kv);
        let eq = kv.index_of("=");
        if eq <= 0 { continue; }
        let name = trim(kv.sub_string(0, eq));
        if name == "A1" || name == "A3" || name == "A1S" {
            parts.push(kv);
        }
    }
    if parts.len() == 0 { return ""; }
    // Join with "; " — the standard Cookie request header format.
    let out = "";
    let first = true;
    for p in parts {
        if first { first = false; } else { out += "; "; }
        out += p;
    }
    out
}

fn read_session() {
    let raw = ();
    try { raw = vault_get("session"); } catch (e) { return (); }
    if raw == () || raw == "" { return (); }
    let parsed = ();
    try { parsed = parse_json(raw); } catch (e) { return (); }
    if parsed == () || type_of(parsed) != "map" { return (); }
    parsed
}

fn write_session(sess) {
    try {
        vault_put("session", to_json_string(sess));
    } catch (e) {
        log_warn("finance-yahoo: vault_put(session) failed: " + e);
    }
}

fn extract_quote_summary(body, symbol) {
    if body == () || type_of(body) != "map" {
        return shape_error("body_not_map", body);
    }
    let outer = body["quoteSummary"];
    if outer == () || type_of(outer) != "map" {
        return shape_error("missing_quoteSummary", body);
    }
    if outer["error"] != () && outer["error"] != "" {
        return #{
            "error":  "upstream_error",
            "symbol": symbol,
            "detail": "" + outer["error"],
        };
    }
    let result_arr = outer["result"];
    if result_arr == () || type_of(result_arr) != "array" || result_arr.len() == 0 {
        return shape_error("empty_result", body);
    }
    let r = result_arr[0];

    let out = #{
        "symbol":             symbol,
        "chat_component_kind": "stock_quote_summary",
    };

    // summaryDetail
    if r["summaryDetail"] != () && type_of(r["summaryDetail"]) == "map" {
        let sd = r["summaryDetail"];
        out["market_cap"]     = pick_raw(sd["marketCap"]);
        out["pe_trailing"]    = pick_raw(sd["trailingPE"]);
        out["pe_forward"]     = pick_raw(sd["forwardPE"]);
        out["dividend_yield"] = pick_raw(sd["dividendYield"]);
        out["fifty_two_high"] = pick_raw(sd["fiftyTwoWeekHigh"]);
        out["fifty_two_low"]  = pick_raw(sd["fiftyTwoWeekLow"]);
        out["beta"]           = pick_raw(sd["beta"]);
        out["currency"]       = if sd["currency"] != () { "" + sd["currency"] } else { () };
    }
    // assetProfile
    if r["assetProfile"] != () && type_of(r["assetProfile"]) == "map" {
        let ap = r["assetProfile"];
        out["sector"]            = if ap["sector"] != ()           { "" + ap["sector"] }           else { () };
        out["industry"]          = if ap["industry"] != ()         { "" + ap["industry"] }         else { () };
        out["full_time_employees"] = if ap["fullTimeEmployees"] != () { ap["fullTimeEmployees"] } else { () };
        out["country"]           = if ap["country"] != ()          { "" + ap["country"] }          else { () };
        out["website"]           = if ap["website"] != ()          { "" + ap["website"] }          else { () };
        out["long_business_summary"] = if ap["longBusinessSummary"] != () { "" + ap["longBusinessSummary"] } else { () };
    }
    // price
    if r["price"] != () && type_of(r["price"]) == "map" {
        let p = r["price"];
        out["short_name"] = if p["shortName"] != () { "" + p["shortName"] } else { () };
        out["long_name"]  = if p["longName"]  != () { "" + p["longName"] }  else { () };
        out["exchange"]   = if p["exchangeName"] != () { "" + p["exchangeName"] } else { () };
    }
    // defaultKeyStatistics
    if r["defaultKeyStatistics"] != () && type_of(r["defaultKeyStatistics"]) == "map" {
        let dks = r["defaultKeyStatistics"];
        out["shares_outstanding"] = pick_raw(dks["sharesOutstanding"]);
        out["float_shares"]       = pick_raw(dks["floatShares"]);
        out["enterprise_value"]   = pick_raw(dks["enterpriseValue"]);
    }
    out
}

// Yahoo wraps every number in `{ raw: 12345, fmt: "12.3K", longFmt: "12,345" }`.
// `pick_raw` unwraps to just the numeric `raw` and tolerates either a
// bare number or the wrapped form.
fn pick_raw(v) {
    if v == () { return (); }
    let t = type_of(v);
    if t == "f64" || t == "float" { return v; }
    if t == "i64" || t == "int"   { return v; }
    if t == "map" {
        let r = v["raw"];
        if r == () { return (); }
        return r;
    }
    ()
}

fn truncate_string(s, max) {
    if s == () { return (); }
    let t = "" + s;
    if t.len() <= max { return t; }
    t.sub_string(0, max) + "…"
}


// =====================================================================
// RATE-LIMIT THROTTLE
// =====================================================================
//
// Yahoo doesn't publish a quota but 429s aggressively above ~50 calls
// per minute from a single source. We track a minute-bucket counter
// in vault and throw when the budget is exhausted. Read-only callers
// (cache hits) still acquire so the budget reflects "tool invocations"
// rather than "actual network calls" — simpler and conservative.

fn throttle_acquire(tool_name) {
    // Inlined max (Rhai top-level const isn't visible in fn bodies):
    let max_per_min = 50;
    let now_s = now();
    let minute = now_s / 60;
    let state = ();
    try {
        let raw = vault_get("throttle");
        if raw != () && raw != "" { state = parse_json(raw); }
    } catch (e) {
        state = ();
    }
    if state == () || type_of(state) != "map" || state["minute"] != minute {
        state = #{ "minute": minute, "count": 0 };
    }
    state["count"] = state["count"] + 1;
    if state["count"] > max_per_min {
        // Don't persist the over-budget bump — leave the count at the
        // limit so a paced caller can resume next minute without an
        // artificial lockout.
        throw "yahoo_finance: rate limit reached (>" + max_per_min +
              " calls in 60s). Tool `" + tool_name + "` was rejected. " +
              "Retry on the next minute boundary or batch fewer calls per turn.";
    }
    try {
        vault_put("throttle", to_json_string(state));
    } catch (e) {
        // Don't fail the tool call if vault write is busted — soft
        // throttle is advisory, the real backstop is Yahoo's own 429.
    }
}


// =====================================================================
// ADMIN ROUTES
// =====================================================================

fn admin_get_config(req) {
    let cfg = read_config();
    #{
        "default_symbols": cfg.default_symbols,
        "tos_ack_at":      cfg.tos_ack_at,
        "version":         "0.1.0",
        "endpoint_status": #{
            "chart_endpoint":  "ok",
            "search_endpoint": "ok",
            "quote_summary":   "deferred_to_v0.2_needs_crumb_support",
        },
    }
}

fn admin_set_config(req) {
    let body = if req["body"] != () { req["body"] } else { #{} };
    let cfg = read_config();
    if body["default_symbols"] != () && type_of(body["default_symbols"]) == "array" {
        cfg["default_symbols"] = body["default_symbols"];
    }
    if body["tos_ack"] == true {
        cfg["tos_ack_at"] = now();
    }
    vault_put("config", to_json_string(cfg));
    #{ "ok": true, "config": cfg }
}

fn admin_test(req) {
    // Round-trip one symbol per asset class so the operator can see
    // at a glance which classes are reachable and which Yahoo broke.
    let cases = [
        #{ "label": "equity", "tool": "yahoo_finance.quote",        "symbol": "AAPL" },
        #{ "label": "index",  "tool": "yahoo_finance.index_quote",  "symbol": "^DJI" },
        #{ "label": "crypto", "tool": "yahoo_finance.crypto_quote", "symbol": "BTC-USD" },
        #{ "label": "fx",     "tool": "yahoo_finance.fx_quote",     "symbol": "EURUSD=X" },
    ];
    let results = [];
    let any_ok = false;
    for c in cases {
        let ok = false;
        let detail = ();
        try {
            let r = tool_call(c.tool, #{ "symbol": c.symbol }, #{});
            if r != () && r["error"] == () {
                ok = true;
                any_ok = true;
                detail = #{
                    "price":  r["price"],
                    "change": r["change_pct"],
                };
            } else if r != () {
                detail = #{ "error": r["error"] };
            }
        } catch (e) {
            detail = #{ "throw": "" + e };
        }
        results.push(#{
            "label":  c.label,
            "tool":   c.tool,
            "symbol": c.symbol,
            "ok":     ok,
            "detail": detail,
        });
    }
    #{ "ok": any_ok, "results": results }
}


// =====================================================================
// HTTP HELPERS
// =====================================================================

// Build the (url, params) pair for a chart-endpoint call. We split
// the symbol out into the path because Yahoo's router insists on it
// being a path segment, but http_get_cached takes a query map only —
// so we inline the symbol into the URL string here.
fn build_chart(symbol, interval, range) {
    let url = "https://query1.finance.yahoo.com/v8/finance/chart/" + url_encode(symbol);
    let params = #{
        "interval":     interval,
        "range":        range,
        "includePrePost": "false",
        "events":       "div,split",
        "lang":         "en-US",
        "region":       "US",
        "corsDomain":   "finance.yahoo.com",
    };
    #{ "url": url, "params": params }
}

fn chart_call(url, params, ttl_secs) {
    // Yahoo's chart endpoint returns 4xx with a JSON error envelope
    // when the symbol is unknown, so http_get_cached will throw on
    // those — callers wrap us with extract_quote/extract_candles
    // which inspect the throw via try/catch in admin_test, and the
    // agent surface either gets the structured error or the throw
    // message bubbled up as a tool error.
    http_get_cached(url, params, "", ttl_secs)
}

fn search_call(params, ttl_secs) {
    http_get_cached(
        "https://query2.finance.yahoo.com/v1/finance/search",
        params,
        "",
        ttl_secs,
    )
}


// =====================================================================
// RESPONSE EXTRACTORS (each is a shape validator + projector)
// =====================================================================

fn extract_quote(body, symbol, asset_class) {
    if body == () || type_of(body) != "map" {
        return shape_error("body_not_map", body);
    }
    let chart = body["chart"];
    if chart == () || type_of(chart) != "map" {
        return shape_error("missing_chart", body);
    }
    if chart["error"] != () && chart["error"] != "" {
        // Yahoo's structured "Not Found" / "Invalid input" envelope.
        return #{
            "error":  "upstream_error",
            "symbol": symbol,
            "detail": "" + chart["error"],
        };
    }
    let result_arr = chart["result"];
    if result_arr == () || type_of(result_arr) != "array" || result_arr.len() == 0 {
        return shape_error("empty_result", body);
    }
    let r = result_arr[0];
    let meta = r["meta"];
    if meta == () || type_of(meta) != "map" {
        return shape_error("missing_meta", body);
    }
    let price = pick_number(meta, "regularMarketPrice");
    let prev  = pick_number(meta, "chartPreviousClose");
    let day_high = pick_number(meta, "regularMarketDayHigh");
    let day_low  = pick_number(meta, "regularMarketDayLow");
    let volume   = pick_number(meta, "regularMarketVolume");
    let currency = if meta["currency"] != () { "" + meta["currency"] } else { () };
    let exchange = if meta["exchangeName"] != () { "" + meta["exchangeName"] } else { () };
    let market_state = if meta["marketState"] != () { "" + meta["marketState"] } else { () };
    let ts = pick_number(meta, "regularMarketTime");
    let change = ();
    let change_pct = ();
    if price != () && prev != () && prev != 0.0 {
        change = price - prev;
        change_pct = ((price - prev) / prev) * 100.0;
    }
    #{
        "symbol":          symbol,
        "asset_class":     asset_class,
        "price":           price,
        "previous_close":  prev,
        "change":          change,
        "change_pct":      change_pct,
        "day_high":        day_high,
        "day_low":         day_low,
        "volume":          volume,
        "currency":        currency,
        "exchange":        exchange,
        "market_state":    market_state,
        "as_of":           ts,
        "chat_component_kind": "stock_quote",
    }
}

fn extract_candles(body, symbol, interval) {
    if body == () || type_of(body) != "map" {
        return shape_error("body_not_map", body);
    }
    let chart = body["chart"];
    if chart == () || type_of(chart) != "map" {
        return shape_error("missing_chart", body);
    }
    if chart["error"] != () && chart["error"] != "" {
        return #{ "error": "upstream_error", "symbol": symbol, "detail": "" + chart["error"] };
    }
    let result_arr = chart["result"];
    if result_arr == () || type_of(result_arr) != "array" || result_arr.len() == 0 {
        return shape_error("empty_result", body);
    }
    let r = result_arr[0];
    let ts_arr = r["timestamp"];
    let ind = r["indicators"];
    if ind == () || type_of(ind) != "map" {
        return shape_error("missing_indicators", body);
    }
    let quote_arr = ind["quote"];
    if quote_arr == () || type_of(quote_arr) != "array" || quote_arr.len() == 0 {
        // Some shapes (illiquid symbols on illiquid intervals) have
        // an empty indicators.quote — return empty candles rather
        // than treating it as a hard shape failure.
        return #{
            "symbol":   symbol,
            "interval": interval,
            "candles":  [],
            "chat_component_kind": "stock_candles",
        };
    }
    let q = quote_arr[0];
    let opens   = if q["open"]   != () { q["open"]   } else { [] };
    let highs   = if q["high"]   != () { q["high"]   } else { [] };
    let lows    = if q["low"]    != () { q["low"]    } else { [] };
    let closes  = if q["close"]  != () { q["close"]  } else { [] };
    let volumes = if q["volume"] != () { q["volume"] } else { [] };
    let ts = if ts_arr != () && type_of(ts_arr) == "array" { ts_arr } else { [] };
    let out = [];
    let n = ts.len();
    for i in 0..n {
        let o = opens[i];
        let c = closes[i];
        // Yahoo emits nulls for non-trading minutes; skip those.
        if o == () && c == () { continue; }
        out.push(#{
            "t":      ts[i],
            "open":   opens[i],
            "high":   highs[i],
            "low":    lows[i],
            "close":  closes[i],
            "volume": volumes[i],
        });
    }
    #{
        "symbol":   symbol,
        "interval": interval,
        "candles":  out,
        "chat_component_kind": "stock_candles",
    }
}

fn extract_search(body, query) {
    if body == () || type_of(body) != "map" {
        return shape_error("body_not_map", body);
    }
    let quotes = body["quotes"];
    if quotes == () || type_of(quotes) != "array" {
        return #{ "query": query, "results": [], "chat_component_kind": "stock_search" };
    }
    let out = [];
    for q in quotes {
        if type_of(q) != "map" { continue; }
        out.push(#{
            "symbol":    if q["symbol"]    != () { "" + q["symbol"] }    else { "" },
            "name":      if q["shortname"] != () { "" + q["shortname"] } else if q["longname"] != () { "" + q["longname"] } else { "" },
            "exchange":  if q["exchDisp"]  != () { "" + q["exchDisp"] }  else { () },
            "type":      if q["quoteType"] != () { "" + q["quoteType"] } else { () },
            "type_disp": if q["typeDisp"]  != () { "" + q["typeDisp"] }  else { () },
        });
    }
    #{
        "query":   query,
        "results": out,
        "chat_component_kind": "stock_search",
    }
}

fn extract_news(body, query) {
    if body == () || type_of(body) != "map" {
        return shape_error("body_not_map", body);
    }
    let news = body["news"];
    if news == () || type_of(news) != "array" {
        return #{ "query": query, "news": [], "chat_component_kind": "stock_news" };
    }
    let out = [];
    for n in news {
        if type_of(n) != "map" { continue; }
        out.push(#{
            "uuid":      if n["uuid"]      != () { "" + n["uuid"] }      else { () },
            "title":     if n["title"]     != () { "" + n["title"] }     else { "" },
            "publisher": if n["publisher"] != () { "" + n["publisher"] } else { () },
            "link":      if n["link"]      != () { "" + n["link"] }      else { () },
            "provider_publish_time": pick_number(n, "providerPublishTime"),
            "type":      if n["type"]      != () { "" + n["type"] }      else { () },
        });
    }
    #{
        "query": query,
        "news":  out,
        "chat_component_kind": "stock_news",
    }
}

fn shape_error(reason, got) {
    let sample = "";
    try {
        sample = to_json_string(got);
        if sample.len() > 240 { sample = sample.sub_string(0, 240) + "…"; }
    } catch (e) {
        sample = "<unserialisable>";
    }
    #{
        "error":  "upstream_shape_changed",
        "reason": reason,
        "got":    sample,
        "hint":   "Yahoo Finance may have changed their response schema. Check Settings → Plugins → Yahoo Finance for the latest health check.",
    }
}

fn pick_number(m, key) {
    let v = m[key];
    if v == () { return (); }
    let t = type_of(v);
    if t == "f64" || t == "float" { return v; }
    if t == "i64" || t == "int"   { return v.to_float(); }
    if t == "string" {
        let f = host_parse_float(v);
        if f != () { return f; }
        let n = host_parse_int(v);
        if n != () { return n.to_float(); }
    }
    ()
}


// =====================================================================
// SYMBOL NORMALIZATION
// =====================================================================

// Map an operator-typed symbol or friendly name into the form Yahoo's
// router actually wants. `class` is a hint that biases ambiguous cases
// (e.g. "btc" -> "BTC-USD" under class="crypto" but stays "BTC" under
// "any"). Returns the normalized symbol string.
fn normalize_symbol(raw, class) {
    let s = trim(raw);
    if s == "" { throw "finance-yahoo: symbol is empty after trimming"; }
    let lower = lower(s);

    // Friendly-name aliases for indices — these have no other reasonable
    // interpretation, so we apply them regardless of class.
    let alias = index_alias(lower);
    if alias != () { return alias; }

    if class == "index" {
        // Caller asked for an index but typed something we don't have
        // an alias for. Accept a bare caret-prefixed symbol; otherwise
        // pass it through verbatim and let Yahoo decide.
        return s;
    }

    if class == "crypto" {
        // BTC -> BTC-USD; BTC-EUR or BTC-USD passes through unchanged.
        if s.contains("-") { return s; }
        return s + "-USD";
    }

    if class == "fx" {
        // Accept "EUR/USD", "EURUSD", "EURUSD=X". Rhai's
        // `string.replace(a, b)` mutates in place and returns Unit
        // (see plugins/signal/main.rhai for the canonical note), so
        // chaining `s.replace(...).replace(...)` blows up — do each
        // mutation against a local instead.
        if s.ends_with("=X") { return s; }
        let cleaned = s;
        cleaned.replace("/", "");
        cleaned.replace(" ", "");
        // Yahoo's FX format expects uppercase, six-letter pair.
        if cleaned.len() >= 6 {
            return cleaned + "=X";
        }
        return s;
    }

    // class == "equity" or "any" — pass through, trimmed.
    s
}

fn index_alias(lower) {
    if lower == "dow"     || lower == "dow jones" || lower == "djia" || lower == "^dji"  { return "^DJI"; }
    if lower == "nasdaq"  || lower == "nasdaq composite" || lower == "^ixic"             { return "^IXIC"; }
    if lower == "nasdaq 100" || lower == "ndx" || lower == "^ndx"                        { return "^NDX"; }
    if lower == "s&p"     || lower == "s&p 500" || lower == "sp500" || lower == "spx" || lower == "^gspc" { return "^GSPC"; }
    if lower == "russell" || lower == "russell 2000" || lower == "^rut"                  { return "^RUT"; }
    if lower == "vix"     || lower == "^vix"                                              { return "^VIX"; }
    if lower == "ftse"    || lower == "ftse 100" || lower == "^ftse"                     { return "^FTSE"; }
    if lower == "nikkei"  || lower == "nikkei 225" || lower == "^n225"                   { return "^N225"; }
    if lower == "dax"     || lower == "^gdaxi"                                            { return "^GDAXI"; }
    ()
}


// =====================================================================
// VALIDATION
// =====================================================================

fn is_valid_interval(s) {
    s == "1m" || s == "5m" || s == "15m" || s == "30m" ||
    s == "1h" || s == "1d" || s == "1wk" || s == "1mo"
}

fn is_valid_range(s) {
    s == "1d" || s == "5d" || s == "1mo" || s == "3mo" ||
    s == "6mo" || s == "1y" || s == "2y" || s == "5y" ||
    s == "10y" || s == "ytd" || s == "max"
}


// =====================================================================
// CONFIG
// =====================================================================

fn read_config() {
    let raw = ();
    try {
        raw = vault_get("config");
    } catch (e) {
        // No host caps wired (test fixture) — fall through.
    }
    let fallback = #{
        "default_symbols": ["AAPL", "^DJI", "^IXIC", "^GSPC"],
        "tos_ack_at":      (),
    };
    if raw == () || raw == "" { return fallback; }
    let parsed = ();
    try {
        parsed = parse_json(raw);
    } catch (e) {
        log_warn("finance-yahoo: vault config JSON unparseable: " + e);
    }
    if parsed == () || type_of(parsed) != "map" { return fallback; }
    for k in fallback.keys() {
        if parsed[k] == () { parsed[k] = fallback[k]; }
    }
    parsed
}


// =====================================================================
// SHARED VALIDATORS
// =====================================================================

fn require_string(value, label) {
    if value == () || value == "" {
        throw "finance-yahoo: `" + label + "` is required";
    }
    if type_of(value) != "string" {
        throw "finance-yahoo: `" + label + "` must be a string";
    }
    trim(value)
}

// Join an Array of values with commas (no trailing comma). Used to
// build CSV query params (modules=summaryDetail,assetProfile,...).
fn csv(arr) {
    let out = "";
    let first = true;
    for v in arr {
        if first { first = false; } else { out += ","; }
        out += "" + v;
    }
    out
}

fn clamp_int(value, lo, hi, fallback) {
    if value == () { return fallback; }
    let n = value;
    let t = type_of(n);
    if t == "string" {
        let parsed = host_parse_int(n);
        if parsed == () { return fallback; }
        n = parsed;
    }
    if type_of(n) != "i64" && type_of(n) != "int" { return fallback; }
    if n < lo { return lo; }
    if n > hi { return hi; }
    n
}


// =====================================================================
// TEST HOOKS (called from crates/script/tests/finance_yahoo_plugin.rs)
//
// Thin wrappers exposing pure-Rhai helpers without going through the
// live host stack — mirrors the open-meteo / discord / signal pattern.

fn _test_normalize_symbol(raw, class)        { normalize_symbol(raw, class) }
fn _test_index_alias(lower)                  { index_alias(lower) }
fn _test_is_valid_interval(s)                { is_valid_interval(s) }
fn _test_is_valid_range(s)                   { is_valid_range(s) }
fn _test_clamp_int(v, lo, hi, fb)            { clamp_int(v, lo, hi, fb) }
fn _test_extract_quote(body, sym, class)     { extract_quote(body, sym, class) }
fn _test_extract_candles(body, sym, ivl)     { extract_candles(body, sym, ivl) }
fn _test_extract_search(body, q)             { extract_search(body, q) }
fn _test_extract_news(body, q)               { extract_news(body, q) }
fn _test_extract_quote_summary(body, sym)    { extract_quote_summary(body, sym) }
fn _test_extract_session_cookie(raw)         { extract_session_cookie(raw) }
fn _test_pick_raw(v)                          { pick_raw(v) }
fn _test_shape_error(reason, got)            { shape_error(reason, got) }
fn _test_truncate_string(s, max)             { truncate_string(s, max) }
