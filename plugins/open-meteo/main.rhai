// open-meteo — public weather/marine/air-quality/seasonal/ensemble/
// flood/climate/geocoding/elevation tools plus a generic chart renderer
// for the SPA + transport attachments. Pure-Rhai, no sidecar, no auth.
//
// Each per-API tool issues a single HTTP GET against the relevant
// Open-Meteo endpoint via `http_get_cached` with a per-API TTL that
// matches how often the upstream data actually changes. Cache keys
// dedupe identical queries inside the TTL window (saves Open-Meteo's
// rate budget AND keeps repeated agent turns snappy).
//
// The agent calls these tools with raw lat/lon (resolve via
// open_meteo.geocode first when the operator gives a place name). To
// keep the per-tool response payload light, the agent supplies the
// `current`/`hourly`/`daily` variable arrays it actually wants — we
// don't fetch everything by default.
//
// Wire format primer:
//   GET https://api.open-meteo.com/v1/forecast
//        ?latitude=64.146&longitude=-21.940
//        &current=temperature_2m,weather_code,wind_speed_10m
//        &hourly=temperature_2m,precipitation
//        &daily=temperature_2m_max,temperature_2m_min,precipitation_sum
//        &timezone=auto
//        &temperature_unit=celsius
//
// The response is a flat JSON object whose top-level keys mirror the
// query (current/hourly/daily) — column-oriented arrays for the time
// series, scalars for current.

// ---- Per-API endpoint roots ---------------------------------------
//
// Rhai's `const` declarations aren't visible inside fn bodies, so
// the URLs are duplicated at each call site. Keep them here as a
// reference comment for grep convenience.
//
//   FORECAST_BASE      = "https://api.open-meteo.com/v1/forecast"
//   HISTORICAL_BASE    = "https://archive-api.open-meteo.com/v1/archive"
//   MARINE_BASE        = "https://marine-api.open-meteo.com/v1/marine"
//   AIR_QUALITY_BASE   = "https://air-quality-api.open-meteo.com/v1/air-quality"
//   SEASONAL_BASE      = "https://seasonal-api.open-meteo.com/v1/seasonal"
//   ENSEMBLE_BASE      = "https://ensemble-api.open-meteo.com/v1/ensemble"
//   FLOOD_BASE         = "https://flood-api.open-meteo.com/v1/flood"
//   CLIMATE_BASE       = "https://climate-api.open-meteo.com/v1/climate"
//   GEOCODING_BASE     = "https://geocoding-api.open-meteo.com/v1/search"
//   ELEVATION_BASE     = "https://api.open-meteo.com/v1/elevation"
//
// Per-API TTLs (seconds): see plugin.toml header for the rationale.
//   forecast=600 historical=86400 marine=600 air_quality=900
//   seasonal=21600 ensemble=900 flood=3600 climate=86400
//   geocode=86400 elevation=86400


// =====================================================================
// AGENT TOOL DISPATCH
// =====================================================================

fn tool_call(name, args, oauth) {
    if name == "open_meteo.geocode"      { return geocode_impl(args); }
    if name == "open_meteo.elevation"    { return elevation_impl(args); }
    if name == "open_meteo.forecast"     { return forecast_impl(args); }
    if name == "open_meteo.historical"   { return historical_impl(args); }
    if name == "open_meteo.marine"       { return marine_impl(args); }
    if name == "open_meteo.air_quality"  { return air_quality_impl(args); }
    if name == "open_meteo.seasonal"     { return seasonal_impl(args); }
    if name == "open_meteo.ensemble"     { return ensemble_impl(args); }
    if name == "open_meteo.flood"        { return flood_impl(args); }
    if name == "open_meteo.climate"      { return climate_impl(args); }
    if name == "open_meteo.render_chart" { return render_chart_impl(args); }
    throw "unknown tool '" + name + "'";
}


// =====================================================================
// open_meteo.geocode
// =====================================================================

fn geocode_impl(args) {
    let name = require_string(args.name, "name");
    if name.len() < 2 {
        throw "open_meteo.geocode: `name` must be at least 2 characters";
    }
    let count = clamp_int(args.count, 1, 20, 5);
    let language = if args.language != () && args.language != "" {
        args.language
    } else { "en" };

    let q = #{
        "name": name,
        "count": "" + count,
        "language": language,
        "format": "json",
    };
    let resp = http_get_cached(
        "https://geocoding-api.open-meteo.com/v1/search",
        q,
        "",
        86400,
    );
    let raw_results = if resp != () && resp.results != () && type_of(resp.results) == "array" {
        resp.results
    } else { [] };

    let out = [];
    for r in raw_results {
        out.push(#{
            "name":      if r.name != ()       { r.name }      else { "" },
            "country":   if r.country != ()    { r.country }   else { () },
            "admin1":    if r.admin1 != ()     { r.admin1 }    else { () },
            "latitude":  if r.latitude != ()   { r.latitude }  else { () },
            "longitude": if r.longitude != ()  { r.longitude } else { () },
            "elevation": if r.elevation != ()  { r.elevation } else { () },
            "timezone":  if r.timezone != ()   { r.timezone }  else { () },
            "population": if r.population != () { r.population } else { () },
        });
    }
    #{
        "query": name,
        "count": out.len(),
        "results": out,
    }
}


// =====================================================================
// open_meteo.elevation
// =====================================================================

fn elevation_impl(args) {
    // 2026-05-14 — fall back to the operator's default location.
    // Elevation accepts arrays of points too (open-meteo's API
    // supports batch lookup), so we only patch the scalar case;
    // an explicit array always wins.
    let merged = merge_with_defaults(args);
    let lat_raw = merged["latitude"];
    let lng_raw = merged["longitude"];
    if lat_raw == () || lng_raw == () {
        throw "open_meteo.elevation: no coordinates supplied and no default location configured. " +
              "Pass `latitude` + `longitude`, call `open_meteo.geocode` first, or set a default " +
              "in Settings → Plugins → Open-Meteo.";
    }
    let lat_str = numbers_to_csv(lat_raw, "latitude");
    let lng_str = numbers_to_csv(lng_raw, "longitude");

    let q = #{
        "latitude": lat_str,
        "longitude": lng_str,
    };
    let resp = http_get_cached(
        "https://api.open-meteo.com/v1/elevation",
        q,
        "",
        86400,
    );
    let elev = if resp != () && resp.elevation != () && type_of(resp.elevation) == "array" {
        resp.elevation
    } else { [] };
    #{
        "elevation": elev,
        "unit": "m",
    }
}


// =====================================================================
// open_meteo.forecast
// =====================================================================

fn forecast_impl(args) {
    // 2026-05-14 — merge defaults FIRST so the operator's pinned
    // location counts as "supplied." See merge_with_defaults for
    // the full rationale.
    let merged = merge_with_defaults(args);
    let coords = require_coords(merged);
    let lat = coords[0];
    let lng = coords[1];

    let q = base_query(lat, lng, merged);
    q = add_csv(q, "current",  merged["current"]);
    q = add_csv(q, "hourly",   merged["hourly"]);
    q = add_csv(q, "daily",    merged["daily"]);
    if merged["forecast_days"] != () { q["forecast_days"] = "" + merged["forecast_days"]; }
    if merged["past_days"] != ()     { q["past_days"]     = "" + merged["past_days"]; }
    if merged["models"] != () && type_of(merged["models"]) == "array" && merged["models"].len() > 0 {
        q["models"] = csv(merged["models"]);
    }

    let resp = http_get_cached(
        "https://api.open-meteo.com/v1/forecast",
        q,
        "",
        600,
    );
    annotate(resp, merged, pick_forecast_kind(merged))
}

fn pick_forecast_kind(merged) {
    // If the caller asked for a `daily` block, render the daily table;
    // otherwise the compact "current conditions" card.
    let d = merged["daily"];
    if d != () && type_of(d) == "array" && d.len() > 0 {
        return "weather_daily";
    }
    "weather_current"
}


// =====================================================================
// open_meteo.historical
// =====================================================================

fn historical_impl(args) {
    let merged = merge_with_defaults(args);
    let coords = require_coords(merged);
    let lat = coords[0];
    let lng = coords[1];
    let start = require_string(args["start_date"], "start_date");
    let end   = require_string(args["end_date"], "end_date");

    let q = base_query(lat, lng, merged);
    q["start_date"] = start;
    q["end_date"]   = end;
    q = add_csv(q, "hourly", merged["hourly"]);
    q = add_csv(q, "daily",  merged["daily"]);

    let resp = http_get_cached(
        "https://archive-api.open-meteo.com/v1/archive",
        q,
        "",
        86400,
    );
    annotate(resp, merged, "weather_daily")
}


// =====================================================================
// open_meteo.marine
// =====================================================================

fn marine_impl(args) {
    let merged = merge_with_defaults(args);
    let coords = require_coords(merged);
    let lat = coords[0];
    let lng = coords[1];

    let q = #{};
    q["latitude"]  = "" + lat;
    q["longitude"] = "" + lng;
    q["timezone"]  = merged["timezone"];
    q = add_csv(q, "current", merged["current"]);
    q = add_csv(q, "hourly",  merged["hourly"]);
    q = add_csv(q, "daily",   merged["daily"]);
    if args["length_unit"] != () && args["length_unit"] != "" {
        q["length_unit"] = args["length_unit"];
    }
    if args["forecast_days"] != () {
        q["forecast_days"] = "" + args["forecast_days"];
    }

    let resp = http_get_cached(
        "https://marine-api.open-meteo.com/v1/marine",
        q,
        "",
        600,
    );
    annotate(resp, merged, "weather_daily")
}


// =====================================================================
// open_meteo.air_quality
// =====================================================================

fn air_quality_impl(args) {
    let merged = merge_with_defaults(args);
    let coords = require_coords(merged);
    let lat = coords[0];
    let lng = coords[1];

    let q = #{};
    q["latitude"]  = "" + lat;
    q["longitude"] = "" + lng;
    q["timezone"]  = merged["timezone"];
    q = add_csv(q, "current", merged["current"]);
    q = add_csv(q, "hourly",  merged["hourly"]);
    if args["forecast_days"] != () {
        q["forecast_days"] = "" + args["forecast_days"];
    }
    if args["domains"] != () && args["domains"] != "" {
        q["domains"] = args["domains"];
    }

    let resp = http_get_cached(
        "https://air-quality-api.open-meteo.com/v1/air-quality",
        q,
        "",
        900,
    );
    annotate(resp, merged, "weather_current")
}


// =====================================================================
// open_meteo.seasonal
// =====================================================================

fn seasonal_impl(args) {
    let merged = merge_with_defaults(args);
    let coords = require_coords(merged);
    let lat = coords[0];
    let lng = coords[1];

    let q = #{};
    q["latitude"]  = "" + lat;
    q["longitude"] = "" + lng;
    q = add_csv(q, "six_hourly", args["six_hourly"]);
    q = add_csv(q, "daily",      merged["daily"]);
    if args["forecast_days"] != () {
        q["forecast_days"] = "" + args["forecast_days"];
    }
    if merged["temperature_unit"] != () && merged["temperature_unit"] != "" {
        q["temperature_unit"] = merged["temperature_unit"];
    }

    let resp = http_get_cached(
        "https://seasonal-api.open-meteo.com/v1/seasonal",
        q,
        "",
        21600,
    );
    annotate(resp, merged, "weather_daily")
}


// =====================================================================
// open_meteo.ensemble
// =====================================================================

fn ensemble_impl(args) {
    let merged = merge_with_defaults(args);
    let coords = require_coords(merged);
    let lat = coords[0];
    let lng = coords[1];

    let q = base_query(lat, lng, merged);
    q = add_csv(q, "hourly", merged["hourly"]);
    q = add_csv(q, "daily",  merged["daily"]);
    if args["models"] != () && type_of(args["models"]) == "array" && args["models"].len() > 0 {
        q["models"] = csv(args["models"]);
    }
    if args["forecast_days"] != () {
        q["forecast_days"] = "" + args["forecast_days"];
    }

    let resp = http_get_cached(
        "https://ensemble-api.open-meteo.com/v1/ensemble",
        q,
        "",
        900,
    );
    annotate(resp, merged, "weather_daily")
}


// =====================================================================
// open_meteo.flood
// =====================================================================

fn flood_impl(args) {
    let merged = merge_with_defaults(args);
    let coords = require_coords(merged);
    let lat = coords[0];
    let lng = coords[1];

    let q = #{};
    q["latitude"]  = "" + lat;
    q["longitude"] = "" + lng;
    q = add_csv(q, "daily", merged["daily"]);
    if args["forecast_days"] != () { q["forecast_days"] = "" + args["forecast_days"]; }
    if args["past_days"] != ()     { q["past_days"]     = "" + args["past_days"]; }
    if args["ensemble"] == true    { q["ensemble"] = "true"; }

    let resp = http_get_cached(
        "https://flood-api.open-meteo.com/v1/flood",
        q,
        "",
        3600,
    );
    annotate(resp, merged, "weather_daily")
}


// =====================================================================
// open_meteo.climate
// =====================================================================

fn climate_impl(args) {
    let merged = merge_with_defaults(args);
    let coords = require_coords(merged);
    let lat = coords[0];
    let lng = coords[1];
    let start = require_string(args["start_date"], "start_date");
    let end   = require_string(args["end_date"],   "end_date");
    if args["models"] == () || type_of(args["models"]) != "array" || args["models"].len() == 0 {
        throw "open_meteo.climate: `models` (non-empty array) is required";
    }

    let q = #{};
    q["latitude"]   = "" + lat;
    q["longitude"]  = "" + lng;
    q["start_date"] = start;
    q["end_date"]   = end;
    q["models"]     = csv(args["models"]);
    q = add_csv(q, "daily", merged["daily"]);
    if merged["temperature_unit"] != () && merged["temperature_unit"] != "" {
        q["temperature_unit"] = merged["temperature_unit"];
    }
    if merged["wind_speed_unit"] != () && merged["wind_speed_unit"] != "" {
        q["wind_speed_unit"] = merged["wind_speed_unit"];
    }

    let resp = http_get_cached(
        "https://climate-api.open-meteo.com/v1/climate",
        q,
        "",
        86400,
    );
    annotate(resp, merged, "weather_daily")
}


// =====================================================================
// open_meteo.render_chart
// =====================================================================
//
// The agent typically builds a ChartSpec directly from a prior tool's
// result and passes it here. The Rhai-side helper does light shape
// validation, then calls host_render_chart which:
//   1. parses the JSON spec into execlaw_charting::ChartSpec
//   2. renders SVG + PNG via plotters (pure-Rust)
//   3. stores PNG as a plugin artifact (TTL = ttl_seconds, default 7d)
//   4. returns { attachment_id, svg, png_data_url, ... }
//
// The plugin echoes the inline SVG in the tool_result so the SPA's
// chat-component dispatcher renders the chart inline; the
// attachment_id flows to the agent for {transport}.send_with_attachments.

fn render_chart_impl(args) {
    let spec = #{};
    spec["title"]     = if args["title"] != ()     { args["title"] }     else { "Chart" };
    spec["kind"]      = if args["kind"] != ()      { args["kind"] }      else { "line" };
    spec["x_label"]   = if args["x_label"] != ()   { args["x_label"] }   else { () };
    spec["y_label"]   = if args["y_label"] != ()   { args["y_label"] }   else { () };
    spec["y_unit"]    = if args["y_unit"] != ()    { args["y_unit"] }    else { () };
    spec["time_axis"] = if args["time_axis"] != () { args["time_axis"] == true } else { false };
    spec["series"]    = if args["series"] != ()    { args["series"] }    else { [] };
    if args["band"] != () {
        spec["band"] = args["band"];
    }

    let width  = if args["width"] != ()  { args["width"]  } else { 720 };
    let height = if args["height"] != () { args["height"] } else { 400 };
    let filename = if args["filename"] != () && args["filename"] != "" {
        args["filename"]
    } else {
        chart_default_filename(spec["title"])
    };
    let ttl = if args["ttl_seconds"] != () { args["ttl_seconds"] } else { 7 * 86400 };

    let spec_json = to_json_string(spec);
    let result = host_render_chart(spec_json, width, height, filename, ttl);

    #{
        "chat_component_kind": "chart",
        "title":          spec["title"],
        "attachment_id":  result["attachment_id"],
        "svg":            result["svg"],
        "png_data_url":   result["png_data_url"],
        "width":          result["width"],
        "height":         result["height"],
        "size_bytes":     result["size_bytes"],
    }
}

fn chart_default_filename(title) {
    // Sanitize title → lowercase, replace non-alnum runs with `-`,
    // strip leading/trailing dashes, append .png. Falls back to
    // "chart.png" for blank titles.
    if title == () || title == "" { return "chart.png"; }
    let s = lower(title);
    let out = "";
    let in_dash = false;
    for c in s {
        let ord = c.to_int();
        let is_lower = ord >= 97 && ord <= 122;
        let is_digit = ord >= 48 && ord <= 57;
        if is_lower || is_digit {
            out += "" + c;
            in_dash = false;
        } else {
            if !in_dash && out != "" {
                out += "-";
                in_dash = true;
            }
        }
    }
    while out.ends_with("-") { out.truncate(out.len() - 1); }
    if out == "" { return "chart.png"; }
    out + ".png"
}


// =====================================================================
// CONFIG + ADMIN ROUTES
// =====================================================================

fn admin_get_config(req) {
    let cfg = read_config();
    #{
        "place_name":          cfg.place_name,
        "default_latitude":    cfg.default_latitude,
        "default_longitude":   cfg.default_longitude,
        "default_timezone":    cfg.default_timezone,
        "temperature_unit":    cfg.temperature_unit,
        "wind_speed_unit":     cfg.wind_speed_unit,
        "precipitation_unit":  cfg.precipitation_unit,
        "default_chart_width":  cfg.default_chart_width,
        "default_chart_height": cfg.default_chart_height,
    }
}

fn admin_set_config(req) {
    let body = if req.body != () { req.body } else { #{} };
    let cfg = #{
        "place_name":         if body.place_name != ()         { body.place_name }         else { () },
        "default_latitude":   if body.default_latitude != ()   { body.default_latitude }   else { () },
        "default_longitude":  if body.default_longitude != ()  { body.default_longitude }  else { () },
        "default_timezone":   if body.default_timezone != ()   { body.default_timezone }   else { "auto" },
        "temperature_unit":   if body.temperature_unit != ()   { body.temperature_unit }   else { "celsius" },
        "wind_speed_unit":    if body.wind_speed_unit != ()    { body.wind_speed_unit }    else { "kmh" },
        "precipitation_unit": if body.precipitation_unit != () { body.precipitation_unit } else { "mm" },
        "default_chart_width":  if body.default_chart_width != ()  { body.default_chart_width }  else { 720 },
        "default_chart_height": if body.default_chart_height != () { body.default_chart_height } else { 400 },
    };
    vault_put("config", to_json_string(cfg));
    #{ "ok": true, "config": cfg }
}

fn admin_test(req) {
    let cfg = read_config();
    let lat = if cfg.default_latitude  != () { cfg.default_latitude  } else { 64.146 };
    let lng = if cfg.default_longitude != () { cfg.default_longitude } else { -21.940 };
    let q = #{
        "latitude":  "" + lat,
        "longitude": "" + lng,
        "current":   "temperature_2m,weather_code",
        "timezone":  "auto",
    };
    let resp = http_get_cached(
        "https://api.open-meteo.com/v1/forecast",
        q,
        "",
        60,
    );
    if resp == () || resp.current == () {
        throw "open-meteo: no response from Open-Meteo (network / DNS / firewall)";
    }
    #{
        "ok": true,
        "latitude":  lat,
        "longitude": lng,
        "current":   resp.current,
    }
}

fn read_config() {
    // Wrap vault_get in try/catch so the plugin still works in test
    // fixtures that build a ScriptEngine without host capabilities
    // (the binding errors at call time rather than returning ()).
    // Production never hits this branch; tests get fallback defaults.
    let raw = ();
    try {
        raw = vault_get("config");
    } catch (e) {
        // Host caps unavailable — fall through to fallback defaults.
    }
    let fallback = #{
        "place_name":           (),
        "default_latitude":     (),
        "default_longitude":    (),
        "default_timezone":     "auto",
        "temperature_unit":     "celsius",
        "wind_speed_unit":      "kmh",
        "precipitation_unit":   "mm",
        "default_chart_width":  720,
        "default_chart_height": 400,
    };
    if raw == () || raw == "" { return fallback; }
    let parsed = ();
    try {
        parsed = parse_json(raw);
    } catch (e) {
        // Malformed vault JSON — fall back to defaults rather than
        // crashing every tool call. Operator can re-save in the UI.
        log_warn("open-meteo: vault config JSON unparseable: " + e);
    }
    if parsed == () || type_of(parsed) != "map" { return fallback; }
    // Merge so an old vault row missing newer fields still works.
    for k in fallback.keys() {
        if parsed[k] == () { parsed[k] = fallback[k]; }
    }
    parsed
}


// =====================================================================
// HELPERS
// =====================================================================

fn merge_with_defaults(args) {
    let cfg = read_config();
    // Bracket-access throughout — Rhai's `.field` lookup on a Dynamic
    // Map occasionally fails to find keys when the property name
    // collides with a registered method name on Array (e.g. `current`).
    // Bracket access is unambiguous and slightly faster anyway.
    //
    // 2026-05-14 — `latitude` + `longitude` now ride through the same
    // arg-or-default merge. Pre-rework each `*_impl` did
    // `require_number(args["latitude"], ...)` BEFORE merging, which
    // threw whenever the agent omitted lat/lon — even though the
    // operator had pinned a default in Settings → Plugins → Open-Meteo.
    // The agent then asked the user "what's your location?" because
    // the schema marked them required and the tool errored without
    // them. Now the merge happens first; require_number runs against
    // `merged[...]` so the operator default counts as "supplied."
    let merged = #{};
    merged["latitude"]           = if args["latitude"] != ()           { args["latitude"] }           else { cfg.default_latitude };
    merged["longitude"]          = if args["longitude"] != ()          { args["longitude"] }          else { cfg.default_longitude };
    merged["place_name"]         = if args["place_name"] != ()         { args["place_name"] }         else { cfg.place_name };
    merged["timezone"]           = if args["timezone"] != ()           { args["timezone"] }           else { cfg.default_timezone };
    merged["temperature_unit"]   = if args["temperature_unit"] != ()   { args["temperature_unit"] }   else { cfg.temperature_unit };
    merged["wind_speed_unit"]    = if args["wind_speed_unit"] != ()    { args["wind_speed_unit"] }    else { cfg.wind_speed_unit };
    merged["precipitation_unit"] = if args["precipitation_unit"] != () { args["precipitation_unit"] } else { cfg.precipitation_unit };
    merged["current"]       = args["current"];
    merged["hourly"]        = args["hourly"];
    merged["daily"]         = args["daily"];
    merged["models"]        = args["models"];
    merged["forecast_days"] = args["forecast_days"];
    merged["past_days"]     = args["past_days"];
    merged
}

// 2026-05-14 — shared error message for the lat/lon-missing path.
// Centralised so the same hint about the operator default appears
// in every per-tool error, and the agent gets a clear suggestion
// instead of a generic "field is required" stub. The agent reads
// throw messages back as tool errors, and this phrasing nudges it
// to either call `open_meteo.geocode` first or surface the missing
// config to the operator instead of pestering the user.
fn require_coords(merged) {
    if merged["latitude"] == () || merged["longitude"] == () {
        throw "open-meteo: no coordinates supplied and no default location configured. " +
              "Either pass `latitude` + `longitude`, call `open_meteo.geocode` to resolve a " +
              "place name to coordinates first, or ask the operator to set a default location " +
              "in Settings → Plugins → Open-Meteo.";
    }
    [merged["latitude"], merged["longitude"]]
}

fn base_query(lat, lng, merged) {
    let q = #{};
    q["latitude"]  = "" + lat;
    q["longitude"] = "" + lng;
    q["timezone"]  = merged["timezone"];
    if merged["temperature_unit"]   != () && merged["temperature_unit"]   != "" { q["temperature_unit"]   = merged["temperature_unit"]; }
    if merged["wind_speed_unit"]    != () && merged["wind_speed_unit"]    != "" { q["wind_speed_unit"]    = merged["wind_speed_unit"]; }
    if merged["precipitation_unit"] != () && merged["precipitation_unit"] != "" { q["precipitation_unit"] = merged["precipitation_unit"]; }
    q
}

// Rhai passes Maps by value, so the caller must reassign the return:
//   q = add_csv(q, "current", merged["current"]);
// Inlining is uglier but using a return-shaped helper keeps the
// per-tool dispatch handlers readable.
fn add_csv(q, key, value) {
    if value == () || type_of(value) != "array" || value.len() == 0 { return q; }
    q[key] = csv(value);
    q
}

fn csv(arr) {
    let out = "";
    let first = true;
    for v in arr {
        if first { first = false; } else { out += ","; }
        out += "" + v;
    }
    out
}

fn numbers_to_csv(value, label) {
    let t = type_of(value);
    if t == "f64" || t == "float" || t == "i64" || t == "int" {
        return "" + value;
    }
    if t == "array" {
        if value.len() == 0 { throw "open-meteo: `" + label + "` array must not be empty"; }
        return csv(value);
    }
    throw "open-meteo: `" + label + "` must be a number or number array";
}

fn annotate(resp, merged, chat_component_kind) {
    if resp == () { resp = #{}; }
    resp["chat_component_kind"] = chat_component_kind;
    if merged["place_name"] != () && merged["place_name"] != "" {
        resp["place_name"] = merged["place_name"];
    }
    if merged["temperature_unit"] != () {
        resp["temperature_unit"] = merged["temperature_unit"];
    }
    if merged["wind_speed_unit"] != () {
        resp["wind_speed_unit"] = merged["wind_speed_unit"];
    }
    if merged["precipitation_unit"] != () {
        resp["precipitation_unit"] = merged["precipitation_unit"];
    }
    resp
}

fn require_string(value, label) {
    if value == () || value == "" {
        throw "open-meteo: `" + label + "` is required";
    }
    if type_of(value) != "string" {
        throw "open-meteo: `" + label + "` must be a string";
    }
    trim(value)
}

fn require_number(value, label) {
    if value == () { throw "open-meteo: `" + label + "` is required"; }
    let t = type_of(value);
    if t == "f64" || t == "float" || t == "i64" || t == "int" { return value; }
    if t == "string" {
        let f = host_parse_float(value);
        if f != () { return f; }
        let n = host_parse_int(value);
        if n != () { return n.to_float(); }
    }
    throw "open-meteo: `" + label + "` must be a number";
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
// TEST HOOKS (called from crates/script/tests/open_meteo_plugin.rs)
//
// These thin wrappers expose pure-Rhai helpers without going through
// the live host stack — same convention as discord/signal/etc. let
// us pin the helper functions' behavior without standing up a real
// HTTP endpoint.

fn _test_chart_filename(title)            { chart_default_filename(title) }
fn _test_csv(arr)                          { csv(arr) }
fn _test_clamp_int(v, lo, hi, fb)          { clamp_int(v, lo, hi, fb) }
fn _test_numbers_to_csv(v, label)          { numbers_to_csv(v, label) }
fn _test_pick_forecast_kind(daily_block)   { pick_forecast_kind(#{ "daily": daily_block }) }

// 2026-05-14 — exposed for the lat/lon-default regression test.
// Returns the merged map so the test can assert that operator
// defaults (read from `read_config`'s fallback path when no vault
// is wired) flow into latitude/longitude as expected.
fn _test_merge_with_defaults(args)         { merge_with_defaults(args) }
// Mirror of `require_coords` so the test can assert the throw
// message points at the operator settings page when neither args
// nor defaults supply coordinates. Returns [lat, lng] on success;
// throws on failure (caught with try/catch in the test).
fn _test_require_coords(merged)            { require_coords(merged) }
