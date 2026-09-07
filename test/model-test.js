// Run with: bash test/model-test.sh
const M = require("../Model.js")

let passed = 0
const failures = []

function check(name, fn) {
  try { fn(); passed++ } catch (e) { failures.push(name + "\n    " + e.message) }
}

function eq(actual, expected, what) {
  const a = JSON.stringify(actual)
  const b = JSON.stringify(expected)
  if (a !== b) throw new Error((what || "value") + ": expected " + b + ", got " + a)
}

function ok(cond, what) {
  if (!cond) throw new Error(what || "expected true")
}

const REPLY = '{"effect":"blur","blur":12,"device":"NPU","input":"/dev/video0",' +
              '"output":"/dev/video10","width":1280,"height":720,"background":false}'

// ---- Commands. Nothing is ever spliced into a command line.

check("the binary is named absolutely, never found on PATH", () => {
  ok(M.BINARY.startsWith("/"), "BINARY should be an absolute path")
  eq(M.statusCommand(), [M.BINARY, "status"])
})

check("arguments stay separate argv entries", () => {
  eq(M.effectCommand("replace"), [M.BINARY, "effect", "replace"])
  eq(M.blurCommand(40), [M.BINARY, "blur", "40"])
})

check("an effect the daemon does not know is refused here, not sent", () => {
  eq(M.effectCommand("banana"), null)
  eq(M.effectCommand("blur; rm -rf ~"), null)
})

check("blur is clamped before it is sent", () => {
  eq(M.blurCommand(9999), [M.BINARY, "blur", "200"])
  eq(M.blurCommand(-5), [M.BINARY, "blur", "0"])
  eq(M.blurCommand("nonsense"), [M.BINARY, "blur", "12"])
})

// ---- Starting and stopping the daemon. This, not `effect none`, is what
//      releases the camera.

check("the unit is started and stopped by absolute path", () => {
  ok(M.SYSTEMCTL.startsWith("/"), "systemctl should be an absolute path")
  eq(M.startCommand(), [M.SYSTEMCTL, "--user", "start", M.UNIT])
  eq(M.stopCommand(), [M.SYSTEMCTL, "--user", "stop", M.UNIT])
})

check("it is a user unit, never a system one", () => {
  ok(M.startCommand().indexOf("--user") !== -1, "start should be --user")
  ok(M.stopCommand().indexOf("--user") !== -1, "stop should be --user")
})

check("off says the camera is released, not merely that effects are off", () => {
  eq(M.powerLabel(M.notRunningState()), "Off")
  eq(M.powerLabel({ running: true }), "On")
  ok(M.powerHint(M.notRunningState()).indexOf("released") !== -1)
  ok(M.powerHint({ running: true }).indexOf("open") !== -1)
})

// ---- Voice focus. A separate unit, asked of systemd rather than the daemon.

check("voice focus is its own unit, started by absolute path", () => {
  ok(M.VOICE_UNIT !== M.UNIT, "voice must not be the camera unit")
  eq(M.voiceCommand(true), [M.SYSTEMCTL, "--user", "start", M.VOICE_UNIT])
  eq(M.voiceCommand(false), [M.SYSTEMCTL, "--user", "stop", M.VOICE_UNIT])
  eq(M.voiceStatusCommand()[0], M.SYSTEMCTL)
  ok(M.voiceStatusCommand().indexOf(M.VOICE_UNIT) !== -1, "must name the voice unit")
})

check("systemd's two lines are reduced to on, off or missing", () => {
  eq(M.parseVoiceState("loaded\nactive\n"), "on")
  eq(M.parseVoiceState("loaded\nactivating\n"), "on")
  eq(M.parseVoiceState("loaded\ninactive\n"), "off")
  eq(M.parseVoiceState("loaded\nfailed\n"), "off")
})

check("a unit that does not exist is missing, not merely off", () => {
  // This is why LoadState is asked for at all. `is-active` prints "inactive"
  // for a unit that does not exist -- the same word as for one that is simply
  // stopped -- so a widget reading that alone offers a switch for a package
  // that was never installed.
  eq(M.parseVoiceState("not-found\ninactive\n"), "missing")
  eq(M.parseVoiceState(""), "missing", "no reply at all")
  eq(M.parseVoiceState("loaded"), "missing", "a truncated reply is not trusted")
})

check("the status command asks for the load state, not just is-active", () => {
  const cmd = M.voiceStatusCommand()
  ok(cmd.indexOf("LoadState") !== -1, "must ask LoadState: " + cmd.join(" "))
  ok(cmd.indexOf("is-active") === -1, "is-active cannot answer this")
})

// ---- Parsing. Every reply carries the whole state, so this is the only
//      place a reply is ever interpreted.

check("a normal reply is read", () => {
  const s = M.parseStatus(REPLY)
  ok(s.ok && s.running, "should be running")
  eq(s.effect, "blur", "effect")
  eq(s.blur, 12, "blur")
  eq(s.device, "NPU", "device")
  eq(s.error, "", "error")
})

check("a refusal is carried through, not dropped", () => {
  const s = M.parseStatus('{"effect":"blur","blur":12,"error":"no background image was loaded"}')
  ok(s.ok, "still a valid reply")
  ok(s.error.length > 0, "the reason should survive")
  eq(s.effect, "blur", "and the effect is what the daemon says, not what was asked for")
})

check("garbage never reads as effects being on", () => {
  for (const bad of ["", "not json", "[]", "null", '{"effect":"banana"}', "x".repeat(9000)]) {
    const s = M.parseStatus(bad)
    eq(s.effect, "none", "effect for " + JSON.stringify(bad.slice(0, 20)))
  }
})

check("a missing daemon is not running, and shows no effect", () => {
  const s = M.notRunningState()
  ok(!s.running, "should not be running")
  eq(s.effect, "none", "effect")
})

// ---- The live preview.

check("preview is asked for explicitly in both directions", () => {
  eq(M.previewCommand(true), [M.BINARY, "preview", "on"])
  eq(M.previewCommand(false), [M.BINARY, "preview", "off"])
})

check("the preview path comes from the daemon, never rebuilt here", () => {
  const s = M.parseStatus('{"effect":"blur","preview":true,"previewPath":"/run/user/1000/x.jpg"}')
  eq(s.preview, true, "preview")
  eq(s.previewPath, "/run/user/1000/x.jpg", "previewPath")
})

check("a reply with no preview fields never claims a path", () => {
  const s = M.parseStatus(REPLY)
  eq(s.preview, false, "preview")
  eq(s.previewPath, "", "previewPath")
  eq(M.notRunningState().previewPath, "", "previewPath when stopped")
})

// ---- Adjustable settings.

check("only settings the current effect uses are offered", () => {
  // Built through parseStatus rather than by hand: panelRows only offers what
  // the daemon reported, so a hand-made state would silently offer nothing.
  const all = '"blur":12,"passes":2,"dim":0,"desat":0'
  const kinds = json => M.panelRows(M.parseStatus(json)).map(r => r.kind + ":" + (r.effect || r.key))

  // Nothing to adjust when effects are off.
  eq(kinds('{"effect":"none",' + all + '}'), ["effect:none", "effect:blur"])

  // Replace has no blur to soften, so no blur or smoothness rows.
  const rep = kinds('{"effect":"replace","background":true,' + all + '}')
  ok(rep.indexOf("param:blur") === -1, "replace should not offer blur: " + rep)
  ok(rep.indexOf("param:dim") !== -1, "replace should offer darken: " + rep)

  // Blur offers all four.
  const bl = kinds('{"effect":"blur",' + all + '}')
  for (const k of ["param:blur", "param:passes", "param:dim", "param:desat"])
    ok(bl.indexOf(k) !== -1, k + " missing from " + bl)
})

check("every setting is clamped to the daemon's range before being sent", () => {
  eq(M.paramCommand("passes", 9), [M.BINARY, "passes", "3"])
  eq(M.paramCommand("passes", 0), [M.BINARY, "passes", "1"])
  eq(M.paramCommand("dim", 500), [M.BINARY, "dim", "100"])
  eq(M.paramCommand("desat", -20), [M.BINARY, "desat", "0"])
  eq(M.paramCommand("blur", 9999), [M.BINARY, "blur", "200"])
})

check("an unknown setting is refused here, not sent", () => {
  eq(M.paramCommand("banana", 1), null)
  eq(M.paramCommand("effect", "none"), null)
})

check("stepping stays inside the range and moves by the right amount", () => {
  eq(M.stepParam({ passes: 2 }, "passes", 1), 3)
  eq(M.stepParam({ passes: 3 }, "passes", 1), 3, "clamped at the top")
  eq(M.stepParam({ dim: 0 }, "dim", -1), 0, "clamped at the bottom")
  eq(M.stepParam({ dim: 50 }, "dim", 1), 60)
})

check("a daemon that does not report a setting is not offered rows for it", () => {
  // An older daemon: knows blur, has never heard of passes, dim or desat.
  const old = M.parseStatus('{"effect":"blur","blur":12}')
  const keys = M.panelRows(old).filter(r => r.kind === "param").map(r => r.key)
  eq(keys, ["blur"], "only settings the daemon reports should appear")

  // A current one offers all four.
  const now = M.parseStatus('{"effect":"blur","blur":12,"passes":2,"dim":0,"desat":0}')
  eq(M.panelRows(now).filter(r => r.kind === "param").map(r => r.key),
     ["blur", "passes", "dim", "desat"])
})

check("a stopped daemon offers no settings at all", () => {
  eq(M.panelRows(M.notRunningState()).filter(r => r.kind === "param").length, 0)
})

check("the daemon's values are read back, not defaulted", () => {
  const s = M.parseStatus('{"effect":"blur","blur":30,"passes":3,"dim":45,"desat":80}')
  eq(M.paramValue(s, "blur"), 30, "blur")
  eq(M.paramValue(s, "passes"), 3, "passes")
  eq(M.paramValue(s, "dim"), 45, "dim")
  eq(M.paramValue(s, "desat"), 80, "desat")
})

check("a reported setting is marked supported, an absent one is not", () => {
  const s = M.parseStatus('{"effect":"blur","blur":12,"passes":2}')
  eq(s.supports.blur, true, "blur")
  eq(s.supports.passes, true, "passes")
  eq(s.supports.dim, false, "dim absent means unsupported")
})

check("every adjustable setting survives a round trip through parseStatus", () => {
  // The panel reads each PARAMS entry off the parsed state, so a setting the
  // daemon reports and the parser drops shows as a slider stuck at its minimum.
  const json = '{"effect":"blur","blur":66,"passes":3,"dim":10,"desat":20}'
  const s = M.parseStatus(json)
  for (const p of M.PARAMS)
    ok(typeof s[p.key] === "number", p.key + " missing from parsed state")
})

check("a reply missing a setting reads as its minimum, never as garbage", () => {
  const s = M.parseStatus('{"effect":"blur"}')
  eq(M.paramValue(s, "passes"), 1, "passes")
  eq(M.paramValue(s, "dim"), 0, "dim")
})

check("the cursor opens on the effect that is on, counting param rows", () => {
  const st = M.parseStatus('{"effect":"blur","blur":12,"passes":2,"dim":0,"desat":0}')
  const rows = M.panelRows(st)
  eq(rows[M.indexOfEffect(st, rows)].effect, "blur")
})

// ---- Auto framing.

check("framing is offered with the effects, not under one of them", () => {
  // Worth having with no background effect at all.
  const off = M.parseStatus('{"effect":"none","blur":12,"framing":false}')
  const keys = M.panelRows(off).map(r => r.kind + ":" + (r.effect || r.key))
  ok(keys.indexOf("toggle:framing") !== -1, "framing missing with effects off: " + keys)
})

check("a daemon that does not report framing is not offered it", () => {
  const older = M.parseStatus('{"effect":"blur","blur":12}')
  eq(M.panelRows(older).filter(r => r.kind === "toggle").length, 0)
})

check("framing is sent as on or off, never as a number", () => {
  eq(M.toggleCommand("framing", true), [M.BINARY, "framing", "on"])
  eq(M.toggleCommand("framing", false), [M.BINARY, "framing", "off"])
  eq(M.toggleCommand("banana", true), null)
})

check("framing state is read back from the daemon", () => {
  eq(M.toggleValue(M.parseStatus('{"effect":"blur","framing":true}'), "framing"), true)
  eq(M.toggleValue(M.parseStatus('{"effect":"blur","framing":false}'), "framing"), false)
  eq(M.toggleValue(M.notRunningState(), "framing"), false, "a stopped daemon frames nothing")
})

// ---- What the bar shows.

check("the glyph distinguishes off, blurred and replaced", () => {
  const g = e => M.glyphFor({ running: true, effect: e })
  ok(g("blur") !== g("replace"), "blur and replace should differ")
  ok(g("none") !== g("blur"), "off and blurred should differ")
  eq(M.glyphFor(M.notRunningState()), g("none"), "a stopped daemon reads as off")
})

check("a stopped daemon says so rather than claiming effects are off", () => {
  ok(M.tooltipFor(M.notRunningState()).indexOf("not running") !== -1)
  ok(M.labelFor(M.parseStatus(REPLY)).indexOf("blur") !== -1)
})

// ---- Choices offered.

check("replace is offered only when an image is actually loaded", () => {
  eq(M.availableEffects({ background: false }), ["none", "blur"])
  eq(M.availableEffects({ background: true }), ["none", "blur", "replace"])
  eq(M.availableEffects(null), ["none", "blur"])
})

check("the cursor lands on the current effect, and survives one that is hidden", () => {
  const st = M.parseStatus('{"effect":"blur","blur":12,"passes":2,"dim":0,"desat":0}')
  const rows = M.panelRows(st)
  eq(rows[M.indexOfEffect(st, rows)].effect, "blur")
  // An effect not on offer (replace with no image) must not point off the list.
  const hidden = M.panelRows(M.parseStatus('{"effect":"replace","blur":12}'))
  ok(M.indexOfEffect({ effect: "replace" }, hidden) < hidden.length, "index must stay in range")
})

check("stepping blur stays inside the daemon's range", () => {
  eq(M.stepBlur({ blur: 12 }, 1), 12 + M.BLUR_STEP)
  eq(M.stepBlur({ blur: M.BLUR_MAX }, 1), M.BLUR_MAX)
  eq(M.stepBlur({ blur: 0 }, -1), 0)
})

if (failures.length) {
  console.error(failures.length + " failed, " + passed + " passed\n")
  for (const f of failures) console.error("  " + f)
  process.exit(1)
}
console.log(passed + " passed")
