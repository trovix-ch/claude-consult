---
description: Full {{PANEL_SIZE}}-model panel — verify a plan, diagnose a stuck problem, or get a fresh proposal
argument-hint: [what to look at, or blank for what we just discussed]
allowed-tools: mcp__openrouter__consult, Read, Glob, Grep
---

The full panel: {{PANEL_NAMES}} — {{PANEL_SIZE}} reviewers, concurrent, a different focus
each. Use it when the decision is expensive to get wrong and disagreement between
reviewers is worth paying for. For a routine second opinion one model is enough:
{{QUICK_COMMANDS}}.

Topic: $ARGUMENTS

(If blank, take up whatever we have been working on in this conversation.)

## Pick the mode first — this matters more than anything else here

**`mode: "review"`** (the default) — we have formed a plan, design, or proposal and want
it assessed. Reviewers check whether its assumptions hold in the code, what it gets wrong,
and what it omits. Covers all of:

- verifying a plan during planning, before we commit to building it
- an idea or proposal we want judged on its merits
- a finished piece of work we want a second opinion on

**`mode: "diagnose"`** — something is wrong and **we have not established why**. Reviewers
work from evidence in the code, rank competing explanations by how well each fits, and
name the cheapest check that would confirm or kill the leading one. Use it when:

- we hit a problem during or before planning and cannot nail the cause
- we have a theory but it does not fully fit the symptoms
- we are stuck and need a direction to steer toward next

Sending an unsolved problem in `review` mode asks the wrong question — reviewers assess a
plan that does not exist yet, instead of finding out what is actually happening. When we
are stuck, that is the mistake to avoid.

## Writing the question

They cannot see this conversation. The `question` must stand alone, and its quality
decides the quality of what comes back.

**For review:** the goal, the plan or idea in specific terms, the constraints that bind,
and what has already been ruled out and why. Present it as under consideration rather than
settled, and do not signal which way I lean or that the user already agreed — agreement
already in the room is what an outside panel is for testing. Include the rejected
alternatives, or they will propose them back at us with no way to know we had considered them.

**For diagnose:** the symptom precisely, exactly as observed rather than as interpreted.
What was expected instead. What we have already checked and ruled out, with the evidence.
Any theory we hold should be labelled as a theory, not stated as background fact — a
hypothesis presented as established is the fastest way to get three reviewers confirming
the wrong cause. Include the real error text, logs, or reproduction steps if we have them.

Pass `root` (absolute path), and `attachments` for files clearly central — for diagnosis,
the code on the failing path.

## Reporting back

- Lead with where reviewers **disagree** — with each other, or with our thinking. That is
  the signal worth paying for; consensus mostly restates what we already believed.
- Separate what they verified against the code from what they asserted. Their training
  data predates this codebase, so check any claim that would change what we do, using
  Read/Grep, before accepting it. Say which held up and which did not.
- In diagnose mode, run the cheap confirming checks they name if we can do so quickly —
  a named check that costs a grep is worth doing before reporting back.
- Do not simply relay the reviews. You have context they lack; your judgement on their
  judgement is the deliverable.
- End with your own recommendation, and say plainly if the panel did not settle it.
