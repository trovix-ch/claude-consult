---
description: Get a clean, unbiased architectural perspective on a problem, with zero repo context
argument-hint: [the problem in general technical terms] [optionally: with <model> / everyone]
allowed-tools: mcp__openrouter__consult_clean
---

## What this produces

A **clean architectural perspective on a problem** — what a good engineer considers the
technically cleanest architecture, judged purely on its own merits. That is the whole
deliverable. Nothing more is being asked for and nothing more should be implied.

What we do with it afterwards is our business and stays on our side: read it against what
we have, see whether anything is worth adjusting, and notice whether they raised a problem
we should look into. **None of that goes into the question, and the responders must not
learn any of it.** A model told its answer will be weighed against an existing system has
a reason to argue for change rather than to say what it actually thinks is clean.

Problem: $ARGUMENTS

## Writing the question — this is the whole job

The failure mode is **me**. Our architecture is in my head and leaks by reflex: through
component names, internal vocabulary, how I frame the problem, even which details I judge
worth mentioning.

The subtler failure is leaking that *any* prior work exists. Writing "we currently do X",
"our existing approach", or even "I'm deliberately not showing you our code" all tell the
responder there is a system to react to. Once it knows that, it stops answering the
question and starts responding to an implied design.

**The test: the question must read as if posed by someone who has no reason to believe any
prior work exists.** A problem, stated plainly, to an engineer being asked how they would
build it.

**Do not include:**
- Code. No snippets, not "for context", not in passing.
- File, module, class, function, or table names from our codebase.
- Our internal jargon — translate every internal term into what the thing actually is.
- Any statement that an implementation exists, including denials of it.
- The answer smuggled into the question. "How should we improve our X-based pipeline" has
  already fixed the answer to X.

**Do include:**
- What the system is ultimately for, and what success looks like.
- The problem in general programming and systems terms — its *shape*, not our instance.
- Real constraints: scale and volume, latency tolerance, online vs offline, what data
  exists and where it comes from, consistency and reliability needs, what is immovable.
- What is genuinely out of scope, so the answer stays on the problem that matters.

## Choosing responders

Defaults to the panel — {{PANEL_ALIASES}} — each pointed at a different aspect of the
problem (simplicity / scale-and-failure / question-the-frame / data-and-state). These
direct attention, not conclusions, so each answer is still that model's honest view.

Clean room is one request per responder with no tool loop, so it costs cents even on
expensive models — grounded reviews are dear because every investigation step resends the
whole conversation, and clean room has no steps. That makes it the place to name a
premium model you would not put on the grounded panel: {{CLEAN_ROOM_EXTRAS}}.

Read a model preference out of the request and pass it as `models`:

- a named model → just that alias (call `list_reviewers` for the roster; an OpenRouter id
  also works if OpenRouter lists it as tool-capable — presets, routers and Anthropic models
  are refused).
- "everyone" / "all of them" → `{{PANEL_PLUS_EXTRAS_JSON}}`, one aspect each.
- nothing stated → the default panel.

## Reporting back

- Lead with the architecture they actually propose — components, boundaries, ownership of
  state, data flow. Give me their view on its own terms first.
- Then, separately, where it differs structurally from ours. Convergence is mild
  reassurance; divergence is the thing worth thinking about.
- Each response names the assumptions it rests on and the one fact that would change its
  recommendation. Check those against our real situation and say which hold — that is
  where a clean-room answer either gains or loses its force.
- Some differences are genuine improvements; others exist only because the responder
  didn't know a real constraint. **Separating those two is the deliverable.** Cleaner in
  the abstract is not automatically better here.
- Close with what you would actually change, if anything, and what you would leave alone.
  A recommendation of "nothing, and here is why theirs doesn't apply" is a fine outcome.
