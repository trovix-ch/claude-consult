---
description: Quick outside view from {{DISPLAY}} on what we're currently discussing — {{TAGLINE}}
argument-hint: [a question, e.g. "what are your thoughts?" — or blank]
allowed-tools: mcp__openrouter__consult, mcp__openrouter__consult_clean, Read, Glob, Grep
---

Quick outside view on what we are discussing right now, from **{{DISPLAY}}**
(`{{ALIAS}}`, {{PRICE}}).

{{PITCH}}

Asked: $ARGUMENTS
(If blank, treat it as "what do you think of what is on the table?")

## Turning the conversation into the question

It cannot see any of this conversation. You write the `question`, and it must stand alone:
what is on the table, what it is meant to achieve, and the constraints that came up.

**Neutrality is the part that needs care.** How you present it decides what comes back:

- Present the idea as **under consideration** — not as settled, not as mine, not as
  something already agreed. Do not write "I recommend X, is that right?"; state what X is
  and let it judge.
- **Include the alternatives we discussed and why they were set aside.** Without them it
  re-proposes what we already rejected, and it cannot tell us we rejected something wrongly.
- Do not signal which option I favour, how confident I am, or that the user seemed to
  agree. Agreement already in the room is precisely what an outside view is for testing.
- Do not flatten open questions into decided ones. If we did not settle something, say so.
- Keep it factual and reasonably short. This is a quick check, not a formal brief.

Call `consult` with `models: ["{{ALIAS}}"]` and `root` (absolute path) so it can read the
code; add `attachments` only for files clearly central to the question. Use
`mode: "diagnose"` instead of the default if we are chasing an unexplained problem rather
than weighing a proposal.

For an architecture question where our existing code would bias the answer, use
`consult_clean` with `models: ["{{ALIAS}}"]` — see `/cleanroom` for how to write that
question.

## Reporting back

- Give me its answer straight, **including when it agrees**. A quick confirmation is a
  real result; do not inflate it into something longer to seem worthwhile.
- Verify any specific claim about our code before accepting it — its training data
  predates this codebase and it does sometimes call correct code broken.
- Then your own view: what you would act on, and where you still disagree and why.
