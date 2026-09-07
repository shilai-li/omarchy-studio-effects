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

// The daemon needs a moment to open the camera after systemd reports the unit
// started, so the widget re-asks rather than concluding from one silent reply
// that starting failed.
var SETTLE_ATTEMPTS = 10;
var SETTLE_INTERVAL_MS = 400;
function toggleCommand() { return command(["toggle"]); }

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
var PARAMS = [
    { key: "blur",   label: "Blur",       min: 0, max: 200, step: 6,  effects: ["blur"] },
    { key: "passes", label: "Smoothness", min: 1, max: 3,   step: 1,  effects: ["blur"] },
    { key: "dim",    label: "Darken",     min: 0, max: 100, step: 10, effects: ["blur", "replace"] },
    { key: "desat",  label: "Desaturate", min: 0, max: 100, step: 10, effects: ["blur", "replace"] }
];

// A setting counts as supported when the daemon reports a value for it.
function supportedParams(parsed) {
    var found = {};
    for (var i = 0; i < PARAMS.length; i++) {
        var key = PARAMS[i].key;
        found[key] = parsed !== null && typeof parsed === "object"
            && typeof parsed[key] === "number";
    }
    return found;
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

    var current = state ? state.effect : "none";
    var supports = state && state.supports ? state.supports : {};
    for (var j = 0; j < PARAMS.length; j++) {
        if (PARAMS[j].effects.indexOf(current) === -1) continue;
        if (!supports[PARAMS[j].key]) continue;
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
        passes: 1,
        dim: 0,
        desat: 0,
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
        input: typeof parsed.input === "string" ? parsed.input : "",
        output: typeof parsed.output === "string" ? parsed.output : "",
        background: parsed.background === true,
        preview: parsed.preview === true,
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

function glyphFor(state) {
    if (!state || !state.running) return "\u{F0568}";   // video-off
    if (state.effect === "replace") return "\u{F02E9}"; // image
    if (state.effect === "blur") return "\u{F0567}";    // video
    return "\u{F0568}";
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

function tooltipFor(state) {
    if (!state || !state.running) return "Studio Effects — daemon not running";
    var where = state.device ? " on " + state.device : "";
    return "Studio Effects — " + labelFor(state).toLowerCase() + where;
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
        panelRows: panelRows,
        stepParam: stepParam,
        indexOfEffect: indexOfEffect,
        stepBlur: stepBlur
    }
}
