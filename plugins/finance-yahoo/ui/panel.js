// Yahoo Finance admin panel — hand-coded ES module (no JSX), mirrors
// the bridge surface used by open-meteo / google-places panels.
//
// What it renders:
//   1. Persistent "personal use only" disclaimer banner (Yahoo ToS).
//   2. Default-symbols list editor (comma-separated tickers — what
//      the chat-card sparkline uses when the operator opens the
//      plugin without picking a symbol).
//   3. Test button that round-trips quote/index/crypto/fx + the
//      crumb-protected quote_summary endpoint via POST /test.
//
// Kept deliberately small (no build step) so the plugin zip is
// drop-in installable without going through scripts/build-plugin-ui.

const React = globalThis.execlawHost.React;
const { useCallback, useEffect, useState } = React;
const e = React.createElement;

const TOS_NOTE =
  "This plugin uses Yahoo Finance's undocumented public endpoints, which Yahoo's terms permit for personal, non-commercial use only. By enabling this plugin you confirm you'll use it that way. The plugin is not affiliated with or endorsed by Yahoo, Inc.";

const Panel = (props) => {
  const { bridge } = props;
  const { ErrorBanner, Button } = bridge.components;
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState(null);
  const [savedMsg, setSavedMsg] = useState(null);
  const [config, setConfig] = useState(null);
  const [symbolsText, setSymbolsText] = useState("");
  const [testState, setTestState] = useState({ kind: "idle" });

  const loadConfig = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const cfg = await bridge.fetchJson(
        "GET",
        "/api/admin/plugins/finance-yahoo/config",
      );
      setConfig(cfg);
      const syms = Array.isArray(cfg.default_symbols) ? cfg.default_symbols : [];
      setSymbolsText(syms.join(", "));
    } catch (err) {
      setError(err instanceof Error ? err.message : "couldn't load config");
    } finally {
      setLoading(false);
    }
  }, [bridge]);

  useEffect(() => {
    loadConfig();
  }, [loadConfig]);

  const onSave = useCallback(async () => {
    setSaving(true);
    setError(null);
    setSavedMsg(null);
    try {
      const symbols = symbolsText
        .split(/[,\n]/)
        .map((s) => s.trim())
        .filter((s) => s.length > 0);
      await bridge.fetchJson("POST", "/api/admin/plugins/finance-yahoo/config", {
        default_symbols: symbols,
        tos_ack: true,
      });
      setSavedMsg("Saved.");
      await loadConfig();
    } catch (err) {
      setError(err instanceof Error ? err.message : "save failed");
    } finally {
      setSaving(false);
    }
  }, [bridge, symbolsText, loadConfig]);

  const onTest = useCallback(async () => {
    setSaving(true);
    setError(null);
    setTestState({ kind: "idle" });
    try {
      const r = await bridge.fetchJson(
        "POST",
        "/api/admin/plugins/finance-yahoo/test",
      );
      if (r.ok === false) {
        setTestState({
          kind: "err",
          message: "Some asset classes failed — see details below.",
          results: r.results || [],
        });
      } else {
        setTestState({
          kind: "ok",
          message: "All asset classes reachable.",
          results: r.results || [],
        });
      }
    } catch (err) {
      setTestState({
        kind: "err",
        message: err instanceof Error ? err.message : String(err),
        results: [],
      });
    } finally {
      setSaving(false);
    }
  }, [bridge]);

  if (loading) {
    return e(
      "div",
      { className: "d-flex align-items-center execlaw-muted" },
      e("span", {
        className: "spinner-border spinner-border-sm me-2",
        role: "status",
        "aria-hidden": true,
      }),
      "Loading…",
    );
  }

  return e(
    "div",
    { "data-testid": "finance-yahoo-config-page" },
    // ToS banner — persistent, not dismissible.
    e(
      "div",
      {
        className: "alert alert-warning",
        "data-testid": "finance-yahoo-tos-banner",
      },
      e("strong", null, "Personal use only. "),
      TOS_NOTE,
    ),
    e(ErrorBanner, {
      message: error,
      onDismiss: () => setError(null),
      className: "mb-3",
    }),
    e(
      "div",
      { className: "card mb-3" },
      e(
        "div",
        { className: "card-body" },
        e("h5", { className: "h6 mb-2" }, "Default watchlist"),
        e(
          "p",
          { className: "execlaw-muted small mb-2" },
          "Comma-separated symbols the chat-card sparkline shows when the operator opens this plugin without picking one. Use any Yahoo symbol — bare ticker (",
          e("code", null, "AAPL"),
          "), caret index (",
          e("code", null, "^DJI"),
          "), crypto (",
          e("code", null, "BTC-USD"),
          "), FX (",
          e("code", null, "EURUSD=X"),
          ").",
        ),
        savedMsg &&
          e(
            "div",
            {
              className: "alert alert-success",
              "data-testid": "finance-yahoo-saved",
            },
            savedMsg,
          ),
        e("input", {
          type: "text",
          className: "form-control",
          placeholder: "AAPL, ^DJI, ^IXIC, BTC-USD",
          value: symbolsText,
          onChange: (ev) => setSymbolsText(ev.target.value),
          "data-testid": "finance-yahoo-symbols-input",
        }),
        e(
          "div",
          { className: "form-text execlaw-muted" },
          config && config.tos_ack_at
            ? "Personal-use disclaimer acknowledged on " +
                new Date(config.tos_ack_at * 1000).toLocaleDateString() +
                "."
            : "Saving will record your personal-use acknowledgment.",
        ),
      ),
    ),
    e(
      "div",
      { className: "d-flex gap-2 mb-3" },
      e(
        Button,
        {
          variant: "primary",
          size: "sm",
          onClick: onSave,
          disabled: saving,
          "data-testid": "finance-yahoo-save",
        },
        "Save",
      ),
      e(
        Button,
        {
          variant: "outline-secondary",
          size: "sm",
          onClick: onTest,
          disabled: saving,
          "data-testid": "finance-yahoo-test",
        },
        "Test connectivity",
      ),
    ),
    testState.kind !== "idle" &&
      e(
        "div",
        {
          className:
            testState.kind === "ok" ? "alert alert-success" : "alert alert-danger",
          "data-testid": "finance-yahoo-test-result",
        },
        e("div", null, testState.message),
        Array.isArray(testState.results) && testState.results.length > 0 &&
          e(
            "ul",
            { className: "mb-0 mt-2 small" },
            testState.results.map((r) =>
              e(
                "li",
                { key: r.label },
                e("strong", null, r.label),
                ": ",
                r.ok ? "ok" : "fail",
                r.detail && r.detail.price !== undefined
                  ? " (" + r.detail.price + ")"
                  : "",
                r.detail && r.detail.error
                  ? " — " + r.detail.error
                  : "",
              ),
            ),
          ),
      ),
    e(
      "div",
      { className: "card" },
      e(
        "div",
        { className: "card-body" },
        e("h5", { className: "h6 mb-2" }, "Endpoint status"),
        e(
          "p",
          { className: "execlaw-muted small mb-0" },
          "v0.1.0 covers the chart endpoint (quotes, indices, crypto, FX, candles) and the search endpoint (symbol lookup, news). The crumb-protected ",
          e("code", null, "quote_summary"),
          " endpoint is also wired — the cookie+crumb session refreshes transparently every 12 h. Yahoo's WebSocket trade stream is on the v0.2 roadmap.",
        ),
      ),
    ),
  );
};

export default Panel;
