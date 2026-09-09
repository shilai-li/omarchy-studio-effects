// Pure logic for the Studio Effects widget: what to run, and what to make of
// what comes back. Deliberately Qt- and locale-free so test/model-test.sh can
// run it under plain node with no compositor.

// The daemon's client, by absolute path. A plugin runs inside the shell
// process with the session's whole environment, so the one thing started from
// here is named outright rather than found on an inherited PATH.
var BINARY = "/usr/bin/studio-effects";

// Starting and stopping the daemon is the real on/off switch, not `effect
// none`. While the daemon runs it holds the camera open: the recording light
// stays lit, no other application can use the real camera, and frames are
// segmented and composited whether or not anything is watching. `effect none`
// only stops the compositing.
var SYSTEMCTL = "/usr/bin/systemctl";
var UNIT = "studio-effects.service";

// Voice focus is a second, independent unit. Denoising a call you are on with
// the camera off is a normal thing to want, and the two fail independently, so
// a camera that will not start must not take the microphone filter with it.
// That means its state is asked of systemd rather than of the camera daemon,
// which knows nothing about audio.
var VOICE_UNIT = "studio-effects-voice.service";

var EFFECTS = ["none", "blur", "replace"];
var BLUR_MIN = 0;
var BLUR_MAX = 200;
var BLUR_STEP = 6;

// The client talks to a unix socket on the same machine: it answers at once or
// not at all. This is only the backstop for it wedging.
var TIMEOUT_SECONDS = 3;

// A reply is one line of JSON. Anything longer is not the daemon we know, and
// is refused rather than parsed.
var MAX_REPLY_BYTES = 8192;

function command(args) {
    // Arguments are passed as argv entries, never spliced into a command line,
    // and there is no shell in the chain at all.
    return [BINARY].concat(args || []);
}

function statusCommand() { return command(["status"]); }

function startCommand() { return [SYSTEMCTL, "--user", "start", UNIT]; }
function stopCommand() { return [SYSTEMCTL, "--user", "stop", UNIT]; }

function voiceCommand(on) {
    return [SYSTEMCTL, "--user", on ? "start" : "stop", VOICE_UNIT];
}

// Asks for LoadState and ActiveState rather than using `is-active`, which
// cannot answer the question. `is-active` prints "inactive" for a unit that
// does not exist at all, the same word it prints for one that is merely
// stopped, and only the exit code differs -- 4 against 3. Reading a state
// machine off an exit code is how "not installed" ends up displayed as a switch
// the user can flip. `show` says "not-found" outright.
function voiceStatusCommand() {
    return [SYSTEMCTL, "--user", "show", VOICE_UNIT,
            "-p", "LoadState", "-p", "ActiveState", "--value"];
}

// Two lines, LoadState then ActiveState, reduced to the three states the panel
// can say something useful about.
function parseVoiceState(text) {
    var lines = String(text === undefined || text === null ? "" : text)
        .split("\n")
        .map(function (l) { return l.trim(); })
        .filter(function (l) { return l.length > 0; });

    if (lines.length < 2) return "missing";
    if (lines[0] !== "loaded") return "missing";
    if (lines[1] === "active" || lines[1] === "activating") return "on";
    return "off";
}

// The daemon needs a moment to open the camera after systemd reports the unit
// started, so the widget re-asks rather than concluding from one silent reply
// that starting failed.
var SETTLE_ATTEMPTS = 10;
var SETTLE_INTERVAL_MS = 400;

function effectCommand(effect) {
    return isEffect(effect) ? command(["effect", effect]) : null;
}

// Preview frames are only published while something is looking at them, so the
// panel turns this on when it opens and off when it closes.
function previewCommand(on) {
    return command(["preview", on ? "on" : "off"]);
}

// How often the panel re-reads the preview file. The daemon publishes about ten
// a second; asking much faster only re-decodes the same JPEG.
var PREVIEW_INTERVAL_MS = 100;

function blurCommand(radius) {
    return command(["blur", String(clampBlur(radius))]);
}

// Every adjustable setting, in one place, so the panel does not have to know
// the range of anything and the daemon is never sent a value it will refuse.
// Ranges here mirror the daemon's; it clamps too, and disagreeing shows up as a
// refusal rather than as a wrong picture.
// `needs` names a toggle that must be on for the setting to be worth showing.
// A zoom slider while framing is off adjusts something with no visible effect,
// which is the same trap as offering a blur radius to a replaced background.
var PARAMS = [
    { key: "zoom",   label: "Zoom in",    min: 100, max: 300, step: 20,
      effects: ["none", "blur", "replace"], needs: "framing" },
    { key: "blur",   label: "Blur",       min: 0, max: 200, step: 6,  effects: ["blur"] },
    { key: "passes", label: "Smoothness", min: 1, max: 3,   step: 1,  effects: ["blur"] },
    { key: "dim",    label: "Darken",     min: 0, max: 100, step: 10, effects: ["blur", "replace"] },
    { key: "desat",  label: "Desaturate", min: 0, max: 100, step: 10, effects: ["blur", "replace"] }
];

// Settings whose value is one of a list the daemon supplies, rather than a
// number or a flag. `values` names the field carrying the list: which models
// are installed is discovered on the daemon's machine, so the widget cannot
// hold that list itself and must be told.
//
// One row, stepped with the arrow keys, not one row per value. Two models is
// two extra rows in a panel that already runs to a dozen, and the list only
// gets longer as models are added.
var CHOICES = [
    { key: "model", label: "Model", values: "models" }
];

function choiceFor(key) {
    for (var i = 0; i < CHOICES.length; i++)
        if (CHOICES[i].key === key) return CHOICES[i];
    return null;
}

// What the daemon says is installed. Never a fallback list: offering a model
// this daemon does not have is a row that can only produce a refusal.
function choiceValues(state, key) {
    var spec = choiceFor(key);
    if (!spec || !state) return [];
    var list = state[spec.values];
    return Array.isArray(list) ? list : [];
}

function choiceValue(state, key) {
    return state && typeof state[key] === "string" ? state[key] : "";
}

// Wraps, because a list of two read as a pair of ends would need four presses
// to get back where it started.
function stepChoice(state, key, direction) {
    var values = choiceValues(state, key);
    if (values.length === 0) return "";
    var at = values.indexOf(choiceValue(state, key));
    var next = (at < 0 ? 0 : at + direction) % values.length;
    return values[next < 0 ? next + values.length : next];
}

function choiceCommand(key, value) {
    return choiceFor(key) && typeof value === "string" && value.length > 0
        ? command([key, value]) : null;
}

// A setting counts as supported when the daemon reports a value for it: a
// number for a slider, a boolean for a toggle, and for a choice both a current
// value and at least two to pick between -- a row that can only cycle back to
// what is already on is a control that does nothing.
function supportedParams(parsed) {
    var found = {};
    var ok = parsed !== null && typeof parsed === "object";
    for (var i = 0; i < PARAMS.length; i++)
        found[PARAMS[i].key] = ok && typeof parsed[PARAMS[i].key] === "number";
    for (var j = 0; j < TOGGLES.length; j++)
        found[TOGGLES[j].key] = ok && typeof parsed[TOGGLES[j].key] === "boolean";
    for (var k = 0; k < CHOICES.length; k++) {
        var spec = CHOICES[k];
        found[spec.key] = ok && typeof parsed[spec.key] === "string"
            && Array.isArray(parsed[spec.values]) && parsed[spec.values].length > 1;
    }
    return found;
}

// Settings that are on or off rather than a number. Kept apart from PARAMS
// because the panel treats them differently: enter flips a toggle, where left
// and right move a slider.
var TOGGLES = [
    { key: "framing", label: "Auto framing" }
];

function toggleFor(key) {
    for (var i = 0; i < TOGGLES.length; i++)
        if (TOGGLES[i].key === key) return TOGGLES[i];
    return null;
}

function toggleCommand(key, on) {
    return toggleFor(key) ? command([key, on ? "on" : "off"]) : null;
}

function toggleValue(state, key) {
    return !!(state && state[key] === true);
}

function paramFor(key) {
    for (var i = 0; i < PARAMS.length; i++)
        if (PARAMS[i].key === key) return PARAMS[i];
    return null;
}

function clampParam(key, value) {
    var p = paramFor(key);
    if (!p) return 0;
    var n = Math.round(Number(value));
    if (!isFinite(n)) return p.min;
    return Math.max(p.min, Math.min(p.max, n));
}

function paramCommand(key, value) {
    return paramFor(key) ? command([key, String(clampParam(key, value))]) : null;
}

function paramValue(state, key) {
    return state && typeof state[key] === "number" ? state[key] : clampParam(key, 0);
}

// The rows the panel shows: the effects, then the settings that apply to
// whichever effect is on. A slider for something the current effect ignores is
// worse than no slider -- it invites a change that does nothing visible.
function panelRows(state) {
    var rows = [];
    var effects = availableEffects(state);
    for (var i = 0; i < effects.length; i++)
        rows.push({ kind: "effect", effect: effects[i], key: "" });

    var supports = state && state.supports ? state.supports : {};

    // Framing is independent of the background effect -- keeping someone
    // centred is worth having with no blur at all -- so it sits with the
    // effects rather than under them.
    for (var t = 0; t < TOGGLES.length; t++) {
        if (!supports[TOGGLES[t].key]) continue;
        rows.push({ kind: "toggle", effect: "", key: TOGGLES[t].key });
    }

    // Which model is segmenting governs both the effects and the framing --
    // the mask feeds all of it -- so it sits with them rather than under any
    // one of them, and above the sliders because it is set once and left.
    for (var c = 0; c < CHOICES.length; c++) {
        if (!supports[CHOICES[c].key]) continue;
        rows.push({ kind: "choice", effect: "", key: CHOICES[c].key });
    }

    var current = state ? state.effect : "none";
    for (var j = 0; j < PARAMS.length; j++) {
        if (PARAMS[j].effects.indexOf(current) === -1) continue;
        if (!supports[PARAMS[j].key]) continue;
        if (PARAMS[j].needs && !toggleValue(state, PARAMS[j].needs)) continue;
        rows.push({ kind: "param", effect: "", key: PARAMS[j].key });
    }
    return rows;
}

function stepParam(state, key, direction) {
    var p = paramFor(key);
    if (!p) return 0;
    return clampParam(key, paramValue(state, key) + direction * p.step);
}

function isEffect(effect) {
    return EFFECTS.indexOf(effect) !== -1;
}

function clampBlur(radius) {
    var n = Math.round(Number(radius));
    if (!isFinite(n)) return 12;
    return Math.max(BLUR_MIN, Math.min(BLUR_MAX, n));
}

// The state the widget shows before it has heard anything, and after it has
// heard something it could not understand.
function unknownState(reason) {
    return {
        ok: false,
        running: false,
        effect: "none",
        blur: 12,
        device: "",
        input: "",
        output: "",
        background: false,
        width: 0,
        height: 0,
        zoom: 100,
        passes: 1,
        dim: 0,
        desat: 0,
        framing: false,
        model: "",
        models: [],
        supports: supportedParams(null),
        preview: false,
        previewPath: "",
        error: reason || ""
    };
}

// Every reply carries the daemon's whole state, which is why a command and a
// status request are parsed by the same function and why nothing here ever
// needs to ask twice.
function parseStatus(text) {
    var raw = String(text === undefined || text === null ? "" : text);
    if (raw.length === 0) return unknownState("");
    if (raw.length > MAX_REPLY_BYTES) return unknownState("reply too long");

    var parsed;
    try {
        parsed = JSON.parse(raw);
    } catch (e) {
        return unknownState("could not read the daemon's reply");
    }
    if (!parsed || typeof parsed !== "object") return unknownState("unexpected reply");

    var effect = isEffect(parsed.effect) ? parsed.effect : "none";
    return {
        ok: true,
        running: true,
        effect: effect,
        blur: clampBlur(parsed.blur),
        // Read by name from PARAMS so adding a setting in one place is enough;
        // forgetting this step showed every new slider sitting at its minimum
        // while the daemon was plainly using something else.
        zoom: clampParam("zoom", parsed.zoom),
        passes: clampParam("passes", parsed.passes),
        dim: clampParam("dim", parsed.dim),
        desat: clampParam("desat", parsed.desat),
        // Which settings this daemon actually reports. A widget can be newer
        // than the daemon it is talking to -- a plugin updates by pulling a
        // git checkout, the daemon by installing a package, and there is
        // nothing making those happen together. Offering a row the daemon has
        // never heard of gives the user a control that does nothing and says
        // nothing, which is worse than not offering it.
        supports: supportedParams(parsed),
        device: typeof parsed.device === "string" ? parsed.device : "",
        // Reported by the daemon and shown in the panel's footer, which is the
        // only place the resolution appears at all.
        width: typeof parsed.width === "number" ? parsed.width : 0,
        height: typeof parsed.height === "number" ? parsed.height : 0,
        input: typeof parsed.input === "string" ? parsed.input : "",
        output: typeof parsed.output === "string" ? parsed.output : "",
        background: parsed.background === true,
        preview: parsed.preview === true,
        framing: parsed.framing === true,
        // Which model is segmenting, and which the daemon found installed. The
        // list is the daemon's own directory scan, so a machine with one model
        // simply reports one and the row does not appear.
        model: typeof parsed.model === "string" ? parsed.model : "",
        models: Array.isArray(parsed.models)
            ? parsed.models.filter(function (m) { return typeof m === "string"; })
            : [],
        // Taken from the daemon rather than rebuilt here, so the two cannot
        // disagree about where the frames are.
        previewPath: typeof parsed.previewPath === "string" ? parsed.previewPath : "",
        // The daemon reports a refusal in the reply as well as in its exit
        // code. Carrying it through means the panel can say why a choice did
        // not take rather than just failing to change.
        error: typeof parsed.error === "string" ? parsed.error : ""
    };
}

// A daemon that is not running is not an error state to shout about — the user
// may simply not have started it — but it must never look like effects are on.
function notRunningState() {
    var state = unknownState("");
    state.running = false;
    return state;
}

// One glyph for the whole plugin, whatever it happens to be doing. It started
// as a camera icon, which stopped being honest once the same widget also
// switched a microphone filter: half of what it controls is not a camera.
// Sparkles say "effects" without claiming which device.
//
// State is not carried by the glyph. The bar already dims an inactive widget
// and accents an active one, and the tooltip says which effects are on -- a
// second encoding in the glyph would only disagree with those eventually.
var GLYPH = "\u{F0674}";

function glyphFor(state) {
    return GLYPH;
}

// The power row's label. Separate from labelFor because "off" here means the
// camera is released, which is a different claim from "effects are off".
function powerLabel(state) {
    return state && state.running ? "On" : "Off";
}

function powerHint(state) {
    return state && state.running
        ? "camera open"
        : "camera released";
}

function labelFor(state) {
    if (!state || !state.running) return "Not running";
    if (state.effect === "replace") return "Background replaced";
    if (state.effect === "blur") return "Background blurred";
    return "Effects off";
}

// The tooltip carries what the glyph no longer does, and it has to cover both
// halves: with a single icon, a user whose camera is off but whose microphone
// filter is on has no other way to tell why the widget looks active.
function tooltipFor(state, voice) {
    var parts = [];
    if (state && state.running) {
        var where = state.device ? " on " + state.device : "";
        parts.push(labelFor(state).toLowerCase() + where);
    } else {
        parts.push("camera effects off");
    }
    if (voice === "on") parts.push("voice focus on");
    else if (voice === "off") parts.push("voice focus off");
    return "Studio Effects — " + parts.join(", ");
}

// `replace` is offered only when the daemon actually loaded an image. Showing
// a choice the daemon will refuse is worse than not showing it.
function availableEffects(state) {
    var usable = [];
    for (var i = 0; i < EFFECTS.length; i++) {
        if (EFFECTS[i] === "replace" && !(state && state.background)) continue;
        usable.push(EFFECTS[i]);
    }
    return usable;
}

// Where the cursor should sit when the panel opens: on the effect that is on.
function indexOfEffect(state, rows) {
    var current = state ? state.effect : "none";
    for (var i = 0; i < rows.length; i++)
        if (rows[i].kind === "effect" && rows[i].effect === current) return i;
    return 0;
}

function stepBlur(state, direction) {
    var base = state ? state.blur : 12;
    return clampBlur(base + direction * BLUR_STEP);
}

// Exported for test/model-test.sh. QML imports this file directly and reads the
// same names off the module object, so nothing below changes how the widget
// sees it.
if (typeof module !== "undefined" && module.exports) {
    module.exports = {
        BINARY: BINARY,
        EFFECTS: EFFECTS,
        BLUR_MIN: BLUR_MIN,
        BLUR_MAX: BLUR_MAX,
        BLUR_STEP: BLUR_STEP,
        TIMEOUT_SECONDS: TIMEOUT_SECONDS,
        MAX_REPLY_BYTES: MAX_REPLY_BYTES,
        SYSTEMCTL: SYSTEMCTL,
        UNIT: UNIT,
        VOICE_UNIT: VOICE_UNIT,
        voiceCommand: voiceCommand,
        voiceStatusCommand: voiceStatusCommand,
        parseVoiceState: parseVoiceState,
        SETTLE_ATTEMPTS: SETTLE_ATTEMPTS,
        SETTLE_INTERVAL_MS: SETTLE_INTERVAL_MS,
        command: command,
        statusCommand: statusCommand,
        startCommand: startCommand,
        stopCommand: stopCommand,
        powerLabel: powerLabel,
        powerHint: powerHint,
        toggleCommand: toggleCommand,
        effectCommand: effectCommand,
        blurCommand: blurCommand,
        previewCommand: previewCommand,
        PREVIEW_INTERVAL_MS: PREVIEW_INTERVAL_MS,
        isEffect: isEffect,
        clampBlur: clampBlur,
        unknownState: unknownState,
        parseStatus: parseStatus,
        notRunningState: notRunningState,
        GLYPH: GLYPH,
        glyphFor: glyphFor,
        labelFor: labelFor,
        tooltipFor: tooltipFor,
        availableEffects: availableEffects,
        PARAMS: PARAMS,
        paramFor: paramFor,
        clampParam: clampParam,
        paramCommand: paramCommand,
        paramValue: paramValue,
        supportedParams: supportedParams,
        CHOICES: CHOICES,
        choiceFor: choiceFor,
        choiceValues: choiceValues,
        choiceValue: choiceValue,
        stepChoice: stepChoice,
        choiceCommand: choiceCommand,
        TOGGLES: TOGGLES,
        toggleFor: toggleFor,
        toggleCommand: toggleCommand,
        toggleValue: toggleValue,
        panelRows: panelRows,
        stepParam: stepParam,
        indexOfEffect: indexOfEffect,
        stepBlur: stepBlur
    }
}
