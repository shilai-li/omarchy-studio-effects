import QtQuick
import Quickshell
import Quickshell.Io
import qs.Commons
import qs.Ui
import "Model.js" as Model

// The bar face of Studio Effects, and the only place that talks to the daemon.
// Panel.qml is a read-out of this widget and calls back into it, so a
// two-monitor desktop cannot end up with two disagreeing glyphs.
//
// This widget owns no settings of its own. The daemon holds the effect and the
// blur radius and answers every command with its whole state, so there is one
// copy of the truth and the widget is only ever showing it. A `defaults` block
// here would be a second opinion that goes stale the moment the CLI, a
// keybinding or another monitor changes anything.
//
// A plugin runs inside the shell process, with the shell's privileges and the
// session's whole environment. The one thing started from here is named by
// absolute path with its arguments as separate argv entries and no shell in
// the chain at all — see Model.js — and it has a deadline it cannot outlive.
BarWidget {
  id: root
  moduleName: "shilai_li.studio-effects"

  // What the daemon last told us. Replaced wholesale by every reply, never
  // patched field by field, because a partial update is how a widget starts
  // claiming a state the daemon is not in.
  property var state: Model.notRunningState()

  readonly property string glyph: Model.glyphFor(root.state)
  readonly property bool effectsOn: root.state.running && root.state.effect !== "none"

  // A start or stop is in flight. The daemon takes a moment to open the camera
  // after systemd reports the unit started, so the widget waits for the socket
  // to answer rather than claiming success the instant systemctl returns.
  // Voice focus: "on", "off", or "missing" when the unit is not installed.
  // Asked of systemd on its own, because it is a separate unit and the camera
  // daemon knows nothing about audio.
  property string voice: "missing"
  property bool voiceSwitching: false

  property bool switching: false
  property bool expectRunning: false
  property int settleAttempts: 0

  // Whether the daemon is installed at all: "unknown" until asked, then "yes" or
  // "no". A plugin arrives by `omarchy plugin add`, which clones files and
  // builds nothing, so the first thing anyone sees is a widget with no daemon
  // behind it -- and "not running" is the wrong thing to say about that.
  property string installed: "unknown"

  // Setup has been handed to a terminal. There is no handle on that window, so
  // this is only "somebody asked": it clears when the daemon turns up, and the
  // panel lets the request be made again if the window was closed.
  property bool settingUp: false

  // packaging/setup.sh beside this file, as a filesystem path.
  readonly property string setupScript:
    decodeURIComponent(Qt.resolvedUrl("packaging/setup.sh").toString().replace(/^file:\/\//, ""))

  // Set when a power change was accepted by systemd but the daemon did not
  // follow. Empty the rest of the time.
  property string powerNote: ""


  // ---- Talking to the daemon.
  //
  // Every run carries a generation. A reply that is superseded, that overruns
  // its deadline, or that outlives the widget has its generation left behind
  // and whatever it prints is dropped: a late answer is not a current one.
  property int generation: 0

  // The most recent request made while another was in flight. One slot, not a
  // queue: holding every keypress would replay a burst of blur changes one at
  // a time long after the user stopped pressing, and only the last one was
  // ever wanted.
  //
  // Dropping it instead, which is what this did first, loses commands that are
  // not merely the newest of a burst -- opening the panel asks to start the
  // preview immediately after a status read, and that request went missing
  // every time, so the preview simply never started.
  property var pending: null

  function send(argv) {
    if (!argv) return
    if (clientProc.running) {
      root.pending = argv
      return
    }

    root.generation++
    clientProc.generation = root.generation
    clientProc.command = argv
    clientProc.running = true
    deadline.restart()
  }

  function refresh() { root.send(Model.statusCommand()) }

  // ---- Installing. The widget builds nothing and runs no sudo: it opens the
  //      setup script in a terminal, where a person can watch it and answer the
  //      password prompt, and then waits to see the daemon appear.
  function checkInstalled() {
    if (installProc.running) return
    installProc.command = Model.installedCommand()
    installProc.running = true
  }

  function runSetup(method) {
    var argv = Model.setupLaunch(root.setupScript, method)
    if (!argv) return
    root.settingUp = true
    Quickshell.execDetached(argv)
    installPoll.restart()
  }

  // ---- Power. Starting and stopping the unit is the real on/off switch.
  //
  // While the daemon runs it holds the camera open: the recording light stays
  // lit, nothing else can open the real camera, and every frame is segmented
  // and composited whether or not anyone is watching. `effect none` only stops
  // the compositing, which is why it is not what this button does.
  function startService() { root.runUnit(Model.startCommand(), true) }
  function stopService() { root.runUnit(Model.stopCommand(), false) }
  function toggleService() {
    if (root.switching) return
    root.state.running ? root.stopService() : root.startService()
  }

  function runUnit(argv, expectRunning) {
    if (unitProc.running || root.switching) return
    root.powerNote = ""
    root.switching = true
    root.expectRunning = expectRunning
    root.settleAttempts = 0
    unitProc.command = argv
    unitProc.running = true
  }

  // systemctl returning is not the daemon being ready, so the state is re-read
  // until it agrees with what was asked for. Giving up leaves whatever the
  // daemon last said on screen rather than a guess.
  function settle() {
    if (root.settleAttempts >= Model.SETTLE_ATTEMPTS) {
      root.switching = false
      // systemd did as it was told and the daemon is still in the wrong state.
      // Almost always this is a daemon started by hand, outside the service:
      // the switch stops a unit that is not running while the loose process
      // keeps the camera. Saying nothing here reads as the switch being broken,
      // which is exactly the wrong place to go looking.
      root.powerNote = root.expectRunning
        ? "The service started but the daemon is not answering."
        : "Still running after the service stopped — a daemon may have been started outside systemd."
      return
    }
    root.settleAttempts++
    root.refresh()
    settleTimer.restart()
  }
  function toggle() { root.send(Model.toggleCommand()) }
  function setEffect(effect) { root.send(Model.effectCommand(effect)) }
  function setBlur(radius) { root.send(Model.blurCommand(radius)) }
  function setParam(key, value) { root.send(Model.paramCommand(key, value)) }
  function setToggle(key, on) { root.send(Model.toggleCommand(key, on)) }
  // Loading a model takes about 15 ms from the warm compile cache, and the
  // frame loop keeps running on the old one until the new one is ready, so
  // this needs no more ceremony than setting a blur radius.
  function setChoice(key, value) { root.send(Model.choiceCommand(key, value)) }

  // ---- Voice focus. Its own process, its own state: nothing here goes through
  //      the camera daemon's socket, so it works with the camera off.
  function readVoice() {
    if (voiceProc.running) return
    voiceProc.command = Model.voiceStatusCommand()
    voiceProc.running = true
  }

  function setVoice(on) {
    if (voiceUnitProc.running || root.voiceSwitching) return
    root.voiceSwitching = true
    voiceUnitProc.command = Model.voiceCommand(on)
    voiceUnitProc.running = true
  }

  function toggleVoice() {
    if (root.voice === "missing") return
    root.setVoice(root.voice !== "on")
  }

  // Asked for only while the panel is open. Nothing is encoded for a picture
  // nobody is looking at, and the daemon deletes the last frame when it stops
  // -- so a widget can never show a still of a camera that is no longer on.
  // Sent unconditionally rather than guarded on the last known state, which
  // may be stale. Asking a daemon that is not there fails harmlessly and
  // records that it is not there, so this is self-correcting where a guard
  // would need the state to already be right.
  function setPreview(on) { root.send(Model.previewCommand(on)) }

  // Give up on whatever is running and make sure nothing it prints is taken as
  // current. Used by the deadline, and on the way out.
  function abandon() {
    deadline.stop()
    // Whatever was waiting behind a command that had to be given up on is
    // waiting on a daemon that is not answering. Sending it would only queue
    // another timeout.
    root.pending = null
    root.generation++
    if (clientProc.running) {
      clientProc.signal(15)
      killTimer.restart()
    }
  }

  // A client that could not connect means no daemon, which is a real state to
  // show rather than an error to swallow — but it must never look like effects
  // are merely off, because the fix is different.
  function noteUnreachable() { root.state = Model.notRunningState() }

  function statusJson() {
    return JSON.stringify({
      running: root.state.running,
      installed: root.installed,
      settingUp: root.settingUp,
      switching: root.switching,
      effect: root.state.effect,
      blur: root.state.blur,
      passes: root.state.passes,
      dim: root.state.dim,
      desat: root.state.desat,
      framing: root.state.framing,
      voice: root.voice,
      powerNote: root.powerNote,
      device: root.state.device,
      input: root.state.input,
      output: root.state.output,
      background: root.state.background,
      // Reported because this is the only window onto what the widget believes,
      // and diagnosing a preview that would not start meant guessing at it.
      preview: root.state.preview,
      previewPath: root.state.previewPath,
      error: root.state.error
    })
  }

  // ---- Panel plumbing. Shape contract for shell.summon/hide/toggle routing:
  //      Bar.findPanelWidget requires open/close/opened on the bar-widget
  //      root, and the popout coordinator prefers closeForPopoutSwitch.
  readonly property bool opened: panelLoader.item ? panelLoader.item.opened === true : false
  readonly property bool popoutSwitchClosing: panelLoader.item ? panelLoader.item.popoutSwitchClosing === true : false

  function open() { if (panelLoader.item) panelLoader.item.open() }
  function close() { if (panelLoader.item) panelLoader.item.close() }
  function togglePanel() { if (panelLoader.item) panelLoader.item.toggle() }
  function closeForPopoutSwitch() { if (panelLoader.item) panelLoader.item.closeForPopoutSwitch() }

  function injectPanel() {
    var target = panelLoader.item
    if (!target) return
    if ("bar" in target) target.bar = root.bar
    if ("settings" in target) target.settings = root.settings
    if ("anchorItem" in target) target.anchorItem = button
    if ("hostWidget" in target) target.hostWidget = root
  }

  readonly property real openPanelIndicatorWidth: button.labelWidth
  readonly property real openPanelIndicatorHeight: Math.max(Style.space(10), Math.round(Style.bar.iconSlot * 0.55))

  implicitWidth: button.implicitWidth
  implicitHeight: button.implicitHeight

  onBarChanged: injectPanel()
  onSettingsChanged: injectPanel()

  Component.onCompleted: {
    root.checkInstalled()
    root.refresh()
    root.readVoice()
  }

  // Nothing this widget started may outlive it. The panel is destroyed on a
  // shell reload as well as on shutdown, and a client left running would go on
  // holding a pipe nobody is reading.
  Component.onDestruction: {
    killTimer.stop()
    settleTimer.stop()
    root.abandon()
    if (clientProc.running) clientProc.running = false
    if (unitProc.running) unitProc.running = false
    if (voiceProc.running) voiceProc.running = false
    if (voiceUnitProc.running) voiceUnitProc.running = false
    if (installProc.running) installProc.running = false
    installPoll.stop()
  }

  Process {
    id: clientProc
    stdout: StdioCollector { id: clientOut; waitForEnd: true }

    // Which send() this run belongs to. Compared against root.generation on
    // the way out; anything else answers a question already asked again.
    property int generation: 0

    onExited: function (exitCode, exitStatus) {
      deadline.stop()
      killTimer.stop()
      if (clientProc.generation !== root.generation) return

      // The client exits non-zero both when the daemon refuses a change and
      // when there is no daemon at all, and those need different messages —
      // so the reply, not the code, decides which happened. A refusal still
      // prints the daemon's full state and is worth applying.
      var parsed = Model.parseStatus(clientOut.text)
      if (parsed.ok) root.state = parsed
      else if (exitStatus !== 0 || exitCode !== 0) root.noteUnreachable()
      else root.state = parsed

      // A start or stop is only finished once the daemon agrees. Until then
      // the widget keeps asking, so the glyph never settles on a state the
      // daemon is not actually in.
      if (root.switching) {
        if (root.state.running === root.expectRunning) {
          root.switching = false
          root.powerNote = ""
        } else {
          settleTimer.restart()
        }
      } else if (root.powerNote.length > 0 && root.state.running === root.expectRunning) {
        // The note outlived the problem. A daemon that was slow to start, or a
        // service fixed and restarted since, leaves a complaint on screen that
        // is no longer true -- and a warning that does not go away when the
        // fault does teaches people to ignore warnings.
        root.powerNote = ""
      }

      if (root.pending) {
        var next = root.pending
        root.pending = null
        root.send(next)
      }
    }
  }

  // systemctl itself. Kept apart from the client's generation bookkeeping:
  // this is a different question with a different failure mode, and letting a
  // slow start cancel a status read would make the glyph flicker.
  Process {
    id: unitProc
    stderr: StdioCollector { id: unitErr; waitForEnd: true }

    onExited: function (exitCode, exitStatus) {
      if (exitStatus !== 0 || exitCode !== 0) {
        // systemd refused outright — a masked unit, or one that is not
        // installed. No amount of waiting will change that.
        root.switching = false
        root.powerNote = "systemd refused to change the service."
        root.noteUnreachable()
        return
      }
      root.settle()
    }
  }

  // `is-active` exits non-zero for anything but active, so the word it printed
  // is what gets read rather than the exit status: a code cannot tell "stopped"
  // from "no such unit", and those need different words in the panel.
  Process {
    id: voiceProc
    stdout: StdioCollector { id: voiceOut; waitForEnd: true }
    onExited: root.voice = Model.parseVoiceState(voiceOut.text)
  }

  // `test -x` on the daemon: exit 0 is there, 1 is not, anything else says
  // nothing and leaves the last answer standing.
  Process {
    id: installProc
    onExited: function (exitCode, exitStatus) {
      var answer = Model.parseInstalled(exitStatus === 0 ? exitCode : -1)
      if (answer === "unknown") return
      var was = root.installed
      root.installed = answer
      if (answer === "yes") {
        root.settingUp = false
        installPoll.stop()
        // It has just appeared: read what it is doing instead of waiting for
        // the next slow poll to say so.
        if (was !== "yes") {
          root.refresh()
          root.readVoice()
        }
      }
    }
  }

  // Quick while setup is running, since it ends when the package lands and
  // nothing here is told. Stops itself when the daemon is there.
  Timer {
    id: installPoll
    interval: Model.SETUP_POLL_MS
    repeat: true
    onTriggered: root.checkInstalled()
  }

  Process {
    id: voiceUnitProc
    onExited: {
      root.voiceSwitching = false
      // systemd has returned, but the filter takes a moment to publish its
      // node, so the answer is read rather than assumed.
      root.readVoice()
    }
  }

  Timer {
    id: settleTimer
    interval: Model.SETTLE_INTERVAL_MS
    onTriggered: root.settle()
  }

  // The client speaks to a unix socket on this machine: it answers at once or
  // not at all. This is the backstop for it wedging, and it treats a wedged
  // client as no daemon, which is what it amounts to from here.
  Timer {
    id: deadline
    interval: Model.TIMEOUT_SECONDS * 1000
    onTriggered: {
      root.abandon()
      root.noteUnreachable()
    }
  }

  Timer {
    id: killTimer
    interval: 2000
    onTriggered: if (clientProc.running) clientProc.signal(9)
  }

  // The glyph is state a user reads at a glance, and it can change from
  // outside this widget — the CLI, a keybinding, another monitor's panel. That
  // makes it exactly the case a periodic re-read is for, unlike a list nobody
  // is looking at. Kept slow, and it is one short-lived process talking to a
  // local socket.
  Timer {
    interval: 10000
    running: true
    repeat: true
    onTriggered: {
      // Cheap, and the way a daemon installed by hand -- or removed -- is
      // noticed without anybody opening the panel.
      root.checkInstalled()
      if (!root.opened) {
        root.refresh()
        root.readVoice()
      }
    }
  }

  Loader {
    id: panelLoader
    active: true
    source: Qt.resolvedUrl("Panel.qml")
    visible: false
    onLoaded: {
      root.injectPanel()
      Qt.callLater(root.injectPanel)
    }
  }

  IpcHandler {
    target: "shilai_li.studio-effects"

    function toggle(): void { root.togglePanel() }
    function open(): void { root.open() }
    function close(): void { root.close() }
    function show(): void { root.open() }
    function hide(): void { root.close() }
    function refresh(): void { root.refresh() }
    function toggleEffects(): void { root.toggle() }
    function on(): void { root.startService() }
    function off(): void { root.stopService() }
    function togglePower(): void { root.toggleService() }
    function setup(): void { root.runSetup() }
    function toggleVoice(): void { root.toggleVoice() }
    function status(): string { return root.statusJson() }
  }

  WidgetButton {
    id: button
    anchors.fill: parent
    bar: root.bar
    text: root.glyph
    // Active if anything the widget controls is on. With one glyph for both
    // halves, a microphone filter running while the camera is off still has to
    // look like something is happening.
    active: root.opened || root.effectsOn || root.voice === "on"
    horizontalMargin: 8.75
    verticalPadding: 8.75
    tooltipText: Model.tooltipFor(root.state, root.voice, root.installed)

    onPressed: function(b) { root.togglePanel() }
  }
}
