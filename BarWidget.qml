import QtQuick
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
  property bool switching: false
  property bool expectRunning: false
  property int settleAttempts: 0

  // ---- Talking to the daemon.
  //
  // Every run carries a generation. A reply that is superseded, that overruns
  // its deadline, or that outlives the widget has its generation left behind
  // and whatever it prints is dropped: a late answer is not a current one.
  property int generation: 0

  function send(argv) {
    if (!argv) return
    // A command already in flight is the current one and it has a deadline.
    // Pressing a key again while it runs is not a reason to start a second.
    if (clientProc.running) return

    root.generation++
    clientProc.generation = root.generation
    clientProc.command = argv
    clientProc.running = true
    deadline.restart()
  }

  function refresh() { root.send(Model.statusCommand()) }

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
      return
    }
    root.settleAttempts++
    root.refresh()
    settleTimer.restart()
  }
  function toggle() { root.send(Model.toggleCommand()) }
  function setEffect(effect) { root.send(Model.effectCommand(effect)) }
  function setBlur(radius) { root.send(Model.blurCommand(radius)) }
  function stepBlur(direction) { root.setBlur(Model.stepBlur(root.state, direction)) }

  // Give up on whatever is running and make sure nothing it prints is taken as
  // current. Used by the deadline, and on the way out.
  function abandon() {
    deadline.stop()
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
      switching: root.switching,
      effect: root.state.effect,
      blur: root.state.blur,
      device: root.state.device,
      input: root.state.input,
      output: root.state.output,
      background: root.state.background,
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

  Component.onCompleted: root.refresh()

  // Nothing this widget started may outlive it. The panel is destroyed on a
  // shell reload as well as on shutdown, and a client left running would go on
  // holding a pipe nobody is reading.
  Component.onDestruction: {
    killTimer.stop()
    settleTimer.stop()
    root.abandon()
    if (clientProc.running) clientProc.running = false
    if (unitProc.running) unitProc.running = false
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
        if (root.state.running === root.expectRunning) root.switching = false
        else settleTimer.restart()
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
        root.noteUnreachable()
        return
      }
      root.settle()
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
    onTriggered: if (!root.opened) root.refresh()
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
    function status(): string { return root.statusJson() }
  }

  WidgetButton {
    id: button
    anchors.fill: parent
    bar: root.bar
    text: root.glyph
    active: root.opened || root.effectsOn
    horizontalMargin: 8.75
    verticalPadding: 8.75
    tooltipText: Model.tooltipFor(root.state)

    onPressed: function(b) { root.togglePanel() }
  }
}
