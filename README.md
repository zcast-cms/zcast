# `zcast`

A fast, modular Rust Zettelkasten CMS built on a contract-oriented microkernel.

## Repositories in `zcast-cms`
- [`zcast-interfaces`](https://github.com/zcast-cms/zcast-interfaces) — Versioned Rust contracts and the minimal `Note` primitive.
- [`zcast`](https://github.com/zcast-cms/zcast) — Microkernel, CLI, docs, and first-party standard modules.
- [`zcast-theme-davidhewitson`](https://github.com/zcast-cms/zcast-theme-davidhewitson) — Editorial dark/light/auto sliding-panes theme inspired by `davidhewitson.co.uk`.

## Quick Start (`v0.1` Local Prototype)

```bash
cargo run -p zcast-cli -- serve
```

Then open [http://127.0.0.1:3000](http://127.0.0.1:3000).

## Documentation
- [Architecture Blueprint & Decision Log](docs/architecture-planning.md)
- [Theme System Design](docs/themes-design.md)
- [Contract Sketches](docs/contracts-sketch.md)
- [Installer & Updater UX](docs/installer-ux.md)
