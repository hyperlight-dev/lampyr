# Lampyr website

This directory contains the fully static Astro + Starlight website published at
<https://hyperlight-dev.github.io/lampyr/>.

The canonical documentation remains in [`../docs`](../docs). The content
collection loads those Markdown files directly at build time and rewrites their
repository-relative links for the Pages base path.

The site builds for <https://hyperlight-dev.github.io/lampyr/> by default.
`SITE_ORIGIN` and `SITE_BASE_PATH` override the origin and base path; an empty
`SITE_BASE_PATH` serves the site from the root, as a private Pages site is. The
Pages workflow sets both from what GitHub Pages reports.

From the repository root:

```bash
just site-check
```

For local development:

```bash
just site-run
```

The server remains attached to the terminal. Press `Ctrl+C` to stop it.
