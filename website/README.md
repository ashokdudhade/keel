# Keel website (React)

Marketing site for Keel — Cursor and Claude Code.

```bash
cd website
npm install
npm run dev      # http://127.0.0.1:5173/keel/
npm run build
npm run preview
```

Fonts (self-hosted via Fontsource):

- **Space Grotesk Variable** — display / brand
- **Source Sans 3 Variable** — body
- **IBM Plex Mono** — code

GitHub Pages deploys `website/dist` with `base: /keel/` via `.github/workflows/pages.yml`.
