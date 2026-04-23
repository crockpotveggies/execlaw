# execlaw-plugin-sdk

Manifest parsing (`plugin.toml`) and ZIP-staging for execlaw's
**hook-declaration** plugin model (§4.2, 2026-04-23 locked).

A plugin is a plugin. No typed "kinds" — a manifest declares which hook
points it attaches to (tools, transport, identity_provider, inference_backend,
hardware_probe, oauth_accounts, ui_panels, chat_components, event_subscriptions,
alert_sources, health_checks, skills, services). One plugin can attach to many.
