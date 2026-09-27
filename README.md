# claude-consult

Outside reviews from non-Claude models, inside Claude Code.

Ask a panel of models from other labs, reached through [OpenRouter](https://openrouter.ai),
to review a plan, a design or a proposed fix. Each reviewer starts from a clean context,
reads your project through read-only tools and never sees what the others said. So when
they agree, it means something.

```
/consult should we move the job queue from polling to LISTEN/NOTIFY?
/deepseek is this retry loop actually idempotent?
/cleanroom how would you design rate limiting for a multi-tenant API?
```

One Rust binary, for Windows, Linux and macOS.

## Install

You need [Claude Code](https://claude.com/claude-code) with `claude` on your `PATH`, and an
[OpenRouter API key](https://openrouter.ai/settings/keys) with credits. Give the key a
**credit limit**: it is the only hard cap on what a runaway review can spend.

```sh
cargo install --locked --git https://github.com/trovix-oss/claude-consult claude-consult
claude-consult install
```

No Rust toolchain? Download the binary for your platform from
[Releases](https://github.com/trovix-oss/claude-consult/releases) and run
`claude-consult install` from there.

`install` asks for your key and checks it with OpenRouter, lets you pick your panel, shows
what it is about to do and waits for you to confirm. It adds the slash commands, registers
the MCP server with Claude Code, and adds a few hooks and a status line to your Claude Code
settings. Restart any open Claude Code sessions afterwards.

To upgrade, get the new binary and run `claude-consult install` again. To remove everything,
run `claude-consult uninstall`. `claude-consult install --help` lists the options.

## Using it

| command | what it does |
|---|---|
| `/consult <topic>` | the full panel, reading your repo |
| `/deepseek`, `/glm`, `/luna`, … | one reviewer from your panel, for a quick second opinion |
| `/cleanroom <problem>` | the panel with no repo context at all |

Claude can also call the consult tools on its own when a second opinion would help.

**Review or diagnose.** `/consult` reviews a plan by default: do its assumptions hold in the
code, and what does it miss? When something is broken and nobody knows why yet, it
*diagnoses* instead: reviewers rank the possible causes and name the cheapest check that
would confirm the top one.

**Clean room** sends only your problem statement: no code, no files, no tools. The answer
isn't anchored on what you already built, and it costs cents.

While a consult runs you see a live progress line. When it's done, a one-line summary shows
the cost and which reviews completed, and the status line keeps the session's total.

`claude-consult manage` is a terminal UI for changing the panel, replacing the key, picking
display styles and controlling the background service. `claude-consult run` consults once
from a plain terminal, without Claude.

## Models

The default panel is three models from three labs: DeepSeek V4 Pro, GLM 5.2 and GPT-6 Luna
Pro. A cheaper budget panel and a few alternates are in [`catalog.json`](catalog.json), and
any tool-capable model on OpenRouter works by its id. Mixing labs is the point: a second
model from a lab already on the panel adds less than you'd expect.

Anthropic models are refused, and so are routers and presets that could end up at Claude.
The idea is to ask anyone but Claude.

Each reviewer has a budget of $1 per review by default. Once past it, the reviewer stops
investigating and writes up what it has. Every result shows the cost OpenRouter reported.

## Privacy and security

- **Your code leaves your machine.** A review sends whatever the reviewers read (file tree,
  file contents, git history) to OpenRouter and the model's provider. Read-only means they
  can't change your repo, not that your code stays local.
- **Reviewers can't write anything.** They get no write, edit or shell tool at all. Paths
  are confined to the project, git is limited to read-only commands, and everything they
  read is size-capped and stripped of terminal escape codes.
- **Your key is stored in plain text** in Claude Code's `settings.json`, like any other
  Claude Code environment setting. The credit limit bounds the damage if it leaks.
- **On Windows, one background service serves every session**, on `127.0.0.1` and without
  authentication, so any local process can use it to read directories and spend your
  credits. On a shared machine, install with `--transport stdio`, so each Claude Code
  session starts its own private server instead.

## Development

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Tests run offline, with no key and no network. To try the installer without touching your
real setup:

```sh
cargo run -p claude-consult -- install --install-dir /tmp/cc-install --claude-dir /tmp/cc-claude --skip-service --skip-mcp-registration
```

[AGENTS.md](AGENTS.md) has the crate layout and the ground rules for contributors, human or
agent. `.mcp.json` offers Claude Code sessions in this repo a Rust docs server,
[rustdoc-mcp](https://crates.io/crates/rustdoc-mcp); it runs only if you have installed it. To release, push a tag `vX.Y.Z` that matches the version in `Cargo.toml`, and CI
attaches the binaries to a GitHub release.

## License

[MIT](LICENSE).
