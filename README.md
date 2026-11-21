<p align="center"><img src="web/icon.svg" width="96" alt=""></p>

<h1 align="center">MindCrab</h1>

<p align="center">
A self-hosted, Obsidian-style Markdown notebook written in Rust — with a built-in
<b>MCP server</b> so Claude, Gemini, Codex and other AI assistants can use your notes as <b>long-term memory</b>.
</p>

<p align="center">
<a href="#quick-start">Quick start</a> ·
<a href="#connect-an-ai-mcp">Connect an AI</a> ·
<a href="#configuration">Configuration</a> ·
<a href="#türkçe-hızlı-başlangıç">Türkçe</a>
</p>

---

## Why

Your notes are plain `.md` files in a folder you own. MindCrab gives that folder:

- a fast **Material You** web app (desktop + phone, installable as a PWA),
- a **full-text index** with backlinks, tags and a graph — plus optional **semantic search** (search by meaning),
- an **MCP endpoint** with memory-focused tools, so every AI you use reads from and writes to the *same* memory — and you can see, edit and undo everything it wrote.

One ~10 MB static binary, no database server, no CDN, no telemetry. Runs fine on a 1 GB VPS.

## Features

**Notebook (Obsidian-compatible)**
- `[[Wikilinks]]`, `[[Note|alias]]`, `[[Note#Heading]]`, embeds `![[image.png]]`, relative Markdown links
- Rename / move a note **or a whole folder** → every link pointing at the moved notes is rewritten
- Backlinks with context, outgoing links (missing ones are one click to create), **unlinked mentions**
- `#tags`, nested `#tags/like/this` and YAML frontmatter `tags:`
- GitHub/Obsidian callouts (`> [!TIP]`), tables, footnotes, task lists (tick them in preview)
- Edit · split · preview modes, autosave, `[[` autocomplete, list continuation, paste/drop images
- Full-text search (SQLite FTS5) that ignores diacritics — `seker` finds `şeker` — with `tag:`, `path:` and `"phrase"` filters
- Force-directed graph (global or around the current note)
- Daily notes (`Journal/YYYY-MM-DD.md`) and **templates** (`Templates/`, Obsidian-style `{{title}}`, `{{date}}`, `{{time}}`, `{{date:DD.MM.YYYY}}`) — pick one for a new note or insert it from the toolbar
- **Semantic search** (optional): finds notes by meaning, not just words — “sunucu” finds a note about your “Oracle makinesi”. Hybrid ranking (keywords + vectors), “related notes” in the side panel. Works with Ollama (fully local), Gemini, OpenAI or any OpenAI-compatible embeddings API
- **Works offline** (installable PWA): opened notes stay readable without a connection, “download all notes” for the whole vault, edits made offline are kept on the device and synced (with conflict check) when you're back
- Works on an **existing Obsidian vault**; edits made in Obsidian, Syncthing, git or any editor are picked up live (inotify + periodic rescan)

**Memory for AI**
- MCP over Streamable HTTP (`/mcp`) *and* stdio (`mindcrab mcp`)
- `memory_overview` → the assistant reads your `MEMORY.md` guide, memory topics, recent notes and tags first
- `search_notes` uses keywords **and meaning** when semantic search is on; `related_notes` surfaces notes the user never linked
- `remember` → appends a dated fact to `Memory/<Topic>.md`, tagged with which AI wrote it
- Surgical edits (`edit_note`, `append_to_note` under a heading) and optimistic locking (`expected_hash`) so an AI never silently overwrites your changes
- **Every change is versioned** — by you, the web UI, an AI or an external editor — with a diff view and one-click restore
- Deletes go to `.trash/`, never `rm`
- Optional read-only MCP mode and a separate MCP token you can rotate without logging yourself out

**Design**
- Material 3 colour roles generated from one seed hue (OKLCH), 8 presets + custom colour + monochrome, light/dark/system
- Roboto Flex and Material Symbols bundled (no external requests)
- Turkish and English UI (follows the browser language)

## Quick start

### Docker Compose

```yaml
services:
  mindcrab:
    image: ghcr.io/mustafabayramguven/mindcrab:latest
    restart: unless-stopped
    ports: ["3436:3436"]
    volumes: ["./vault:/data/vault"]
    environment:
      MINDCRAB_TOKEN: change-me-to-a-long-random-string
    user: "1000:1000"
```

```sh
mkdir vault && docker compose up -d
```

Open `http://your-server:3436` and sign in with the token. If you leave `MINDCRAB_TOKEN` empty, one is generated, printed in `docker logs mindcrab` and saved to `vault/.mindcrab/mindcrab_token`.

### Binary

```sh
cargo build --release
MINDCRAB_VAULT=~/Notes ./target/release/mindcrab        # serves on :3436
```

### Behind Caddy (HTTPS)

```caddy
notes.example.com {
    reverse_proxy mindcrab:3436
}
```

Set `MINDCRAB_COOKIE_SECURE=true` once it is served over HTTPS. claude.ai connectors need a public HTTPS URL.

## Connect an AI (MCP)

**Settings → Connect AI** in the web app shows your endpoint, token and copy-ready snippets. Endpoint: `https://your-host/mcp`, auth: `Authorization: Bearer <token>`.

**Claude Code**
```sh
claude mcp add --transport http mindcrab https://your-host/mcp \
  --header "Authorization: Bearer $MINDCRAB_TOKEN"
```

**claude.ai / Claude Desktop** — *Settings → Connectors → Add custom connector* and paste the URL that contains the token:
```
https://your-host/mcp/<token>
```
(Custom connectors can't send headers, so the token goes in the path. Treat that URL like a password.)

**Gemini CLI** — `~/.gemini/settings.json`
```json
{
  "mcpServers": {
    "mindcrab": {
      "httpUrl": "https://your-host/mcp",
      "headers": { "Authorization": "Bearer <token>" }
    }
  }
}
```
or `gemini mcp add --transport http mindcrab https://your-host/mcp --header "Authorization: Bearer <token>"`.

**Codex CLI** — `~/.codex/config.toml`
```toml
[mcp_servers.mindcrab]
url = "https://your-host/mcp"
bearer_token_env_var = "MINDCRAB_TOKEN"
```

**Local stdio** (client launches the binary, no server needed)
```json
{ "mcpServers": { "mindcrab": { "command": "mindcrab", "args": ["mcp"], "env": { "MINDCRAB_VAULT": "/home/me/Notes" } } } }
```

### Tools

| Tool | What it does |
|---|---|
| `memory_overview` | `MEMORY.md`, memory topics, recently changed notes, top tags — call first |
| `search_notes` | Ranked search (keywords + meaning) with snippets; `tag:`, `path:`, `"phrase"`; `mode` = auto/keyword/semantic |
| `read_note` | Full Markdown + hash + backlinks (accepts a path, name or title) |
| `list_notes` | By folder, alphabetical or most recent |
| `get_links` | Backlinks with context, outgoing links, unlinked mentions |
| `related_notes` | Notes closest in meaning (semantic search) |
| `list_tags` | Tags with counts |
| `daily_note` | Read or append to today's (or any day's) journal |
| `remember` | Save one durable fact to `Memory/<topic>.md` |
| `write_note` | Create / replace (with `expected_hash`) |
| `edit_note` | Replace one exact snippet |
| `append_to_note` | Append to the end or under a heading |
| `move_note` | Rename/move a note or folder and fix links |
| `delete_note` | Move to trash |
| `note_history` / `restore_revision` | List, read and restore earlier versions |

With `MINDCRAB_MCP_READ_ONLY=true` only the read tools are exposed.

### How the memory is organised

```
vault/
├── MEMORY.md          ← the guide + index every assistant reads first (you own it)
├── Memory/
│   ├── Inbox.md       ← facts without a topic
│   ├── Preferences.md ← - Prefers Fish shell — _2026-11-21, claude-code_
│   └── Servers.md
├── Journal/2026-11-21.md
└── … your own notes and folders
```

Edit `MEMORY.md` to tell assistants how *you* want them to use the vault (what to store, where, in which language). The server also sends short instructions on `initialize`.

## Semantic search

Off by default. Point MindCrab at any OpenAI-compatible `/embeddings` endpoint and it chunks every note, embeds the chunks in the background (only changed chunks are re-embedded) and fuses vector hits with keyword hits. Without it everything else works the same.

**Gemini** (free tier, good multilingual quality — fine for a small VPS):

```yaml
      MINDCRAB_EMBED_URL: https://generativelanguage.googleapis.com/v1beta/openai
      MINDCRAB_EMBED_MODEL: gemini-embedding-001
      MINDCRAB_EMBED_KEY: <your Gemini API key>
      MINDCRAB_EMBED_DIMS: "768"          # optional, smaller vectors
```

**Ollama** (fully local — needs a machine with ~1 GB free RAM for the model):

```sh
ollama pull embeddinggemma      # multilingual; or bge-m3, qwen3-embedding, nomic-embed-text
```
```yaml
      MINDCRAB_EMBED_URL: http://ollama:11434/v1
      MINDCRAB_EMBED_MODEL: embeddinggemma
      # nomic-embed-text needs prefixes:
      # MINDCRAB_EMBED_QUERY_PREFIX: "search_query: "
      # MINDCRAB_EMBED_DOC_PREFIX: "search_document: "
```

Changing the model re-embeds the vault automatically. Progress and errors are shown in **Settings → Semantic search**; if the API is down, search quietly falls back to keywords. Note text is sent to the embeddings provider you configure — use Ollama if it must stay on your hardware.

## Offline

Install MindCrab as an app (browser menu → *Install* / *Add to Home screen*). Needs HTTPS (or `localhost`).
Notes you open are cached on the device; **Settings → Offline → Download all notes** caches the whole vault. Edits made offline are stored locally, survive reloads, and are saved when the connection returns — if the note changed elsewhere meanwhile you choose which version wins (the other one stays in History). *Sign out* clears the device cache.

## Configuration

All options are flags or environment variables.

| Variable | Default | |
|---|---|---|
| `MINDCRAB_VAULT` | `./vault` | Folder with the notes |
| `MINDCRAB_DATA` | `<vault>/.mindcrab` | Index (`index.db`) and generated tokens |
| `MINDCRAB_HOST` / `MINDCRAB_PORT` | `0.0.0.0` / `3436` | Listen address |
| `MINDCRAB_TOKEN` | generated | Web UI + REST API token (≥ 16 chars) |
| `MINDCRAB_MCP_TOKEN` | = web token | Separate token for AI clients |
| `MINDCRAB_MCP_READ_ONLY` | `false` | Expose only read tools |
| `MINDCRAB_COOKIE_SECURE` | `false` | `Secure` cookie (use with HTTPS) |
| `MINDCRAB_DAILY_DIR` | `Journal` | Daily notes folder |
| `MINDCRAB_MEMORY_DIR` | `Memory` | Folder for `remember` |
| `MINDCRAB_TEMPLATES_DIR` | `Templates` | Template notes (`Daily.md` shapes daily notes) |
| `MINDCRAB_EMBED_URL` | – | OpenAI-compatible embeddings base URL (enables semantic search) |
| `MINDCRAB_EMBED_MODEL` | – | Embedding model name |
| `MINDCRAB_EMBED_KEY` | – | API key (Bearer), if the provider needs one |
| `MINDCRAB_EMBED_DIMS` | – | Output dimensions, for APIs that support it |
| `MINDCRAB_EMBED_QUERY_PREFIX` / `_DOC_PREFIX` | – | Prefixes some models expect |
| `MINDCRAB_EMBED_MIN_SCORE` | `0` | Minimum cosine similarity for semantic hits |
| `MINDCRAB_EMBED_BATCH` | `32` | Chunks per embeddings request |
| `MINDCRAB_NO_SEED` | `false` | Don't create starter notes in an empty vault |
| `RUST_LOG` | `mindcrab=info` | Log level |

Commands: `mindcrab serve` (default) · `mindcrab mcp` (stdio) · `mindcrab reindex` · `mindcrab token`.

**Backups:** the vault folder *is* the backup — notes are plain files. `.mindcrab/index.db` holds the search index and version history; copy it too if you want to keep history. Add `.mindcrab/` and `.trash/` to `.gitignore` if the vault is a git repo.

## Security notes

- Every path from the UI or an AI is normalised; `..`, absolute paths and hidden folders (`.mindcrab`, `.git`, `.obsidian`, `.trash`) are rejected.
- Rendered Markdown is sanitised (ammonia); attachments are served with a sandboxing CSP and `nosniff`, HTML is never rendered.
- Tokens are compared in constant time; failed logins are slowed down. Use HTTPS when exposed to the internet.
- MCP clients only get vault access — there is no shell or network tool.

## Development

```sh
cargo test
cargo run -- --vault ./dev-vault
```

The web UI lives in `web/` (plain ES modules, no build step) and is embedded into the binary at compile time.

## Türkçe hızlı başlangıç

MindCrab, Rust ile yazılmış, kendi sunucunda çalışan Obsidian benzeri bir Markdown not defteri. İçinde MCP sunucusu var. Böylece Claude, Gemini ve Codex gibi asistanlar notlarını **ortak uzun süreli hafıza** olarak kullanabilir.

1. `docker compose up -d` ile başlat ve `http://sunucu:3436` adresine gir. Anahtar `MINDCRAB_TOKEN` içindedir; boş bırakırsan logda görünür.
2. **Ayarlar → Yapay zekâ bağla** bölümünden uç noktayı ve hazır yapılandırmaları kopyala.
3. Anlama göre arama için `MINDCRAB_EMBED_URL` ve `MINDCRAB_EMBED_MODEL` ayarla (Gemini ya da Ollama, yukarıdaki örneklere bak). Böylece "sunucu" araması "Oracle makinesi" geçen notu da bulur.
4. Uygulamayı telefona kur. Açtığın notlar çevrimdışı da okunur; çevrimdışı yaptığın değişiklikler bağlantı gelince kaydedilir.
5. Asistanlar önce `MEMORY.md` dosyasını okur. Küçük bilgileri `Memory/<Konu>.md` içine tarihli madde olarak kaydeder. Yaptıkları her değişiklik notun **Geçmiş** sekmesinde görünür ve geri alınabilir.

Notlar düz `.md` dosyalarıdır. Var olan bir Obsidian kasasını da bağlayabilirsin, dışarıdan yapılan değişiklikler canlı olarak algılanır.

## License

MIT. Bundled: [Material Symbols](https://github.com/google/material-design-icons) (Apache-2.0), [Roboto Flex](https://github.com/googlefonts/roboto-flex) (OFL-1.1, see `web/fonts/OFL.txt`).
