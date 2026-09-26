---
name: openrouter-workflow
description: Reference for when and how to combine Anthropic and OpenRouter models in a multi-agent workflow. Use when planning a Workflow script, deciding which model runs which stage, or judging whether cross-lineage review is worth its cost.
argument-hint: [what the workflow should accomplish]
---

Building multi-agent workflows that use both Anthropic models and the OpenRouter models
behind the `consult` MCP server.

Task: $ARGUMENTS

## Decide whether to mix at all

Mixing costs money, latency, and sends repository context to a third party. Work through
these before designing a mixed workflow — if the answers don't line up, use one lineage
and spend the budget elsewhere.

- **Is the output hard to verify by running something?** A test suite, a type checker, or
  a reproduction beats any model opinion. Mixing earns its cost where correctness is a
  judgement call, not where it is checkable.
- **Is a missed failure expensive?** Latency and spend are certain; the benefit is not.
- **Can you name what a given stage is meant to decorrelate?** If not, that stage doesn't
  need a second lineage.
- **Will the outside model actually receive the artifact it is judging?** See *Artifact
  handoff* below — this is the most common way a mixed workflow silently does nothing.
- **May this code leave the machine?** Every grounded consult sends repository-derived
  context to OpenRouter. Read-only means reviewers can't *modify* the repo; it does not
  mean the data stays local.

**The premise, stated as a premise:** models from different labs are assumed to fail in
less overlapping ways than one lab's models do, so a judge from another lineage may catch
what the generator missed. That is a working assumption about training independence, not
a measured result — labs share data sources, benchmarks, and conventions, and a strongly
anchored prompt can align any two models regardless of origin. Treat the benefit as
plausible rather than established.

## What is actually possible — and the limit

**A workflow subagent cannot be an OpenRouter model.** `opts.model` accepts Anthropic
models only (a Claude Code platform constraint, not something this repo enforces).

**A workflow script cannot call a tool.** Only an agent can call `mcp__openrouter__consult`
(grounded, reads the repo) or `mcp__openrouter__consult_clean` (question only, no repo), so
every consult goes through an agent that does nothing else — a relay, see *Relays*.

So the achievable shape is: **Anthropic agents drive; OpenRouter models are consulted at
decision points.**

**The limit worth naming up front:** because an Anthropic agent is always the last to
touch the result, **the final synthesis stays Anthropic.** This decorrelates *generating*
from *judging*, but not the step that chooses and fuses — an Anthropic synthesizer can
reintroduce the lineage's preferences when it decides which review to act on. If that
last step is what matters for your case, this pattern does not solve it; the closest
approximation is to have the synthesis stage state each outside model's verdict verbatim
and justify any it overrode, so the override is visible rather than silent. Counting the
verdicts need not involve a model at all — see *Asking for a verdict*.

## Artifact handoff — the failure that looks like success

**Reviewers cannot see the workflow's conversation or state.** A grounded reviewer gets
only the `question` you write, an auto-generated project orientation, and any
`attachments`. So a stage that asks *"review the design"* without including the design
gets a review of the repository and the prompt — plausible output, no relationship to the
candidate. Nothing errors.

Every mixed stage must therefore:

- **Serialize the artifact into `question`** — the candidate design, the findings, the
  diff under review. If it is not in the question text or an attachment, it does not exist.
- **Use `attachments`** for files that are central, rather than hoping the reviewer finds them.
- **Vary the brief across reviewers.** The `consult` panel gives each model in one call a
  different lens; separate single-model calls get no such spread, so there the brief is
  the only thing that differs. A workflow that fans out the *same* question gets
  correlated answers and loses the thing it paid for. Vary what each brief asks the
  reviewer to examine, never which answer it should reach.
- **Pick the right tool:** `consult` (has repo context) for judging something concrete;
  `consult_clean` (no context) only for design questions where the existing implementation
  would anchor the answer — see `/cleanroom`.

## Relays — a model carries the review, code reads it

An agent that exists only to load the consult tool, call it once and hand back what came
out is a relay. It should understand nothing on the way through.

- **Every relay runs on Haiku: `model: "haiku"` on each relay `agent()` call**, whatever
  the session model, whatever model the user named for the workflow, and whatever a
  general preference says about subagents. Loading a tool, calling it once and copying
  the result is mechanical, and nearly all of its wall-clock is spent waiting on that one
  call. Keep the stronger models for stages that do real work: finding, reproducing,
  synthesizing.
- **The script writes the arguments; the relay passes them on verbatim.** Hand it the
  tool's exact arguments as JSON. A relay that rephrases can add the lean the question
  was written to leave out.
- **The relay returns the tool output verbatim and adds nothing.** Call it without a
  schema, so `agent()` returns that text, and parse it in script code. A relay asked to
  report the verdict, or to quote "the decisive passage", chooses what the tally sees: an
  Anthropic model judging the outside review before it is counted, which is the step the
  stage was meant to take away from it.
- **Copying can drift.** A small model can drop or alter parts of a long review as it
  copies it back out. What the script reads is short and matched exactly, so drift
  reaches only what a human reads — and a record or verdict line that did not survive
  counts as no signal, which is the safe direction to fail.

**Reading the result.** Every `consult` and `consult_clean` result ends with one status
record per reviewer, one per line, in panel order: `<!-- consult-result v1 {...} -->`,
whose JSON carries `alias`, `status`, `complete`, `capped` and `cost_usd` among others.
Script code:

- takes records only from the unbroken run of record lines at the very end, since a
  reviewer can quote the format in its review. No such block, or a malformed line in it,
  is **no signal**: an older install, or a relay that dropped it.
- counts a review as finished only when `complete` is true (`status` `ok`). `incomplete`
  keeps text that may be cut off, `empty` and `error` carry no review, and a single `failed`
  record with a null `alias` means the call failed before any reviewer ran.
- reads `capped: true` as an investigation that a step, time or cost budget cut short. The
  review can still be complete, but it has not endorsed what it did not reach.
- sums `cost_usd` for the spend.

The progress line and the summary drawn on screen are display only and never part of the
result; their styles are install options described in the project README.

## Asking for a verdict — only when the script counts it

A normal consult never asks for one: the review's prose, ending in its "Bottom line", is
the signal. Only a workflow whose script counts or branches on the outside verdict asks,
in its own question — first to try to refute the claim, then to end with one fixed line,
such as `VERDICT: CONFIRMED | REFUTED | INCONCLUSIVE`.

- **One model per call**, so each result holds one review and the one record it belongs
  to. In a panel result, pairing each verdict line with its reviewer's record is guesswork.
- **Match it in script code:** the last line that is `VERDICT:` plus one of the values and
  nothing else (allow for markdown emphasis), counted only when that record has
  `complete: true`. No such line is no signal, not assent.
- **The line is for counting.** The prose and its Bottom line stay the primary signal, and
  they are what the synthesis stage reads.

## Model roster

### Anthropic — workflow agent brains (`opts.model`)

| model | reach for it when | $/M in-out |
|---|---|---|
| `fable` | the hardest reasoning and longest-horizon agentic work | 10 / 50 |
| `opus` | deep reasoning, complex agentic coding — the default for hard stages | 5 / 25 |
| `sonnet` | near-Opus on coding and agentic work at lower cost — good for wide fan-out | 3 / 15 |
| `haiku` | fast mechanical work: classification, extraction, simple transforms — and every relay | 1 / 5 |

Context is 1M except Haiku 4.5 at 200K. These figures drift — check the current
Anthropic model reference rather than trusting this table indefinitely.

**Named models are honoured.** A model the user names for the workflow ("a sonnet
workflow") runs every Anthropic stage except the relays: pass it as `opts.model` on each
of them rather than leaving them to the session model. With none named, omit `opts.model`
to inherit the session model and set it only where a different tier clearly fits. Outside
voices the user names each get their own relay call and a different brief: "a sonnet
workflow with 2 deepseek voices" is Sonnet stages, two Haiku relays and two differently
briefed DeepSeek calls. `models` takes an alias that `list_reviewers` shows, an
OpenRouter id or a quick-command name, tried in that order; anything else must be a model
OpenRouter lists as tool-capable. An Anthropic model, a preset (`@...`) or a router that
picks the model per request is refused, however it is named, before any request.

### OpenRouter — consulted via the `consult` tools

Default panel, as chosen at install time ({{GENERATED_ON}}):

| alias | lab | plays to | $/M in-out ({{PRICES_AS_OF}}) |
|---|---|---|---|
{{PANEL_TABLE}}

Premium models are poor value on the grounded panel — every investigation step resends the
whole conversation — but cheap in clean room, which has no tool loop: {{CLEAN_ROOM_EXTRAS}}.
Call `list_reviewers` for the current roster and prices rather than trusting this table;
the registry is the source of truth.

## Where outside models help

- **Judging candidates.** Anthropic agents produce N designs; an outside model scores them
  — with the designs actually in the question.
- **Verifying claims.** A stage that produces findings hands them to a different lineage to
  confirm or refute, rather than self-verifying.
- **Unanchored design input.** `consult_clean` before the Anthropic agents see the code.
  The answer is unanchored by the existing design *and* unverified against it — some
  differences are real improvements, others exist only because a constraint was unknown.
- **Diagnosis.** `mode: "diagnose"` (default is `"review"`) when a stage must establish a
  cause rather than assess a plan.

Not where the work is mechanical, and not where the answer is checkable by running something.

## Silence is not agreement — at every stage

A stage stopped part-way can hand back something that reads exactly like a clean pass.
This holds for every stage the script reads, not only the mixed ones:

- **Outside reviews:** anything short of a trailing record with `complete: true` is no
  signal, never assent.
- **Relays:** `agent()` returns `null` when an agent dies or is skipped — no signal. A
  relay that dies after its consult ran has still been billed, and a `null` does not say
  whether the consult ran, so a retry may pay twice.
- **Anthropic stages:** a stage stopped by a safety classifier, a turn limit or an API
  error can still return a well-formed empty result, and "found nothing" then reads
  exactly like "never finished". Any stage whose empty output would mean *all clear* must
  say whether it finished: a required completion flag, with a reason when it is false. A
  false flag or a `null` is a gap, never a clean pass.
- **Decide explicitly** whether a stage with a gap proceeds with one fewer lineage or
  halts — and record which reviewers and stages actually delivered, so the synthesis stage
  cannot present partial coverage as consensus.
- **Tolerate the tool being absent.** If the MCP server connected after the session
  started, ToolSearch may not surface it and the relay comes back without records. Degrade
  to one fewer lineage rather than failing the workflow.

## Cost and time

- Measured three-model panels (2026-08, DeepSeek/GLM/Luna panel): **~0.11 USD** on a small
  repo, **~0.23 USD** on a medium one, up to **~0.75 USD** on a large one. Naming a single model
  is roughly a third of that. The records' `cost_usd` is the real figure; trust it over these.
- Costs compound with fan-out: five agents each running a panel is five panels.
- **Prefer one named model per call** and let the workflow's own fan-out supply the
  spread — cheaper than nesting panels inside agents, and gives you control over which
  lineage sees what.
- A grounded consult runs for minutes and can outlast the reviewers' own 25-minute
  wall-clock budget. Check that a workflow agent tolerates a single tool call blocking
  that long before relying on it; clean-room calls return in seconds.
- Concurrent consults share one API key — rate limiting degrades gracefully via backoff
  but stretches wall-clock well past the single-panel figure.

## Before running one

Workflows spawn many agents and spend real money, so they need explicit opt-in. If the
request is merely workflow-shaped, describe what you would run and its rough cost, and ask.

Sketch the stages and say what each mixed stage decorrelates before writing the script.
