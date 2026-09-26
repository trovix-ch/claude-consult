export const meta = {
  name: 'verify-claims',
  description: 'Verify claims across model families: per claim, a local reproduction and one outside review per voice, tallied in code',
  whenToUse: 'Checking concrete claims about a project (defects, behaviours) against a local reproduction and reviewers from other labs through the consult MCP server. Costs one OpenRouter consult per claim per voice; args.dryRun exercises the plumbing without that spend.',
  phases: [
    { title: 'Find', detail: 'only when args.claims is absent: one agent proposes up to 5 claims about args.scope' },
    { title: 'Verify', detail: 'per claim, concurrently: a local reproduction and one Haiku relay per outside voice' },
  ],
}

const DEFAULT_VOICES = {{WORKFLOW_VOICES}}

const USAGE =
  'args: { root: absolute path of the project (required); ' +
  'claims?: [{ id, claim, where?, mechanism?, repro? }]; ' +
  'scope?: what a finder should examine, required when claims is absent; ' +
  'voices?: outside reviewer aliases or OpenRouter ids, one consult per claim each, repeat one to hear it twice under another brief (default: the installed panel); ' +
  'model?: Anthropic model for the finder and the local check (default: the session model); ' +
  'localCheck?: boolean, default true; ' +
  'dryRun?: boolean, needs claims; runs no finder and no local check, the relays return canned output and nothing is sent to OpenRouter }'

const ARG_KEYS = ['root', 'claims', 'scope', 'voices', 'model', 'localCheck', 'dryRun']
const CLAIM_KEYS = ['id', 'claim', 'where', 'mechanism', 'repro']
const MAX_FOUND = 5

// Each voice slot is pointed at a different part of the problem, so two calls to
// one model are not the same question twice. A lens steers attention only: every
// slot gets the same instruction about how to reach and state its verdict.
const LENSES = [
  { name: 'code path', text: 'the code path itself: trace the control flow and the data from the stated input to the stated behaviour.' },
  { name: 'preconditions', text: 'the preconditions: whether the triggering input or state can actually arise from the callers, and what validation, defaults or guards stand in its way.' },
  { name: 'surroundings', text: 'the surroundings: tests, error handling, configuration and other callers that already cover, contradict or depend on this case.' },
  { name: 'evidence', text: "the claim's own evidence: whether the cited location, mechanism and reproduction say what the claim needs them to say." },
]

const FOUND = {
  type: 'object',
  properties: {
    completed: { type: 'boolean', description: 'true only if you examined the whole scope; an empty list with completed=true means "looked, found nothing"' },
    incomplete_reason: { type: 'string', description: 'why you did not finish; empty when completed' },
    claims: {
      type: 'array',
      maxItems: MAX_FOUND,
      items: {
        type: 'object',
        properties: {
          id: { type: 'string' },
          claim: { type: 'string', description: 'one sentence: the behaviour claimed' },
          where: { type: 'string', description: 'file:line' },
          mechanism: { type: 'string', description: 'why the code produces it, citing the exact lines' },
          repro: { type: 'string', description: 'a concrete input or sequence that would show it' },
        },
        required: ['id', 'claim', 'where', 'mechanism', 'repro'],
      },
    },
  },
  required: ['completed', 'incomplete_reason', 'claims'],
}

const LOCAL = {
  type: 'object',
  properties: {
    verdict: { type: 'string', enum: ['confirmed', 'refuted', 'inconclusive'] },
    ran_code: { type: 'boolean', description: 'true only if you executed code and the verdict rests on its output' },
    evidence: { type: 'string', description: 'what you ran and the decisive output, abbreviated' },
  },
  required: ['verdict', 'ran_code', 'evidence'],
}

const RECORD_PREFIX = '<!-- consult-result'
const RECORD = /^<!-- consult-result v1 (\{.*\}) -->$/
const VERDICT_LEAD = /^VERDICT\s*:/i
const VERDICT = /^VERDICT\s*:\s*(CONFIRMED|REFUTED|INCONCLUSIVE)\s*\.?$/i

const isText = v => typeof v === 'string' && v.trim() !== ''
const optText = v => (isText(v) ? v.trim() : null)
const round4 = x => Math.round(x * 10000) / 10000
// agent() can also throw (a spent budget, a skipped run); either way it is a gap.
const settle = start => Promise.resolve().then(start).catch(() => null)

function readArgs(raw) {
  let a = raw
  if (typeof a === 'string') {
    try { a = JSON.parse(a) } catch (e) { return { errors: ['args is a string that is not JSON; pass an object'] } }
  }
  if (!a || typeof a !== 'object' || Array.isArray(a)) return { errors: ['args must be an object'] }
  const errors = []
  const unknown = Object.keys(a).filter(k => !ARG_KEYS.includes(k))
  if (unknown.length) errors.push(`unknown key(s): ${unknown.join(', ')}`)
  if (!isText(a.root)) errors.push('root must be the absolute path of the project')

  let claims = null
  if (a.claims !== undefined && a.claims !== null) {
    if (!Array.isArray(a.claims) || !a.claims.length) {
      errors.push('claims must be a non-empty array, or left out so a finder proposes them')
    } else {
      claims = a.claims.map((c, i) => {
        if (!c || typeof c !== 'object' || Array.isArray(c) || !isText(c.claim)) {
          errors.push(`claims[${i}] needs a claim text`)
          return null
        }
        const extra = Object.keys(c).filter(k => !CLAIM_KEYS.includes(k))
        if (extra.length) errors.push(`claims[${i}] has unknown key(s): ${extra.join(', ')}`)
        for (const k of ['id', 'where', 'mechanism', 'repro']) {
          if (c[k] !== undefined && c[k] !== null && typeof c[k] !== 'string') errors.push(`claims[${i}].${k} must be a string`)
        }
        return { id: optText(c.id) || `c${i + 1}`, claim: c.claim.trim(), where: optText(c.where), mechanism: optText(c.mechanism), repro: optText(c.repro) }
      })
      const ids = claims.filter(Boolean).map(c => c.id)
      const dup = ids.filter((id, i) => ids.indexOf(id) !== i)
      if (dup.length) errors.push(`claim ids repeat: ${[...new Set(dup)].join(', ')}`)
    }
  } else if (!isText(a.scope)) {
    errors.push('without claims, scope must say what the finder should examine')
  }

  let voices = DEFAULT_VOICES
  if (a.voices !== undefined && a.voices !== null) {
    if (!Array.isArray(a.voices) || !a.voices.length || !a.voices.every(isText)) {
      errors.push('voices must be a non-empty array of reviewer aliases or OpenRouter ids')
    } else {
      voices = a.voices.map(v => v.trim())
    }
  } else if (!Array.isArray(voices) || !voices.length || !voices.every(isText)) {
    errors.push('no voices given and no default panel installed; pass voices')
  }
  if (a.model !== undefined && a.model !== null && !isText(a.model)) errors.push('model must be a model name such as sonnet or opus')
  for (const k of ['localCheck', 'dryRun']) {
    if (a[k] !== undefined && a[k] !== null && typeof a[k] !== 'boolean') errors.push(`${k} must be true or false`)
  }
  // A dry run runs no finder and no local check, the two stages that could reach
  // OpenRouter on their own, so only the relays run, and they get canned output.
  const dryRun = a.dryRun === true
  if (dryRun && (a.claims === undefined || a.claims === null)) errors.push('dryRun needs claims: a dry run runs no finder')
  if (dryRun && a.localCheck === true) errors.push('dryRun runs no local check; leave localCheck out or set it to false')
  return {
    errors,
    root: isText(a.root) ? a.root.trim() : null,
    claims,
    scope: optText(a.scope),
    voices,
    model: optText(a.model),
    localCheck: a.localCheck !== false && !dryRun,
    dryRun,
  }
}

function tidyFound(list) {
  const out = []
  const seen = new Set()
  list.forEach((c, i) => {
    if (!c || !isText(c.claim)) return
    let id = optText(c.id) || `c${i + 1}`
    if (seen.has(id)) id = `${id}-${i + 1}`
    seen.add(id)
    out.push({ id, claim: c.claim.trim(), where: optText(c.where), mechanism: optText(c.mechanism), repro: optText(c.repro) })
  })
  return out
}

function claimBlock(c) {
  return [
    `Claim ${c.id}: ${c.claim}`,
    c.where ? `Where: ${c.where}` : null,
    c.mechanism ? `Mechanism given: ${c.mechanism}` : null,
    c.repro ? `Proposed reproduction: ${c.repro}` : null,
  ].filter(Boolean).join('\n')
}

// For the Anthropic stages only: an outside review they started themselves would
// be billed without being counted, tallied or shown.
const NO_OUTSIDE = "Do not call any mcp__openrouter__ tool: outside reviews go only through this workflow's relays, where they are counted."

function finderPrompt(root, scope) {
  return `Examine the project at ${root}. The scope, in the user's words: ${scope}\n\n` +
    `Propose at most ${MAX_FOUND} concrete claims within that scope. A claim is a specific statement about how the code behaves ` +
    `that running it, or a careful reader, could confirm or refute. For each give where (file:line), the mechanism (why the code ` +
    `produces it, citing the lines) and a concrete input or sequence that would show it. Use ids c1, c2, and so on. ` +
    `Prefer fewer, specific claims to padding. Do not modify any file. ${NO_OUTSIDE}\n\n` +
    `Set completed=true only if you examined the whole scope. If you could not finish, set completed=false and say why in ` +
    `incomplete_reason: an empty list with completed=true reads as "looked everywhere, found nothing", so never return that ` +
    `for a scope you did not finish.`
}

function localPrompt(root, c) {
  return `Check this claim about the project at ${root} by running something, not by reasoning alone.\n\n${claimBlock(c)}\n\n` +
    `Never modify anything under ${root}: work in a fresh temporary directory outside it (your session scratchpad if you have one). ` +
    `Never make a network call, and never read, print or use an API key or other credential; replace anything that would reach ` +
    `the network with a local stub. ${NO_OUTSIDE}\n\n` +
    `confirmed: you ran code and its output shows the claimed behaviour. refuted: you ran the claimed input and its output shows ` +
    `correct behaviour. inconclusive: you could not build a run that settles it; say why. Prefer inconclusive to a guess.`
}

function question(c, lens) {
  return [
    'Below is a claim about this project. Try first to refute it: find the code that handles the case, or show why the stated ' +
      'mechanism or reproduction does not produce the claimed behaviour. Only if you cannot refute it, say it is confirmed and ' +
      'trace the exact path through the code that produces it. If the code does not settle it either way, say what would.',
    '',
    `Look in particular at ${lens.text}`,
    '',
    claimBlock(c),
    '',
    'End your review with one line that holds only the verdict: VERDICT: CONFIRMED, VERDICT: REFUTED or VERDICT: INCONCLUSIVE.',
  ].join('\n')
}

function relayPrompt(toolArgs) {
  return 'You are a relay between this workflow and an outside reviewer. Do exactly this and nothing else:\n\n' +
    '1. Load the tool with ToolSearch, query "select:mcp__openrouter__consult".\n' +
    '2. Call mcp__openrouter__consult once, with exactly the arguments between <arguments> and </arguments> below. ' +
    'They are JSON: pass every field unchanged and the question word for word.\n' +
    '3. Reply with the tool output exactly as it came back, character for character, including the lines at the end that ' +
    'start with "<!-- consult-result". Add nothing before or after it: no preamble, no summary, no code fence.\n\n' +
    'If the tool cannot be found, reply only: RELAY: consult tool not found\n' +
    'If the call returns an error instead of output, reply only: RELAY: tool error: followed by the error text\n' +
    'Never call the tool a second time: every call is billed. Do not read files and do not judge the review.\n\n' +
    '<arguments>\n' + JSON.stringify(toolArgs, null, 2) + '\n</arguments>'
}

// The canned output never names the consult tool, so a dry-run relay has nothing
// to reach for. Complete reviews say INCONCLUSIVE and only an incomplete one says
// CONFIRMED, so an outside CONFIRMED or REFUTED in a dry run means the parser let
// through a verdict it should have dropped. The complete one claims a tool call,
// or it would be dropped as a review that opened nothing.
function cannedOutput(voice, complete) {
  const record = {
    alias: voice, short: voice, status: complete ? 'ok' : 'incomplete', complete,
    finish: complete ? 'stop' : 'length', capped: false, tool_calls: complete ? 1 : 0, cost_usd: 0,
    tokens_in: 0, tokens_out: 0, seconds: 0,
  }
  return [
    '# Panel review - 1 reviewer',
    'Dry run - nothing was sent to OpenRouter - 0 USD',
    '',
    `## ${voice} (canned)`,
    complete ? '*canned: complete*' : '*canned: incomplete - stopped at the token limit, the review below may be cut off*',
    '',
    'Canned text. No reviewer read the claim; this output only exercises the relay and the parsing.',
    '',
    `VERDICT: ${complete ? 'INCONCLUSIVE' : 'CONFIRMED'}`,
    '',
    '---',
    '',
    `<!-- consult-result v1 ${JSON.stringify(record)} -->`,
  ].join('\n')
}

function dryRunPrompt(canned) {
  return 'Dry run. Do not load or call any tool.\n' +
    'Reply with exactly the text between <output> and </output> below, character for character, without the tags, ' +
    'and nothing else: no preamble, no code fence, no comment.\n\n<output>\n' + canned + '\n</output>'
}

function wellFormed(r) {
  return !!r && typeof r === 'object' && !Array.isArray(r) &&
    typeof r.status === 'string' && typeof r.complete === 'boolean' && typeof r.capped === 'boolean' &&
    typeof r.cost_usd === 'number' && Number.isFinite(r.cost_usd) && r.cost_usd >= 0 &&
    (r.alias === null || typeof r.alias === 'string')
}

// A reviewer can quote the record format, so only the lines at the very end count.
// Every line with the prefix belongs to that block: one damaged line voids it
// rather than silently shortening it.
function trailingRecords(t) {
  const lines = t.replace(/\s+$/, '').split('\n')
  let start = lines.length
  while (start > 0 && lines[start - 1].trimEnd().startsWith(RECORD_PREFIX)) start--
  const block = lines.slice(start).map(l => l.trimEnd())
  const body = lines.slice(0, start).join('\n').trim()
  if (!block.length) return { records: null, body, reason: 'no status record at the end: an older install, or the relay changed the output' }
  const records = []
  for (const line of block) {
    const m = RECORD.exec(line)
    let rec = null
    if (m) {
      try { rec = JSON.parse(m[1]) } catch (e) { rec = null }
    }
    if (!wellFormed(rec)) return { records: null, body, reason: 'malformed status record at the end' }
    records.push(rec)
  }
  return { records, body }
}

// The last VERDICT line decides. A malformed one is no signal rather than a reason
// to fall back to an earlier line the reviewer may have gone on to revise.
function lastVerdict(body) {
  const lines = body.split('\n')
  for (let i = lines.length - 1; i >= 0; i--) {
    const bare = lines[i].replace(/[*_`]/g, '').replace(/^[\s#>]+/, '').trim()
    if (!VERDICT_LEAD.test(bare)) continue
    const m = VERDICT.exec(bare)
    if (m) return { verdict: m[1].toLowerCase() }
    return { verdict: null, reason: `the last VERDICT line is not one of the three values: ${bare.slice(0, 120)}` }
  }
  return { verdict: null, reason: 'complete review without a VERDICT line' }
}

function noSignal(reason, output, seen) {
  return { signal: false, verdict: null, reason, status: null, complete: null, capped: null, cost_usd: null, ...(seen || {}), output: output || null }
}

function readConsult(text) {
  if (text === null || text === undefined) {
    return noSignal('relay died, was skipped or could not start; the consult may still have run and been billed')
  }
  if (typeof text !== 'string' || !text.trim()) return noSignal('relay returned no text')
  const t = text.replace(/\r\n?/g, '\n')
  const found = trailingRecords(t)
  if (!found.records) {
    const first = t.trim().split('\n')[0]
    return noSignal(first.startsWith('RELAY:') ? first.slice(0, 300) : found.reason, found.body || t.trim())
  }
  const cost = found.records.reduce((s, r) => s + r.cost_usd, 0)
  if (found.records.length !== 1) {
    return noSignal(`expected one status record, found ${found.records.length}`, found.body, { cost_usd: round4(cost) })
  }
  const r = found.records[0]
  const seen = { status: r.status, complete: r.complete, capped: r.capped, cost_usd: r.cost_usd }
  if (r.status === 'failed') {
    return noSignal(`the consult failed before the reviewer ran: ${found.body.split('\n')[0].slice(0, 300)}`, found.body, seen)
  }
  if (r.complete !== true) return noSignal(`review not complete (status ${r.status})`, found.body, seen)
  // The relays only ever call the grounded consult, so a reviewer that made no
  // tool call never looked at the code the claim is about.
  if (r.tool_calls === 0) {
    return noSignal('the reviewer did not open a single file: opinion on the brief only, not a check of the code', found.body, seen)
  }
  const v = lastVerdict(found.body)
  if (!v.verdict) return noSignal(v.reason, found.body, seen)
  return { signal: true, verdict: v.verdict, reason: null, ...seen, output: found.body }
}

function readLocal(r) {
  if (!r || typeof r !== 'object') return { signal: false, verdict: null, reason: 'local check died or was skipped' }
  if (r.ran_code !== true && r.verdict !== 'inconclusive') {
    return { signal: true, verdict: 'inconclusive', ran_code: false, reported: r.verdict, evidence: r.evidence, reason: `reported ${r.verdict} without running code, counted as inconclusive` }
  }
  return { signal: true, verdict: r.verdict, ran_code: r.ran_code === true, evidence: r.evidence, reason: null }
}

function agreement(verdicts, expected) {
  const n = v => verdicts.filter(x => x === v).length
  const tally = { confirmed: n('confirmed'), refuted: n('refuted'), inconclusive: n('inconclusive'), no_signal: expected - verdicts.length }
  if (tally.confirmed && tally.refuted) return { agreement: 'disagree', verdict: null, tally }
  const decided = tally.confirmed ? 'confirmed' : tally.refuted ? 'refuted' : null
  if (!decided) return { agreement: verdicts.length ? 'inconclusive' : 'no signal', verdict: null, tally }
  return { agreement: tally[decided] === expected ? 'agree' : 'partial', verdict: decided, tally }
}

// ---------------------------------------------------------------------------

const A = readArgs(args)
if (A.errors.length) return { status: 'bad_args', errors: A.errors, usage: USAGE }
const MODEL = A.model ? { model: A.model } : {}

let claims = A.claims
if (claims) {
  if (A.scope) log('claims were given, so scope is not used')
} else {
  phase('Find')
  const found = await settle(() => agent(finderPrompt(A.root, A.scope), { label: 'find', phase: 'Find', schema: FOUND, ...MODEL }))
  const stop = reason => ({
    status: 'no_signal', stage: 'find', scope: A.scope, reason,
    note: 'Nothing was verified. This is not a clean result: the finder did not report finishing its scope.',
  })
  if (!found) return stop('the finder agent died or was skipped')
  if (found.completed !== true) return stop(`the finder did not finish: ${optText(found.incomplete_reason) || 'no reason given'}`)
  const list = Array.isArray(found.claims) ? found.claims : []
  if (list.length > MAX_FOUND) log(`the finder returned ${list.length} claims; only the first ${MAX_FOUND} are verified`)
  claims = tidyFound(list.slice(0, MAX_FOUND))
  if (!claims.length) {
    return {
      status: 'no_claims', scope: A.scope,
      note: 'The finder reports it examined the whole scope and proposed no claims. No local check or outside reviewer saw ' +
        'the scope: this is one Anthropic agent\'s reading, not a verified clean result.',
    }
  }
}

phase('Verify')
const consults = claims.length * A.voices.length
log(`${claims.length} claim(s) x ${A.voices.length} voice(s) = ${consults} outside consult(s)` +
  (A.dryRun ? ' (dry run: canned output, nothing sent to OpenRouter)' : '') +
  `; local check ${A.localCheck ? 'on' : 'off'}`)
log('voices: ' + A.voices.map((v, i) => `${v}#${i + 1} (${LENSES[i % LENSES.length].name})`).join(', '))
if (A.voices.length > LENSES.length) log(`there are ${LENSES.length} briefs, so voices from #${LENSES.length + 1} on reuse one`)

async function verify(c, ci) {
  const local = A.localCheck
    ? settle(() => agent(localPrompt(A.root, c), { label: `${c.id}:local`, phase: 'Verify', schema: LOCAL, ...MODEL }))
    : Promise.resolve(null)
  const relays = A.voices.map((voice, vi) => {
    const lens = LENSES[vi % LENSES.length]
    const toolArgs = { question: question(c, lens), models: [voice], mode: 'review', root: A.root }
    const prompt = A.dryRun ? dryRunPrompt(cannedOutput(voice, (ci + vi) % 2 === 0)) : relayPrompt(toolArgs)
    return settle(() => agent(prompt, { label: `${c.id}:${voice}#${vi + 1}`, phase: 'Verify', model: 'haiku' }))
      .then(text => ({ voice, slot: vi + 1, lens: lens.name, ...readConsult(text), ...(A.dryRun ? { would_send: toolArgs } : {}) }))
  })
  const [l, ...voices] = await Promise.all([local, ...relays])
  const loc = A.localCheck ? readLocal(l) : null
  const verdicts = [...(loc && loc.signal ? [loc.verdict] : []), ...voices.filter(v => v.signal).map(v => v.verdict)]
  const expected = A.voices.length + (A.localCheck ? 1 : 0)
  const agreed = agreement(verdicts, expected)
  log(`${c.id}: ${agreed.agreement}${agreed.verdict ? ' ' + agreed.verdict : ''} (${verdicts.length}/${expected} with a signal)`)
  return { id: c.id, claim: c.claim, where: c.where, ...agreed, local: loc || 'off', voices }
}

const settled = await parallel(claims.map((c, ci) => () => verify(c, ci)))
const rows = settled.map((r, i) => r || {
  id: claims[i].id, claim: claims[i].claim, where: claims[i].where,
  ...agreement([], A.voices.length + (A.localCheck ? 1 : 0)),
  error: 'verifying this claim failed in the script; none of its checks were read',
  local: A.localCheck ? { signal: false, verdict: null, reason: 'not read' } : 'off',
  voices: A.voices.map((voice, vi) => ({ voice, slot: vi + 1, lens: LENSES[vi % LENSES.length].name, ...noSignal('not read') })),
})

const cells = rows.flatMap(r => r.voices)
const expectedChecks = rows.length * (A.voices.length + (A.localCheck ? 1 : 0))
const checksWithSignal = rows.reduce((s, r) => s + (r.local !== 'off' && r.local.signal ? 1 : 0) + r.voices.filter(v => v.signal).length, 0)
const coverage = {
  checks_expected: expectedChecks,
  checks_with_signal: checksWithSignal,
  local: A.localCheck ? { with_signal: rows.filter(r => r.local.signal).length, of: rows.length } : 'off',
  voices: A.voices.map((voice, vi) => ({
    voice, slot: vi + 1, lens: LENSES[vi % LENSES.length].name,
    with_signal: rows.filter(r => r.voices[vi].signal).length, of: rows.length,
    no_signal: rows.filter(r => !r.voices[vi].signal).map(r => ({ claim: r.id, reason: r.voices[vi].reason })),
  })),
}
const cost = cells.reduce((s, v) => s + (typeof v.cost_usd === 'number' ? v.cost_usd : 0), 0)
const costUnknown = cells.filter(v => typeof v.cost_usd !== 'number').length
const status = checksWithSignal === expectedChecks ? 'checked' : checksWithSignal ? 'partial' : 'no_signal'
log(`coverage: ${checksWithSignal}/${expectedChecks} checks gave a signal; outside cost ${round4(cost)} USD` +
  (costUnknown ? `, plus ${costUnknown} call(s) whose cost could not be read` : ''))

return {
  status,
  dry_run: A.dryRun,
  root: A.root,
  claims_from: A.claims ? 'args' : 'finder',
  model: A.model || '(session model)',
  voices: A.voices,
  outside_cost_usd: round4(cost),
  outside_calls_cost_unknown: costUnknown,
  coverage,
  rows,
  note: 'An outside verdict counts only from a complete review ending in a VERDICT line, and a local one only from a check ' +
    'that ran code; everything else is listed as no signal, never as agreement. The reviews in rows[].voices[].output are ' +
    'the evidence; the tally only counts them.' +
    (A.dryRun ? ' Dry run: the relays returned canned output and nothing was sent to OpenRouter, so no outside verdict here is real.' : ''),
}
