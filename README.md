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
```

The tool will:
1. Search GitHub repositories by stars for your query.
2. Keep only repos that expose a latest release.
3. Let you pick a repo, then pick an asset from the latest release.
4. Stream the download with a progress bar to the chosen directory (default `.`).

## Notes
- Uses GitHub's public API; unauthenticated requests are rate-limited. Set `GITHUB_TOKEN` to avoid hitting limits.
- Uses rustls TLS; no OpenSSL required.
- OpenAI is only invoked with `--ai`; otherwise no API calls are made.
