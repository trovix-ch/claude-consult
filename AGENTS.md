# claude-consult

claude-consult lets Claude Code ask models from other labs, through OpenRouter, for an
outside opinion on a plan, a design or a problem nobody has explained yet. It started as a
personal build and is shared as open source, for anyone with an OpenRouter key and a
little budget. It is small and stays small: one Rust binary, `claude-consult`.

**Independence is the product.** An outside opinion is worth asking for only if it is not
Claude and does not echo Claude. Reviewers come from other labs, start from a clean
context and never see each other; on a panel each gets a different lens. The question
states what is on the table, not which way Claude leans. Lenses steer attention, never
conclusions, and clean room never lets on that an implementation exists. Anything that
lets the conversation, a preference or another reviewer's answer reach a reviewer breaks
the product, however helpful it looks. So the wording of prompts, lenses and templates is
the product too, and changing it is the user's call.

## The ways to hurt yourself

1. **Touching the live install.** The machine you are on may run the shared service every
   Claude Code session there depends on. `claude-consult install` without `--skip-service`
   re-points and restarts it; without a scratch `--claude-dir` it rewrites the user's real
   commands, `settings.json` and MCP registration. `uninstall`, `service` and `manage`
   act on the real install too. None of them without being asked. An edit under `crates/`
   reaches nobody until `cargo install` and `claude-consult install`.
2. **Spending money.** Every real consult is billed, a smoke test included, and so is
   `claude-consult run`. Ask first.
3. **Leaking the key.** Never on a command line, in output, logs, fixtures or commits, and
   never printed unmasked. An `sk-or-` string in a diff means stop.
4. **Breaking the build contract.** `cargo install --git` works only while exactly one
   package in the workspace has a binary: no `[[bin]]` and no `src/main.rs` outside
   `crates/claude-consult`. Tests stay offline, with no key and no network (mock servers
   only), write only inside a temporary directory, and never touch the real Claude dir,
   the real `OpenRouterMCP` task, `schtasks` or the `claude` CLI.

## Where things live

| crate | responsibility |
|---|---|
| `claude-consult` | the only binary: a thin clap front end, no logic of its own |
| `consult-core` | catalog, registry, key, OpenRouter client, sandbox, panel (prompts and lenses), settings merge, file generation |
| `consult-mcp` | the MCP server over stdio and HTTP, the progress heartbeat, `run` and `reviewers` |
| `consult-hooks` | the summary, display and status line hooks |
| `consult-service` | the Windows scheduled task; unsupported elsewhere |
| `consult-tui` | shared terminal UI pieces, the panel picker among them |
| `consult-install` | install, upgrade, uninstall, and the Python-install cleanup |
| `consult-manage` | the `manage` TUI; every write goes through the crates above |

`templates/` and `catalog.json` are built into the binary.

## Before you call it done

`cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings` and
`cargo test --workspace` are clean. CI runs all three on Windows, Linux and macOS with
warnings as errors.

## Commits

Every commit follows [Conventional Commits](https://www.conventionalcommits.org/):
`type(scope): summary`, where the scope is optional and the type is one of `feat`, `fix`,
`docs`, `refactor`, `perf`, `test`, `build`, `ci` or `chore`. Anything that breaks an
existing install or a caller gets a `!` after the type and a `BREAKING CHANGE:` footer.

---

If a rule here fights the code or the task, say so. The user decides which one moves.
