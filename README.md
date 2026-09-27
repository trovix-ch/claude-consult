# claude-consult

Outside reviews from non-Claude models, inside Claude Code.

Claude Code asks a panel of models from other labs, reached through
[OpenRouter](https://openrouter.ai), to review a plan, a design or a proposed fix.
Each reviewer gets a clean context and read-only access to your project. It runs its
own agentic loop to investigate before it answers. Reviewers run concurrently and never
see each other's output, so when they agree, that's real corroboration.

```
/consult should we move the job queue from polling to LISTEN/NOTIFY?
/deepseek is this retry loop actually idempotent?
/cleanroom how would you design rate limiting for a multi-tenant API?
```

claude-consult is a single Rust binary, `claude-consult`, for Windows, Linux and macOS.

## Install

You need:

- the `claude-consult` binary: build it with a current stable Rust toolchain from
  [rustup](https://rustup.rs), or download a prebuilt one (below)
- [Claude Code](https://claude.com/claude-code), with its `claude` CLI on `PATH`
- git, optionally. Reviewers use read-only git history when it is there and work without
  it when it isn't.
- An OpenRouter API key with credits: <https://openrouter.ai/settings/keys>. Give the
  key a **credit limit**. That limit is the only hard cap on what a runaway review can
  spend.

Get the binary one of three ways, then run `claude-consult install`.

**From source, with cargo:**

```sh
cargo install --locked --git https://github.com/trovix-oss/claude-consult claude-consult
claude-consult install
```

**Prebuilt, with [cargo-binstall](https://github.com/cargo-bins/cargo-binstall):**

```sh
cargo binstall --git https://github.com/trovix-oss/claude-consult claude-consult
claude-consult install
```

The crate carries binstall metadata pointing at the release archives, so this downloads
the prebuilt binary for your platform instead of compiling it.

**Prebuilt, by hand:** download the archive for your platform from
[GitHub Releases](https://github.com/trovix-oss/claude-consult/releases), unpack it and run
the binary inside:

| platform | archive |
|---|---|
| Windows x64 | `claude-consult-vX.Y.Z-x86_64-pc-windows-msvc.zip` |
| Linux x64 | `claude-consult-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz` |
| macOS Apple silicon | `claude-consult-vX.Y.Z-aarch64-apple-darwin.tar.gz` |
| macOS Intel | `claude-consult-vX.Y.Z-x86_64-apple-darwin.tar.gz` |

```sh
./claude-consult-vX.Y.Z-<target>/claude-consult install
```

Wherever the binary came from, `install` copies it into the install dir, so the
downloaded or cargo-installed one is only the installer.

### What `install` asks

1. **Your OpenRouter key.** This is required. If Claude Code's `settings.json` or the
   `OPENROUTER_API_KEY` environment variable already holds one, it offers that, masked.
   Otherwise the input is hidden. The key is checked against OpenRouter's free `/key`
   endpoint before anything is written, and the check reports what it has spent and its
   credit limit.
2. **Your panel.** A picker lists the curated favourites from
   [`catalog.json`](catalog.json) first, with the recommended panel already ticked, then
   every other tool-capable model on OpenRouter, most popular first, each with its live
   price and context size. Press Enter to take the recommended panel as it is.
3. **Your status line**, only if you already have one of your own (see below).
4. **Display styles** for the progress line and the summary (see
   [What you see while it runs](#what-you-see-while-it-runs)). The installed styles are
   kept unless you change them.

Then it shows the plan (install dir, Claude dir, panel with prices, commands, masked key,
status line, display, service) and does nothing until you confirm.

| key in the picker | does |
|---|---|
| Up/Down, PgUp/PgDn, Home/End | move |
| Space | tick or untick the model under the cursor |
| typing | filter by alias, id, name or lab; Backspace edits the filter, Esc clears it |
| Ctrl+R, Ctrl+B | switch to the recommended or the budget panel |
| Enter | confirm; needs at least one model ticked |
| Ctrl+C | quit the install |

The footer warns when two picks come from one lab. The other models and all the prices come
from OpenRouter's public model listing, fetched once per run, without your key. If it can't be
reached, the picker offers the favourites only, without prices, and says so; the install
still goes ahead. AltGr combinations type their character rather than trigger a shortcut.

The installer is a full-screen terminal UI when stdin and stdout are both a terminal. When
either is redirected, it asks the same questions as plain lines, and the picker becomes a
numbered list of the favourites: Enter for the recommended panel, `b` for the budget one,
or numbers, aliases and OpenRouter ids separated by commas. With `--unattended` it asks
nothing at all (see the flags).

A model from outside the favourites gets a quick command named after its id:
`mistralai/mistral-medium-3.1` becomes `/mistral-medium-3-1`, with `-2`, `-3`, … added if
that name is taken.

### What `install` does

| step | where |
|---|---|
| stops the service if this install runs one, then copies the running binary | `<install dir>/bin/claude-consult` (`.exe` on Windows) |
| writes the reviewer registry, the display styles and a manifest of everything it wrote | `models.json`, `display.json`, `manifest.json` in the install dir |
| generates `/consult`, `/cleanroom` and one quick command per panel model | `<claude dir>/commands/` |
| generates the `openrouter-workflow` skill | `<claude dir>/skills/openrouter-workflow/` |
| renders the `verify-claims` workflow, with your panel as its default voices | `<claude dir>/workflows/verify-claims.js` |
| stores the key, and raises the MCP timeouts a long panel needs | `<claude dir>/settings.json`, `env` block |
| adds a hook that tallies each consult, one that draws its summary, and a status line unless you have one | `<claude dir>/settings.json`, `hooks` and `statusLine` |
| removes an old Python install's files from the same install dir | the install dir |
| Windows, service transport: registers and starts a scheduled task serving MCP on `127.0.0.1:8765` | Task Scheduler: `OpenRouterMCP` |
| registers the server with Claude Code at user scope, as `openrouter` | `claude mcp remove`, then `claude mcp add` |

The install dir is `%LOCALAPPDATA%\claude-consult` on Windows and
`$XDG_DATA_HOME/claude-consult` (by default `~/.local/share/claude-consult`) elsewhere.
The Claude dir is `--claude-dir`, else `$CLAUDE_CONFIG_DIR`, else `~/.claude`. Per-session
consult totals go in `state/` in the install dir, and any file of yours an install
displaced goes in `backup/<timestamp>/` there. The service logs to `state/service.log`
(see [One shared service](#one-shared-service)).

**Transport.** On Windows the default is one shared service for every session: a
scheduled task running `"<install dir>/bin/claude-consult.exe" serve --http --port 8765
--detached`, registered with `claude mcp add --transport http --scope user openrouter
http://127.0.0.1:8765/mcp`. Everywhere else, and on Windows with `--transport stdio`, Claude
Code starts the server itself for each session: `claude mcp add --transport stdio --scope
user openrouter -- "<install dir>/bin/claude-consult" serve`. Switching a Windows install
to stdio removes its scheduled task. The service is Windows only; asking for it elsewhere
stops the install before anything is written.

If `claude` isn't on `PATH`, or `claude mcp add` fails, the installer prints the exact
`claude mcp add` command to run by hand.

Restart open Claude Code sessions afterwards.

Besides those `env` entries, the installer adds three things to `settings.json`: a
`PostToolUse` hook on the two consult tools, a `MessageDisplay` hook (only while the
summary is on), and the `statusLine` if it set one. Each runs
`"<install dir>/bin/claude-consult" hook summary|display|statusline`. A re-run replaces
them in place and never duplicates them. Your own hooks are left as they are, in the same
order. Uninstall removes exactly these and asks before deleting the key; the two raised
timeouts stay. Only a command exactly as the installer writes it counts as one of these: a
command of yours that runs the binary, such as a status line that pipes to it, stays
yours.

**If you already have a status line**, the installer asks before replacing it, and the
default is to keep yours. `--unattended` always keeps it. When yours stays, the installer
prints the command your own status line script can pipe its stdin to, to add the consult
total. If it did replace yours, uninstall puts yours back.

If a file the installer generates would replace one you wrote yourself (say an existing
`glm.md` command), it backs yours up first and restores it on uninstall.

### Flags

```sh
claude-consult install --unattended --panel a,b,c        # no prompts; key from OPENROUTER_API_KEY or settings.json
claude-consult install --install-dir D:\tools\consult    # install somewhere else
claude-consult install --port 8766                       # if 8765 is taken
claude-consult install --transport stdio                 # no shared service, even on Windows
claude-consult install --summary-style off               # display styles, see "What you see while it runs"
claude-consult uninstall                                 # remove everything it installed
```

| `install` flag | meaning |
|---|---|
| `--unattended` | no prompts: the key from `OPENROUTER_API_KEY` or `settings.json`, the recommended panel unless `--panel`, a status line of yours kept, every question its default |
| `--panel A,B,C` | skip the picker: favourites' aliases and OpenRouter ids, mixed, comma-separated |
| `--port N` | the shared service's port; default `8765` |
| `--transport service\|stdio` | how Claude Code reaches the server; default `service` on Windows, `stdio` elsewhere |
| `--progress-style STYLE` | the progress line's style; default: keep the installed one |
| `--summary-style STYLE` | the summary's style; default: keep the installed one |
| `--skip-key-check` | don't check the key against OpenRouter (offline installs) |
| `--skip-service` | leave the scheduled task alone |
| `--skip-mcp-registration` | don't run `claude mcp` |
| `--install-dir DIR` | the install dir |
| `--claude-dir DIR` | Claude Code's config dir |

`--panel deepseek-v4-pro,glm-5.2,mistralai/mistral-medium-3.1` takes favourites and
outside models together. A model from outside the favourites must be in the live listing
with tool calling, so while the listing is unreachable `--panel` takes favourites only. A
favourite that OpenRouter no longer offers with tool calling is refused while the listing
is up, and let through, unchecked, while it isn't. The installer never offers an
`anthropic/` id, and refuses one if you name it, as it does a router or a preset (see
[Using it](#using-it)): consult exists to ask anyone but Claude. `--unattended` without
`--panel` installs the recommended panel, on a re-run too.

`--skip-service`, `--skip-mcp-registration` and `--claude-dir` exist for test installs.
With a `--claude-dir` that isn't Claude Code's real config dir, the installer never runs
`claude mcp`, because that command always edits the real config. A service install into
such a Claude dir warns first, since the service would not find the key there.

**Start at boot** (Windows) needs the task registered with the S4U logon type, which
only an **elevated** process may do. Run from an admin shell, the installer registers it
directly. Run unelevated, it first registers a task that starts at logon only, prints
`Start-at-boot needs administrator rights; accept the Windows prompt that appears.` and
brings up the Windows administrator prompt; a hidden elevated run of the installer binary
then re-registers the task for boot, as you. A dismissed prompt, `--no-elevate` or
`--unattended` keeps the logon-only task, and the installer says so and prints the
command that fixes it later:
`& '<install dir>\bin\claude-consult.exe' service install --elevate`.

### Upgrading

**Re-running `install` is the upgrade path.** Get the new binary the same way you got the
first one, then run `claude-consult install` again. It stops the service, swaps in the new
copy, regenerates the commands, skill, workflow and settings entries, and restarts the
service, so you never keep talking to a stale build. Re-run it the same way to change the
panel or rotate the key, or use [`claude-consult manage`](#claude-consult-manage).

On Windows the service and the hooks run the **copy** in `<install dir>\bin`, never the
binary cargo installed. Windows locks a running executable, so `cargo install` can replace
its own binary while the service is running without a fight; the new code reaches your
sessions only when `claude-consult install` swaps the copy and restarts. A copy that a
running stdio session still holds is renamed aside as `claude-consult.exe.old-<n>` and
deleted by a later install once nothing runs it.

With the stdio transport, each open session keeps the server it started with. Restart
the sessions to pick up the new binary.

### Upgrading from the Python version

Run `claude-consult install` into the same install dir. The Python installer's default,
`%LOCALAPPDATA%\claude-consult`, is also this one's, so nothing needs saying:

- The scheduled task keeps its name, `OpenRouterMCP`, so it is replaced and now runs the
  binary instead of Python.
- The Python install's hook and status line commands in `settings.json`
  (`"<dir>/.venv/Scripts/python.exe" "<dir>/hooks/....py"`) are recognised as the
  installer's own and replaced in place, never left beside the new ones. This also holds
  for a Python install in another dir, and for one whose script is already gone.
- Its files in the install dir are deleted: `server.py`, `panel.py`, `sandbox.py`,
  `service.ps1`, `requirements.txt`, the three scripts in `hooks\` and their
  `__pycache__`, `.venv` and `__pycache__`. `hooks\` itself goes only if nothing else is
  left in it. `models.json`, `display.json` and `manifest.json` stay and are rewritten,
  so your display styles carry over.
- The MCP registration is removed and added again.

If the task served a Python install from another dir, the installer says so and asks
before re-pointing it. That old folder is left untouched. Uninstalling the Python version
first (`install.ps1 -Uninstall` from its checkout) is the cleanest route there.

### Uninstall

```sh
claude-consult uninstall                 # asks before removing, and before deleting the key
claude-consult uninstall --yes           # no questions; the key stays
claude-consult uninstall --yes --remove-key
```

It removes this install's scheduled task (never another install's), the MCP registration
(unless another install's service still uses it, or the Claude dir isn't the real one),
the generated commands, skill and workflow, its `settings.json` entries, putting back
anything they displaced, and the install dir. On Windows the running binary can't delete
itself, so when you uninstall with the installed copy, the rest goes at once and the copy
goes as soon as it exits. The binary you installed with cargo or downloaded stays; remove
it with `cargo uninstall claude-consult` or by deleting it.

## Using it

| command | what it does |
|---|---|
| `/consult <topic>` | the full panel, grounded in the repo |
| `/deepseek`, `/glm`, `/luna`, … | one reviewer from your panel, for a quick "what do you think" |
| `/cleanroom <problem>` | the panel with **no repo context at all** |

The quick commands follow your panel: one per model on it, named in the favourites
(`/deepseek`, `/glm`, …) or, for a model from outside them, after its id. Claude can also
call the `consult`, `consult_clean` and `list_reviewers` MCP tools directly whenever a
second opinion would help.

Their `models` argument names a reviewer by its alias (`deepseek-v4-pro`), its OpenRouter
id (`deepseek/deepseek-v4-pro`) or its quick-command name (`deepseek`), tried in that
order. Every favourite answers to all three whether or not it's on your panel.

Anything else is taken as an OpenRouter model id, and it has to name one concrete model.
An `anthropic/` id, a router such as `openrouter/auto` and a preset (`@preset/...`, alone or
after an id) are refused before any request, since each could end up at Claude. While
OpenRouter's model listing answers, an id it doesn't list as tool-capable is refused too,
and so is a panel model from outside the favourites that has dropped out of it. That is
also how a router with an ordinary-looking id gets caught: the listing prices it per
request. When the listing can't be reached, only the name is checked.

### Two grounded modes

`consult` takes a `mode`, and picking the wrong one asks the wrong question.

**`review`** (the default) assesses a plan, design, proposal or idea. Do its assumptions
hold in the code? What does it get wrong, and what does it leave out? It ends with a
"Bottom line".

**`diagnose`** is for when something is wrong and the cause isn't known yet. Reviewers work
from evidence in the code and keep verified, inferred and guessed separate. They rank the
competing explanations and end with a "Most likely cause" that names the single cheapest
check to confirm or kill it.

If you send an unsolved problem as `review`, the reviewers assess a plan that doesn't exist
yet.

### Lenses

Reviewers who share one brief make the same opening moves and read the same files. Then
"all three agreed" tells you about the labs, not about the evidence. So each reviewer
gets a different mandate, assigned round-robin:

- **verification**: chase the plan's claims into the source
- **failure modes**: unhappy paths, concurrency, security, rollback
- **prior art and fit**: what the repo already solves, what the plan disturbs, whether
  to build it at all

A reviewer that answers without opening a single file gets pushed back once. If it still
won't investigate, its review is labelled as an opinion on the brief, not on the code.

### Clean room

`consult_clean` sends the problem statement and nothing else: no file tree, no code, no
git, no tools. What comes back is a **clean architectural perspective**, meaning what a
good engineer would consider the cleanest architecture on its own merits.

The prompt never tells the responder that an implementation exists or that its answer will
be weighed against anything. A model that knows those things has a reason to argue for
change instead of saying what it honestly thinks is clean. The question you write must keep
that discipline too, and `/cleanroom` carries the checklist.

Each responder is pointed at a different aspect: simplicity, scale-and-failure,
question-the-frame, or data-and-state. These steer *attention*, not *conclusions*.
Clean room is one request per model with no tool loop, so it costs cents even on premium
models.

### Checking claims: the `verify-claims` workflow

The installer also puts a saved Claude Code workflow, `verify-claims`, in
`<claude dir>/workflows/`. It checks concrete claims about a project ("retry() re-sends a
POST after a timeout, so an order can be placed twice") from two sides. For each claim,
concurrently:

- **A local check.** An agent on your Claude subscription tries to reproduce the claim by
  running code. A verdict it reached without running anything counts as inconclusive. It is
  told to work in a temporary directory outside the project and never to touch the network;
  that is an instruction to the agent, not a sandbox.
- **One outside consult per voice.** An outside reviewer is asked to refute the claim
  first and to end with a `VERDICT:` line. Each voice gets a different brief (the code
  path, the preconditions, the surroundings, the claim's own evidence), so naming one model
  twice asks two different questions. A workflow script can't call a tool, so each consult
  goes through a small Haiku agent that only relays the call and returns its output.

The script, not a model, does the tally. An outside verdict counts only from a review whose
status record says complete and that ends in a verdict line; everything else is listed as
no signal, never as agreement. Without `claims`, a finder agent first proposes up to five
within `scope`, and the run stops there unless it reports having examined the whole scope.

Claude Code runs a workflow only on an explicit request, and each run needs its own. So ask
for it by name:

```
Run the verify-claims workflow on C:\path\to\project for this claim: ...
```

| arg | meaning |
|---|---|
| `root` | absolute path of the project (required) |
| `claims` | `[{claim, id?, where?, mechanism?, repro?}]`; leave it out to have them found |
| `scope` | what the finder should examine; required without `claims` |
| `voices` | reviewers, named as `models` accepts them, one consult per claim each; default: the panel you installed |
| `model` | the Anthropic model for the finder and the local check; default: the session model |
| `localCheck` | `false` skips the local check; default `true` |
| `dryRun` | `true`: needs `claims`; no finder and no local check run, the relays return canned output, and nothing is sent to OpenRouter |

An unknown key stops the run before any agent starts.

| status | meaning |
|---|---|
| `checked` | every check gave a signal |
| `partial` | some checks gave a signal, some didn't |
| `no_signal` | no check did, or the finder died or didn't finish its scope |
| `no_claims` | the finder examined the whole scope and proposed nothing: one agent's reading, not a clean bill |
| `bad_args` | the args were rejected; the errors and the usage come back |

Each claim comes back with its tally and a word for it (`agree`, `partial`, `disagree`,
`inconclusive` or `no signal`), each voice with its verdict or the reason it gave none, and
the run with its coverage and the outside cost OpenRouter reported, plus a count of calls
whose cost couldn't be read.

**Cost.** Every voice is one consult per claim: five claims with three voices is fifteen
consults. A rough estimate, not a measurement: 0.04 to 0.25 USD per consult, so about
0.25 USD for one claim with three voices and 1 to 2 USD for five. The finder, the local
checks and the relays use your Claude subscription on top of that.

**Dry run.** `dryRun: true` needs `claims`, and runs no finder and no local check. Only
the Haiku relays run, each returning canned output instead of calling `consult`, so
nothing is sent to OpenRouter and the run costs nothing there. The canned output
alternates a complete review that ends `INCONCLUSIVE` with a cut-off one that ends
`CONFIRMED`, which the tally must drop. So with two or more voices, expect `partial` with
every outside verdict `inconclusive`. An outside `confirmed` or `refuted` in a dry run
means the tally counted something it should have dropped.

### What you see while it runs

While a consult runs, Claude Code shows one line for it, updated about once a second:

```
consult · 184s · 0.31 USD · 1.92M in / 22k out · 1/3 finished
```

When the result is back, a one-line summary appears under Claude's next reply: how many
reviews are complete, cost, tokens, the slowest reviewer's time, and a mark per reviewer.
It's dimmed, with a green ✓ for a complete review and a red ✗ for one that is incomplete or
failed:

```
consult · 2/3 complete · 0.5120 USD · 3.12M in / 41k out · 412s · ✓ deepseek ✓ glm ✗ luna
```

The summary is for your eyes only. It's drawn on screen and never enters Claude's context.
Clean-room calls are labelled `cleanroom`.

From the first consult in a session on, the status line shows that session's running total:

```
consult this session · 2 calls · 0.5520 USD · 3.12M in / 53k out
```

Both lines come in styles. Set them in the installer's display screen, in
`claude-consult manage`, or with flags; a later re-run without the flags keeps what you
chose.

```sh
claude-consult install --progress-style marks --summary-style quote
```

| progress style | the line while it runs |
|---|---|
| `full` (default) | elapsed time, cost, tokens in and out, reviewers finished |
| `ticker` | elapsed time, cost so far, reviewers finished |
| `count` | reviewers finished, tool calls so far |
| `marks` | each reviewer's step while it works, then its mark: `consult · deepseek ▸9 · glm ✓ · luna ▸6` |
| `latest` | the newest step of the most recently active reviewer: `glm: read_file(path=src/lib.rs)` |
| `percent` | `consult · reviewers finished`, with Claude Code's own percentage after it |
| `quiet` | only who was asked: `consult · asking deepseek, glm, luna` |

| summary style | the summary |
|---|---|
| `dim` (default) | dimmed, coloured marks |
| `italic` | markdown italics, no colour |
| `quote` | a markdown quote, no colour |
| `off` | none, and its display hook isn't registered at all |

The styles live in `display.json` in the install dir. The server and the hooks read it on
every call, so a hand edit there applies to the next consult without a restart. Switching
the summary off or back on is the exception: its display hook is in `settings.json` only
while the summary is on, so use `--summary-style` (or `manage`) for that.

### `claude-consult manage`

A full-screen terminal UI over one install. Tab and Shift+Tab (or Left/Right, or `1` to
`8`) switch screens, `r` refreshes, `?` shows help, `q` or Esc quits. Every change it
makes re-runs the install flow with the answers settled on screen, so it writes exactly
what `install` would.

| screen | what it does |
|---|---|
| Status | the install at a glance: version, install and Claude dirs, where the key comes from, the panel and when its prices were taken, display styles, transport, service state and processes |
| Panel | Enter opens the picker; re-pick the panel and regenerate |
| Key | Enter to paste a new key (shown as stars), checked with OpenRouter, then saved |
| Display | Up/Down and Space pick the progress and summary styles, Enter applies them |
| Sessions | the per-session consult totals; `d` or Delete removes one |
| Catalog | Enter checks the favourites against OpenRouter's live listing |
| Service | start, stop, restart, register or unregister the scheduled task |
| Uninstall | Enter, then confirm, to remove everything |

It needs a terminal: with stdin or stdout redirected it refuses to start. It manages the
install it finds as described under [The install dir](#the-install-dir); pass
`--install-dir` for another one.

### From a terminal, without Claude

`claude-consult run` consults once and prints the result, using the installed registry and
key:

```sh
claude-consult run --root /path/to/project --question-file plan.md
claude-consult run --root . --question "is this retry loop idempotent?" --models deepseek
cat plan.md | claude-consult run --root .
# --models kimi,gemini-3.1-pro      pick reviewers: aliases, command names or OpenRouter ids
# --attach src/main.rs,Cargo.toml   put files in front of them up front
# --mode diagnose                   diagnose instead of review
# --clean                           clean room: no project context
# --max-steps 24                    tool-calling steps per reviewer
# --max-cost 1                      per-reviewer spend ceiling in USD
# --json                            raw output instead of markdown
```

The question comes from `--question-file`, `--question`, or stdin when stdin isn't a
terminal; with none of them it's a usage error (exit 2). `--root` defaults to the current
directory. `claude-consult reviewers` prints the registered reviewers as `list_reviewers`
shows them.

The key comes from `OPENROUTER_API_KEY` if set, else from Claude Code's `settings.json`.

### Reading a result from a script

The tools return plain markdown. Some earlier builds came back wrapped by the MCP SDK as a
JSON string, `{"result": "..."}`; anything that unwrapped that should now take the text as
it is.

Every result ends with one status record per reviewer, one per line, in panel order:

```
<!-- consult-result v1 {"alias":"glm-5.2","short":"glm","status":"ok","complete":true,"finish":"stop","capped":false,"tool_calls":14,"cost_usd":0.1873,"tokens_in":912345,"tokens_out":8123,"seconds":201.4} -->
```

| key | meaning |
|---|---|
| `alias`, `short` | the reviewer, and the short name the display uses (its quick command, else the alias) |
| `status` | `ok`, `incomplete`, `empty`, `error`, or `failed` when the whole call failed |
| `complete` | true only for `ok`: text came back and the provider's last `finish_reason` was `stop` |
| `finish` | that last `finish_reason`; `"missing"` if the provider gave none, null if nothing answered |
| `capped` | a step, time or cost budget cut the investigation short. A capped review can still be complete |
| `tool_calls`, `cost_usd`, `tokens_in`, `tokens_out`, `seconds` | as metered |

A call that fails before any reviewer runs ends with its error and a single record:
`status` `failed`, `alias` and `short` null, zeros elsewhere.

Take records only from the run of record lines at the very end of the text. A reviewer can
quote the format in its review, and such a line is not a record. No block at the end, or a
malformed line in it, means no signal. The markdown output of `claude-consult run` ends the
same way (a failed run prints `consult failed: ...` to stderr and exits 1 instead);
`--json` carries `status`, `complete`, `finish` and `capped` on each review.

## Models

[`catalog.json`](catalog.json) holds the curated favourites: what OpenRouter's listing
can't tell you. Per model that is its OpenRouter id, a short alias, its quick-command name,
a tier and our notes on what it plays to, plus the recommended and budget panels. It holds
no prices and no context sizes, because those drift within hours. The catalog is built
into the binary; the installer reads the prices, and the list of every other model, from
OpenRouter's public model listing at install time. That listing needs no key and costs
nothing.

The prices written into your generated commands and skill say when they were fetched, and
read "price unknown" after an offline install. `list_reviewers` fetches the listing again
when it's called (10-second timeout, cached for two minutes), so it shows current prices
and context for every registered model. If OpenRouter doesn't answer, it shows the values
recorded at install and says when they were taken, or "price unknown" if there are none. A
registered model that has dropped out of the listing is flagged.

| panel | models | why |
|---|---|---|
| recommended | `deepseek-v4-pro`, `glm-5.2`, `gpt-6-luna-pro` | three labs; correctness, breadth, agentic investigation |
| budget | `glm-5.3-flash`, `deepseek-v4.1-flash`, `minimax-m3` | three labs at a fraction of the price, not yet measured in this harness |

Alternates (`grok-4.7`, `qwen3.8-max`) and premium models (`gemini-3.1-pro`, `kimi-k3`,
`gpt-6-sol`) round it out. Premium models are poor value on a grounded review, because every
investigation step resends the whole conversation. They are cheap in clean room.

**Diversity of lineage is the point of a panel.** A second model from a lab that's already
on the panel buys much less than its benchmark delta suggests, and the picker warns if you
choose one.

Any other model in OpenRouter's tool-capable listing also works at call time, by its id,
whether or not it's a favourite. Routers and presets don't (see [Using it](#using-it)).

### Keeping the favourites honest

```sh
claude-consult check-catalog            # --quiet prints problems only
```

This checks that every favourite is still listed on OpenRouter and still supports tool
calling, which grounded review can't do without. It reads the whole public listing, so it
tells a withdrawn model from one that lost tool calling, and it needs no key and costs
nothing. Prices and context sizes aren't checked: the catalog holds none. It exits 0 when
every favourite checks out, 1 on any problem, and 2 when the listing couldn't be read, so
nothing was checked. The installer makes the same check as it goes: a favourite missing
from the listing is left out of the picker, with a note.

It checks the catalog built into the binary running it. To update the favourites: edit
`catalog.json`, rebuild, run `cargo run -p claude-consult -- check-catalog`, and install
the new binary.

Benchmark figures in the catalog notes are **as reported** by the sources listed in
`_sources`, not re-measured here. Treat them as a starting prior. The cost printed with
every review comes from OpenRouter's own accounting and is the number to trust.

## Security and privacy

Read this before you install it for someone else.

- **Your code leaves the machine.** A grounded review sends whatever the reviewer reads
  (the file tree, file contents, git history) to OpenRouter and the model's provider.
  Read-only means reviewers can't *modify* your repo. It doesn't mean the data stays local.
- **The key sits in plain text** in `<claude dir>/settings.json`, like any other Claude Code
  `env` setting. It's exported to Claude Code sessions and the processes they start. A
  credit limit on the key bounds the damage if it leaks.
- **The key never goes into this repo.** The installer writes it only to `settings.json`,
  never puts it on a command line (command lines are visible to every process on the
  machine), and never shows more than a masked form. `.gitignore` also covers stray
  `settings.json`, `.env` and `*.key` files.
- **The service's MCP port is unauthenticated.** The service binds `127.0.0.1` only (never
  `0.0.0.0`), but any local process can reach it. The `root` a caller passes decides what
  gets read, so anything on the machine can use it to read any directory and spend your
  credits. On a shared machine, install with `--transport stdio`: then there is no port,
  and only the Claude Code session that started the server can talk to it.

Reviewers are read-only **by construction**. The sandbox they work through has no write,
edit or shell tool at all, and the threat model is an untrusted repository:

- Every path is resolved (following symlinks) and must land inside the review root.
- `git` is limited to read-only subcommands *and* safe flags. That matters because
  `git diff --output=FILE` writes, and `--ext-diff`/`--textconv` run commands the repo
  names. Those run with `diff.external`, pagers and `protocol.ext` neutralised, under a
  45-second timeout.
- Reads, greps, globs and git output are size-capped.
- Every tool result is stripped of terminal escapes and control bytes, so a crafted commit
  message can't rewrite what the reviewer sees.
- A filesystem root or the home directory is refused as a review root.

## How it works

### One shared service

On Windows, a single process serves every Claude Code session on the machine, over
stateless streamable HTTP on `127.0.0.1:8765`. The alternative is a stdio child per
session. For the Python build that was measured on 2026-08-01 at 17 MB idle and up to
58 MB after a run, or ~230 MB across eight sessions. The Rust binary is lighter: measured
2026-09-26 on Windows 11 (release build, `serve --http` on a spare port, read with
PowerShell's `Get-Process`), 11.5 MB working set idle and 12.9 MB after an MCP
`initialize` and `tools/list`, with 3.2 to 3.4 MB private. Its footprint during and after
a real consult has not been measured, nor has a stdio child's.

The costs of sharing, and how each one is handled:

- **Staleness.** The service holds the binary in memory, so a new build takes effect only
  after a restart. `claude-consult install` restarts it. The registry and the display
  styles are read on every call, so those changes need no restart.
- **Single point of failure.** One crash takes the tool away from every session, so the
  task restarts up to 3 times at 1-minute intervals. It has no execution time limit and
  runs on battery.
- **Identity.** The task runs *as you* (S4U, no stored password), not as SYSTEM. Under
  SYSTEM, `~` is `C:\Windows\System32\config\systemprofile`, the key would never be
  found, and every consult would fail while the port looked healthy. `service status`
  prints where the key was resolved from, for exactly this reason.

```sh
claude-consult service status     # task, state, identity, triggers, port, key source, processes and RSS
claude-consult service restart    # stop, then start: sessions reach the current binary
claude-consult service stop | start | install | uninstall
```

`service` takes the registered task's port unless you pass `--port`. On Linux and macOS
there is no service; `service status` says so and reports whether the port is listening.

`service install` tries start-at-boot (S4U) first. Refused for want of administrator
rights, it registers the logon-only task and, in a terminal, goes straight to the
administrator prompt as the installer does. `--elevate` does so without a terminal too,
`--no-elevate` never does, and `--no-fallback` (what the elevated run is given) fails
instead of registering the logon-only task. The manage TUI's *Re-register task* goes to
the prompt after its one confirmation.

**No console, a log file instead.** The task passes `--detached`: the server gives up the
console Task Scheduler opened for it, so no window stays on screen (one may flash for an
instant as it starts), and it logs to `state/service.log` in the install dir rather than
to stderr. Past 2 MB the log moves to `service.log.1`, replacing the one before, so a
crash's lines survive the restart that follows. It records warnings and errors, plus one
line when the service starts; `RUST_LOG` overrides that. Without `--detached`,
`serve --http` logs to stderr as before, for runs by hand.

### The install dir

Every subcommand finds the install it works on in this order: `--install-dir`, then the
`CLAUDE_CONSULT_DIR` environment variable, then the directory whose `bin/` holds the
running binary (only when that directory also holds a `models.json` or `manifest.json`, so
cargo's own `bin/` never counts), then the platform default. The hooks and the service run
the installed copy, so they always find their own install; for a non-default install, pass
`--install-dir` or set `CLAUDE_CONSULT_DIR` when you run `manage`, `run` or `service` from
another copy.

### Idle timeout, and why the server emits progress

MCP clients abort a tool call that sends neither a response nor a progress notification
within an idle window. On HTTP transport that window is **5 minutes**, and it's a
**separate timer from `MCP_TOOL_TIMEOUT`**, which doesn't extend it. A panel routinely
runs longer. So the server sends its progress line on a clock, once a second for the
whole call, whether anything changed or not: the line you see is also the heartbeat. The
installer also raises `CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT` to at least 1800000 (30 minutes)
as a backstop, and `MCP_TOOL_TIMEOUT` to at least 2400000 (40 minutes), since a
reviewer's wall-clock budget is 25 minutes. It raises them and never lowers them.

The clock has a price. The idle window no longer notices a provider request that hangs,
because the heartbeat keeps going around it. Each request has a 300-second *read*
timeout, which fires only when no bytes at all arrive for that long; the request is then
tried up to four times, so one that goes completely silent is given up after about 20
minutes. It is not a deadline on the whole response. A response that keeps trickling in
never trips it, and the reviewer's 25-minute budget is checked only between steps, so
nothing on the server stops that one: only `MCP_TOOL_TIMEOUT`, when Claude Code gives up
on the whole call.

### The display hooks, and what they cost

The progress line comes from the server. The summary and the status line come from three
hooks, each a run of the installed binary (`claude-consult hook summary|display|statusline`):

- After each `consult` or `consult_clean` call, a `PostToolUse` hook reads the result's
  status records, adds the call to the session's total and leaves the summary for the next
  reply. It prints nothing, so none of this reaches Claude.
- A `MessageDisplay` hook draws that summary under the last chunk of Claude's next reply.
  It changes only what your terminal shows. The stored message, and what Claude reads on
  the next turn, stay as they were.
- The status line prints the session's total.

Every hook exits 0 on any failure: a broken hook must never cost you a result you already
paid for. Totals are small files in `state/` in the install dir, one per session with a
line per consult that the status line adds up, pruned after 7 days.

The cost is the `MessageDisplay` hook. Claude Code runs it on every chunk of every reply, in
every session, whether or not it ever consults, and each run starts a process. On a chunk
that isn't the last, it reads two fields and touches nothing on disk. Measured 2026-09-26
on Windows 11, release build, 20 runs on a non-final chunk: median 15.4 ms when started
through .NET's `Process` API and 8.1 ms under PowerShell's `Measure-Command`, against
15.5 ms and 7.7 ms for `claude-consult --version` measured the same two ways. So that path
costs about as much as starting the binary at all. The Python hook it replaced took about
40 ms per run (measured 2026-09-25, median; a bare Python start was about 32 ms). With
`--summary-style off` the hook isn't registered at all. The status line also starts the
binary each time it refreshes.

Headless runs get no summary: `claude -p`, and any run whose `CLAUDE_CODE_ENTRYPOINT`
starts with `sdk`. There, display output lands in stdout, inside whatever a script
captures, so the display hook stays silent. The call still counts towards the session's
total.

### Token budgets and truncation

On OpenRouter, `max_tokens` caps *completion* tokens, and for a reasoning model that
includes its private thinking. Measured 2026-08-01: Kimi K3 given `max_tokens=3000` spent
2,997 tokens reasoning and emitted nothing. So investigation steps run with a 16K ceiling,
the written review with 32K, and when `finish_reason == "length"` the server makes up to
three stitched continuation requests.

A review counts as complete only when its last response ended with `finish_reason` `stop`.
Anything else keeps its text but is marked incomplete, with the reason in a warning above
it: still at the ceiling after three continuations, a continuation request that failed, a
content filter, or no finish reason at all. Its status record says `"complete":false` and
the summary shows ✗. The check is only as good as the provider's report: a model that stops
early of its own accord reports `stop` like any other.

Don't "fix" this with `reasoning: {max_tokens: N}`. Measured on Kimi K3, that collapsed
reasoning from ~4.8K tokens to 7.

### Cost

Prompt caching is automatic on OpenRouter for these models. The measured hit rate on real
reviews was 84–86%, and the output reports it per reviewer. The main cost driver is the
model, not the cache. Same review, same repo, 2026-08-01, at that day's prices:

| reviewer | tool calls | input tokens | cost |
|---|---|---|---|
| deepseek-v4-pro | 38 | 1.48M | $0.21 |
| glm-5.2 | 28 | 1.83M | $0.40 |
| kimi-k3 | 36 | 0.78M | $0.76 |

A per-reviewer budget (default $1.00, `--max-cost` on `run`) stops a reviewer that keeps
finding more to read and makes it write up what it has. Steps default to 24 per reviewer.

**Don't cut cost by shrinking tool results.** Measured 2026-08-01: cutting `read_file` from
400 to 250 lines made the reviewer page instead. `read_file` calls went from 11 to 21, input
from 706K to 777K, and cost from $0.687 to $0.755. Smaller results don't shrink the context;
they spread it over more steps, and every step resends the whole conversation.

## Repository layout

A Cargo workspace with one binary and seven libraries:

| path | role |
|---|---|
| `crates/claude-consult` | the only binary: a thin clap front end dispatching to the libraries |
| `crates/consult-core` | everything shared: the catalog, the registry, the key, the OpenRouter client and live listing, the read-only sandbox, the panel (prompts, lenses, agentic loop, budgets, rendering, status records), the `settings.json` merge and file generation |
| `crates/consult-mcp` | the MCP server (`consult`, `consult_clean`, `list_reviewers`) over stdio and streamable HTTP, the progress heartbeat, and the runner behind `run` and `reviewers` |
| `crates/consult-hooks` | the summary, display and status line hooks |
| `crates/consult-service` | the shared service: the Windows scheduled task, through `schtasks.exe` |
| `crates/consult-tui` | shared terminal UI pieces: theme, the panel picker, text input, confirm, step log |
| `crates/consult-install` | install, upgrade and uninstall, in full-screen, plain and unattended form, and the Python-install cleanup |
| `crates/consult-manage` | the `manage` TUI |
| `templates/` | slash commands, skill and the `verify-claims` workflow, with `{{PLACEHOLDERS}}` filled from the favourites, the panel and the live listing; built into the binary |
| `catalog.json` | the curated favourites, recommended and budget panels; no prices; built into the binary |
| `.github/workflows/` | `ci.yml` (format, clippy, tests on three platforms) and `release.yml` (release archives on a version tag) |
| `.mcp.json` | development only: a Rust documentation server (`rustdocs`) for Claude Code sessions in this repo; not part of what the installer ships |

Tests sit next to the code and under `crates/<crate>/tests/`. `cargo test --workspace` runs
offline, with no key and no network: HTTP is tested against local mock servers, every file
a test writes is in a temporary directory, and no test touches the real Claude dir, the
real scheduled task or the `claude` CLI. The one exception is ignored by default and
registers a throwaway scheduled task (never `OpenRouterMCP`), to run by hand on Windows:
`cargo test -p consult-service --test live_task -- --ignored`.

`.mcp.json` registers [rustdoc-mcp](https://crates.io/crates/rustdoc-mcp) for anyone working on
this repository with Claude Code. It looks up any crate on docs.rs, the standard library and the
workspace's own crates. Claude Code asks you to approve it the first time a session starts
here; it only runs if it is installed:

```sh
cargo install rustdoc-mcp
rustup toolchain install nightly                             # it reads rustdoc's JSON output
rustup component add rust-docs-json --toolchain nightly      # for std, core and alloc
```

## Development

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

CI runs those three on `windows-latest`, `ubuntu-latest` and `macos-latest`, with warnings
as errors. Exactly one package in the workspace has a binary, which is what lets
`cargo install --git` work; keep it that way.

To try an install without touching your real one, point it at scratch directories and
leave the service and the MCP registration alone:

```sh
cargo run -p claude-consult -- install --install-dir /tmp/cc-install --claude-dir /tmp/cc-claude --skip-service --skip-mcp-registration
```

That still asks for a key and checks it with OpenRouter (free), unless you add
`--skip-key-check`. Any real consult, from `run` or a session, is billed.

`CLAUDE_CONSULT_OPENROUTER_BASE_URL` points the OpenRouter client at another API root. It
exists for the end-to-end tests, which run the real binary against a mock server. It is not
a user setting: the key is sent to whatever root it names.

**Releasing.** Set `version` under `[workspace.package]` in `Cargo.toml`, commit, and push
a tag `vX.Y.Z` matching it. The release workflow builds `x86_64-pc-windows-msvc`,
`x86_64-unknown-linux-gnu`, `aarch64-apple-darwin` and `x86_64-apple-darwin`, packs each
binary with `README.md` and `LICENSE` as `claude-consult-vX.Y.Z-<target>.zip` (Windows)
or `.tar.gz`, and attaches them to a GitHub release with generated notes. cargo-binstall
finds the archives by the crate's version, so the tag and the version must agree.

Every commit follows [Conventional Commits](https://www.conventionalcommits.org/):
`type(scope): summary`, with the type one of `feat`, `fix`, `docs`, `refactor`, `perf`,
`test`, `build`, `ci` or `chore`. Anything that breaks an existing install or a caller gets
a `!` after the type and a `BREAKING CHANGE:` footer.

## License

[MIT](LICENSE).
