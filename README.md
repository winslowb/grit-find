# grit-find

Small Rust CLI to search GitHub for repositories that have release assets and download them, with optional help from OpenAI to craft better search queries.

## Prerequisites
- Rust toolchain (tested with Rust 1.81+)
- `OPENAI_API_KEY` exported in your shell if you want AI-assisted queries.
- Optional: `GITHUB_TOKEN` to raise rate limits.

## Usage
```bash
# plain keyword search (interactive selection)
cargo run -- ripgrep

# let OpenAI turn a description into a search query
cargo run -- --ai "terminal markdown previewer"

# choose a destination directory
cargo run -- --output /tmp/downloads ripgrep

# paging (shows 100 per batch from GitHub; 'n'/'p' to move batches)
cargo run -- --page 2 ripgrep
```

The tool will:
1. Search GitHub.com repositories by stars for your query (fetches up to 100 results per batch; `--page` chooses the starting GitHub page).
2. Keep only repos that expose a latest release.
3. Shows up to 100 repos at once; enter a number to pick, `n` for next 100, `p` for previous 100, `c` to cancel; then pick an asset from the latest release.
4. Stream the download with a progress bar to the chosen directory (default `.`).

Caching & rate limits
- Results are cached per query under your OS cache dir (e.g., `~/.cache/grit-find/cache.json`).
- If GitHub returns HTTP 429, the CLI waits briefly and retries (up to 3 attempts) and will use cached pages when available to avoid re-hitting the API.

## Notes
- Uses GitHub's public API; unauthenticated requests are rate-limited. Set `GITHUB_TOKEN` to avoid hitting limits.
- Uses rustls TLS; no OpenSSL required.
- OpenAI is only invoked with `--ai`; otherwise no API calls are made.
