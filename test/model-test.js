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
  const list = M.availableEffects({ background: false })
  eq(M.indexOfEffect({ effect: "blur" }, list), 1)
  eq(M.indexOfEffect({ effect: "replace" }, list), 0, "hidden effect falls back to the first row")
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
