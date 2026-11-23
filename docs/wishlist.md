# `zcast` Feature Wishlist

> Ideas and future enhancements to explore someday — not scheduled for immediate implementation yet. Add freely below!

---

## Authoring & Editor

- **Full-screen "Expand" editor mode**
  - Add an **Expand** button in the top-right chrome of the editor/card pane that opens a distraction-free full-screen editing mode (with an easy collapse / `Esc` exit back to the sliding-pane view).
- **Autosave & draft resilience**
  - *(Note: `zcast-editor-inline` currently does 800ms debounced autosave + `Cmd+S` while editing.)*
  - Future polish: offline/`localStorage` crash-recovery backup for unsaved drafts, clearer persistent autosave status/toggle, and protection against accidental tab close with unsaved changes.
- **Revision history (`zcast-revisions` module)**
  - Track snapshots/diffs of note edits over time (stored via `ModuleStore`, per DEC-03 / DEC-09).
  - Include a setting/option to keep revision history **private** (admin-only) or expose it **publicly** (readers can browse how a note evolved).

---

## Discovery & Graph Navigation

- **Orphaned notes view**
  - A way to surface notes with no links (no incoming backlinks and/or no outgoing `[[wikilinks]]`) so nothing gets lost in the Zettelkasten.
- **Index of all notes**
  - A clean, browsable/sortable directory of all notes (respecting `private` visibility for public visitors vs. admin), e.g., alphabetical, recently updated, or most linked.

---

## API & AI Agent Access (ChatGPT, Claude, Local Coding Agents)

- **Scoped REST API (`/_zcast/api/v1/...`) + OpenAPI spec**
  - Clean token-authenticated JSON API for reading, searching, inspecting backlinks/graph links, and (optionally) creating/updating notes.
  - Scoped personal access tokens (stored hashed in `ModuleStore`): choose **read-only vs. read-write** and **public-only vs. include-private** per token.
  - Expose `/_zcast/api/v1/openapi.json` so tools like ChatGPT Custom Actions or Raycast can connect with minimal setup.
- **Recommended ways for a local/remote agent to connect:**
  1. **MCP server (`zcast mcp` / `zcast-mcp` module) — best for ChatGPT, Claude Desktop, and IDE agents:**
     - **Local (`stdio`):** `zcast mcp` runs directly over stdin/stdout against the local DB or running server (defaults to `--read-only`, with an explicit `--include-private` opt-in). Exposes typed tools (`search_notes`, `get_note`, `get_backlinks`, `list_notes`, `list_orphans`) and `zcast://note/<slug>` resources.
     - **Remote (Streamable HTTP at `/_zcast/mcp`):** Lets web/cloud agents (like ChatGPT Connectors) talk directly to a hosted VPS instance using a scoped bearer token.
  2. **JSON-first CLI (`zcast notes search | read | backlinks | list --json`) — best for local terminal/coding agents:**
     - Zero-config for agents that have shell access; works locally against SQLite or against a remote server via `--url` + `ZCAST_API_TOKEN`.
  3. **Start read-only by default:** Give agents safe read/search/backlink access first, with optional draft-only creation (`create_draft`) before full write permissions.
