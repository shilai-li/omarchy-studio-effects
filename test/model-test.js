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

check("the resolution reaches the panel, which is the only place it shows", () => {
  const s = M.parseStatus(REPLY)
  eq(s.width, 1280, "width")
  eq(s.height, 720, "height")
  // A reply without them must not put "undefined" on screen.
  const bare = M.parseStatus('{"effect":"blur"}')
  eq(bare.width, 0, "width")
  eq(M.notRunningState().height, 0, "height when stopped")
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

check("zoom is offered only while framing is on", () => {
  const all = '"blur":12,"passes":2,"dim":0,"desat":0,"zoom":200'
  const keys = json => M.panelRows(M.parseStatus(json)).filter(r => r.kind === "param").map(r => r.key)

  // Framing off: a zoom slider would adjust something with no visible effect.
  ok(keys('{"effect":"blur","framing":false,' + all + '}').indexOf("zoom") === -1,
     "zoom must be hidden while framing is off")
  ok(keys('{"effect":"blur","framing":true,' + all + '}').indexOf("zoom") !== -1,
     "zoom must appear once framing is on")
})

check("zoom is clamped to what the daemon accepts", () => {
  eq(M.paramCommand("zoom", 500), [M.BINARY, "zoom", "300"])
  eq(M.paramCommand("zoom", 10), [M.BINARY, "zoom", "100"])
  eq(M.stepParam({ zoom: 300 }, "zoom", 1), 300, "clamped at the top")
})

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

check("one glyph covers the whole plugin, camera and microphone alike", () => {
  // A camera icon described half of what this widget controls once it also
  // switched the microphone filter.
  const seen = new Set(["blur", "replace", "none"].map(e => M.glyphFor({ running: true, effect: e })))
  seen.add(M.glyphFor(M.notRunningState()))
  eq(seen.size, 1, "the glyph must not change with state")
  eq([...seen][0], M.GLYPH)
})

check("the tooltip carries the state the glyph no longer does", () => {
  // Both halves, because one icon cannot say which of them is on.
  const on = M.tooltipFor(M.parseStatus(REPLY), "on")
  ok(on.indexOf("blur") !== -1, "should name the camera effect: " + on)
  ok(on.indexOf("voice focus on") !== -1, "should name voice focus: " + on)

  const camOff = M.tooltipFor(M.notRunningState(), "on")
  ok(camOff.indexOf("camera effects off") !== -1, camOff)
  ok(camOff.indexOf("voice focus on") !== -1, "a filter running with the camera off must show: " + camOff)

  // A missing voice unit is not mentioned at all rather than called "off".
  ok(M.tooltipFor(M.parseStatus(REPLY), "missing").indexOf("voice focus") === -1)
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

// ---- The model row.

const TWO = '{"effect":"blur","blur":12,"passes":1,"dim":0,"desat":0,' +
            '"model":"matting","models":["matting","segmentation"]}'

check("the model and the installed list are read out of the reply", () => {
  const st = M.parseStatus(TWO)
  eq(st.model, "matting")
  eq(st.models, ["matting", "segmentation"])
})

check("the model row appears only when there is something to switch to", () => {
  const kinds = (json) => M.panelRows(M.parseStatus(json))
      .filter((r) => r.kind === "choice").map((r) => r.key)
  eq(kinds(TWO), ["model"])
  // One installed model is not a choice, and a row that cycles back to what is
  // already on is a control that does nothing.
  eq(kinds('{"effect":"blur","model":"matting","models":["matting"]}'), [])
  // A daemon too old to report either says nothing, and gets no row.
  eq(kinds('{"effect":"blur","blur":12}'), [])
})

check("stepping the model wraps and never leaves the installed list", () => {
  const st = M.parseStatus(TWO)
  eq(M.stepChoice(st, "model", 1), "segmentation")
  eq(M.stepChoice(st, "model", -1), "segmentation")
  eq(M.stepChoice({ model: "segmentation", models: ["matting", "segmentation"] }, "model", 1),
     "matting")
  // A daemon running a model it did not list -- installed from elsewhere, or
  // renamed underneath us -- still steps somewhere real rather than nowhere.
  eq(M.stepChoice({ model: "gone", models: ["matting", "segmentation"] }, "model", 1), "matting")
  eq(M.stepChoice({ model: "matting", models: [] }, "model", 1), "")
})

check("the model command names the model, and refuses to name nothing", () => {
  eq(M.choiceCommand("model", "segmentation"), [M.BINARY, "model", "segmentation"])
  eq(M.choiceCommand("model", ""), null)
  eq(M.choiceCommand("nonsense", "segmentation"), null)
})

// ---- Whether the daemon exists, and how setup is started.

check("installed is asked of the filesystem by absolute path", () => {
  ok(M.DAEMON.startsWith("/") && M.TEST.startsWith("/"), "both named absolutely")
  eq(M.installedCommand(), [M.TEST, "-x", M.DAEMON])
})

check("only a clean yes or no decides installed; anything else is unknown", () => {
  eq(M.parseInstalled(0), "yes")
  eq(M.parseInstalled(1), "no")
  // Could not run, or was killed: that says nothing about the daemon, and
  // "not installed" would offer to reinstall something that is there.
  for (const code of [-1, 2, 126, 127, 137, null, undefined]) eq(M.parseInstalled(code), "unknown")
})

check("setup is started through a fixed shell with the path as an argument", () => {
  const script = "/home/x/.config/omarchy/plugins/p/packaging/setup.sh"
  const argv = M.setupLaunch(script)
  eq(argv[0], "/usr/bin/bash")
  eq(argv[1], "-c")
  eq(argv[argv.length - 1], script)
  // The path is data. It must not appear in the script text, or a path with a
  // quote or a space in it becomes code.
  ok(argv[2].indexOf(script) === -1, "the script text must not contain the path")
  ok(argv[2].indexOf("$1") !== -1, "and reads it as $1")
})

check("a hostile path is passed through untouched, not interpreted", () => {
  const nasty = "/tmp/a b/$(touch pwned)/`id`/x'y\"z/setup.sh"
  const argv = M.setupLaunch(nasty)
  eq(argv[argv.length - 1], nasty)
  ok(argv[2].indexOf("touch pwned") === -1 && argv[2].indexOf("`id`") === -1, "nothing spliced in")
})

check("setup refuses a path that is empty, relative or not a string", () => {
  for (const bad of ["", "setup.sh", "./setup.sh", "../setup.sh", null, undefined, 42, {}])
    eq(M.setupLaunch(bad), null)
})

check("the launchers are found on a fixed PATH, not the inherited one", () => {
  const s = M.LAUNCH_SCRIPT
  ok(s.indexOf("PATH=/usr/share/omarchy/bin:/usr/bin:/bin") !== -1, "trusted() sets PATH itself")
  ok(s.indexOf("omarchy-launch-floating-terminal-with-presentation") !== -1, "prefers Omarchy's own")
  ok(s.indexOf("xdg-terminal-exec") !== -1, "falls back to the default terminal")
  // The presenter joins its arguments into a string a shell parses: it must be
  // handed one already quoted, or a path with a space runs as two commands.
  ok(s.indexOf("%q") !== -1, "the command handed to the presenter is quoted")
})

check("the tooltip says so when the daemon is not installed", () => {
  ok(M.tooltipFor(M.notRunningState(), "off", "no").indexOf("not installed") !== -1)
  // Installed or not yet known: the ordinary wording, so a slow check does not
  // flash "not installed" at somebody who has it.
  for (const i of ["yes", "unknown", undefined])
    ok(M.tooltipFor(M.notRunningState(), "off", i).indexOf("not installed") === -1, String(i))
})

if (failures.length) {
  console.error(failures.length + " failed, " + passed + " passed\n")
  for (const f of failures) console.error("  " + f)
  process.exit(1)
}
console.log(passed + " passed")
