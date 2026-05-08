# execlaw — UI screenshots

Drop SPA / admin-panel screenshots here for inline embedding in
`README.md`, `docs/architecture.md`, `docs/agent-model.md`,
`docs/plugins.md`, etc.

## Conventions

- **Format**: PNG for UI shots, SVG for diagrams, WebP if you need <100 KB.
- **Size**: keep each file under ~500 KB. If a shot needs to be larger
  than that, link to a Git LFS-tracked path or a CDN — don't bloat the
  bare repo.
- **Naming**: kebab-case + describe-what-it-shows: `control-thread.png`,
  `plugin-install-flow.png`, `whatsapp-pairing-qr.png`,
  `approval-card.png`.
- **Resolution**: 1× for dense screens (laptop ~1440×900) is fine.
  Avoid 2× / Retina dumps — they triple the file size for no doc value.
- **Crop**: trim browser chrome / OS chrome unless it's relevant to
  what's being demonstrated.

## Referencing in markdown

```markdown
![Control thread](docs/screenshots/control-thread.png)
```

From inside `docs/`:

```markdown
![Control thread](screenshots/control-thread.png)
```

## What's worth capturing

When the UI is steady enough to immortalise, the highest-value shots are:

1. The pinned **Control thread** with messages from multiple channels
   collapsed into one stream.
2. The **approval card** for a sensitive-tool / cold-contact flow.
3. The **plugin install** dialog + per-plugin admin panel (Signal QR
   pairing, WhatsApp pairing, Slack OAuth, Google Calendar OAuth).
4. The **research progress** page mid-session.
5. A **voice session** with VAD + TTS visualisation (lands with Phase 8
   audio plugins).

`.gitkeep` reserves the directory in git so the path doesn't 404
before the first real screenshot lands.
