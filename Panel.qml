import QtQuick
import Quickshell.Io
import qs.Commons
import qs.Commons as Commons
import qs.Ui
import "Model.js" as Model

// The effect panel: a live preview, the camera and microphone switches, the
// effects as a row of buttons, and the settings the chosen effect uses.
//
// The panel owns the cursor and nothing else. The daemon's state and every
// command live on BarWidget.qml, so two monitors showing this panel see the
// same thing and a change made on one appears on the other.
//
// One highlight on screen at a time: rows paint from CursorSurface's
// `hasCursor`, never from `containsMouse`, and the mouse moves the same cursor
// the arrow keys do.
Panel {
  id: root
  moduleName: "shilai_li.studio-effects"
  ipcTarget: "shilai_li.studio-effects"
  manageIpc: false

  property var anchorItem: null

  // The bar tracks the widget mounted in its slot — BarWidget.qml — not this
  // nested panel, so everything the bar identifies a panel by has to be that
  // widget.
  property var hostWidget: null
  readonly property var barIdentity: hostWidget || root
  readonly property var host: hostWidget

  // ---- Read-out of the host. Guarded throughout: the bar-widget contract
  //      instantiates this bare, before injection.
  readonly property var state: host ? host.state : Model.notRunningState()
  readonly property bool running: state.running === true
  readonly property bool switching: host ? host.switching === true : false
  readonly property string voice: host ? host.voice : "missing"
  readonly property string powerNote: host ? host.powerNote : ""
  readonly property bool voiceSwitching: host ? host.voiceSwitching === true : false

  // The daemon is not there at all, which is a different problem from it not
  // running and has a different fix: build one, not start one. Only a definite
  // "no" counts, so a check that has not answered yet shows the ordinary panel
  // rather than flashing an install offer at somebody who has the daemon.
  readonly property bool notInstalled: host ? host.installed === "no" : false
  readonly property bool settingUp: host ? host.settingUp === true : false
  readonly property string prebuiltStatus: host ? (host.prebuiltStatus || "checking") : "checking"
  readonly property bool prebuiltAvailable: root.prebuiltStatus === "available"
  readonly property var rows: Model.panelRows(root.state)
  readonly property var currentRow: selectedIndex >= 0 && selectedIndex < rows.length
    ? rows[selectedIndex] : null

  // The one list of rows, drawn in three places. Each keeps its index into
  // `rows`, so the cursor still walks a single list in the order it is drawn.
  function rowsOf(kinds) {
    var picked = []
    for (var i = 0; i < root.rows.length; i++)
      if (kinds.indexOf(root.rows[i].kind) !== -1) picked.push({ row: root.rows[i], index: i })
    return picked
  }
  readonly property var effectTiles: root.rowsOf(["effect", "hint"])
  readonly property var listRows: root.rowsOf(["toggle", "choice"])
  readonly property var paramRows: root.rowsOf(["param"])
  readonly property var hintSpec: {
    for (var i = 0; i < root.rows.length; i++)
      if (root.rows[i].kind === "hint") return Model.hintFor(root.rows[i].key)
    return null
  }

  property int selectedIndex: 0
  property int setupIndex: 1
  property bool cursorActive: false

  readonly property string previewPath: root.state.previewPath || ""
  readonly property bool previewLive: root.running && root.state.preview && previewPath.length > 0

  // The window's scale, so the daemon can encode the preview at the box's size
  // in physical pixels and it is drawn 1:1. The screen's is the fallback, and
  // it is never smaller than the window's, so a guess errs toward pixels to
  // spare.
  readonly property real previewScale: {
    var window = previewBox.Window.window
    var ratio = window ? Number(window.devicePixelRatio) : 0
    return ratio > 0 ? ratio : Screen.devicePixelRatio
  }
  readonly property string previewSize:
    Model.previewSize(previewBox.width, previewBox.height, root.previewScale)

  // Every request carries the size, so the daemon always encodes for the box
  // as it is now.
  function requestPreview() {
    if (root.host) root.host.setPreview(true, previewBox.width, previewBox.height, root.previewScale)
  }

  // ---- Theme. Nothing here names a color; the palette does.
  readonly property color contentForeground: bar ? bar.foreground : Commons.Color.foreground
  readonly property string contentFontFamily: bar ? bar.fontFamily : Style.font.family
  readonly property color surface: Commons.Color.popups.background
  readonly property color dim: Qt.darker(contentForeground, 1.5)
  readonly property color accentColor: Style.selectedStateColor(contentForeground, Commons.Color.accent)
  readonly property color hoverFill: Style.hoverFillFor(contentForeground, Commons.Color.accent)
  readonly property color selectedFill: Style.selectedFillFor(contentForeground, Commons.Color.accent)

  // Cards are a tint of the text color laid over the panel, so they sit on any
  // theme without a color of their own. Something switched on is filled with
  // the accent and carries its glyph in the panel's own color, cut out of it.
  readonly property color cardFill: Util.alpha(contentForeground, 0.06)
  readonly property color quietFill: Util.alpha(contentForeground, 0.12)
  readonly property color onFill: Commons.Color.accent

  // The theme decides whether anything is round. Square corners stay square;
  // rounded ones grow a step, because the cards here are larger than a row.
  readonly property bool rounded: Style.cornerRadius > 0
  readonly property int cardRadius: rounded ? Math.max(Style.cornerRadius * 2, Style.space(12)) : 0

  readonly property int rowHeight: Math.max(Style.space(30), Style.font.body + Style.space(16))

  // ---- Lifecycle.
  function open() {
    root.cursorActive = true
    // Turning the preview on doubles as the refresh: every reply carries the
    // daemon's whole state. Asking for both would be one request too many --
    // only one command is in flight at a time, so the second would be dropped.
    root.requestPreview()
    // Installed by hand, or removed, since the last slow poll.
    if (root.host) root.host.checkInstalled()
    if (root.notInstalled && root.host) root.host.checkPrebuilt(false)
    // Voice focus is a different unit with a life of its own -- a keybinding or
    // systemctl may have changed it since the last poll.
    if (root.host) root.host.readVoice()
    root.syncCursorToEffect()
    root.controller.show()
    Qt.callLater(function() {
      if (root.opened) root.setCenterHoverRevealSuppressed(true)
    })
  }

  // The hide goes first. Everything after it is cosmetic, and this panel is
  // a full-screen layer-shell surface holding keyboard focus: a call that
  // throws before the hide lands leaves the user with no way out of it and a
  // bar that no longer answers. Nothing decorative gets to stand in front of
  // the one line that releases the screen.
  function close() {
    root.controller.hide()
    // Stop the encoder on the way out, not on the way in to the next open: a
    // panel closed on one monitor should cost nothing, and the daemon has no
    // other way to know nobody is watching.
    if (root.host) root.host.setPreview(false)
    root.setCenterHoverRevealSuppressed(false)
  }

  function toggle() { root.opened ? root.close() : root.open() }

  function switchPanel(direction) {
    if (root.bar && typeof root.bar.switchPanelFrom === "function")
      return root.bar.switchPanelFrom(root.barIdentity, direction)
    return false
  }

  // Summoning by hotkey moves no pointer, so a hover the bar was still holding
  // must not keep the center indicators revealed behind the panel.
  //
  // The setter is the supported route: the `bar` a plugin is handed is a
  // PluginBarApi facade, where this flag is readonly and backed by a scoped
  // callback. Assigning to it throws rather than failing quietly, which is
  // why the direct write is only the fallback for a host that predates the
  // setter — and why it is now second.
  function setCenterHoverRevealSuppressed(value) {
    if (root.bar && typeof root.bar.setCenterHoverRevealSuppressed === "function")
      root.bar.setCenterHoverRevealSuppressed(value)
    else if (root.bar && "centerHoverRevealSuppressed" in root.bar)
      root.bar.centerHoverRevealSuppressed = value
  }

  // ---- Cursor. It follows the daemon rather than staying where it was: the
  //      row that is on is the one worth landing on.
  function syncCursorToEffect() {
    root.selectedIndex = Model.indexOfEffect(root.state, root.rows)
  }

  // Up and down walk the rows, with the effects counted as one stop because
  // they are drawn as one row of buttons. Hints are walked past: they are there
  // to be read, and enter does nothing on one.
  function moveCursor(delta) {
    if (root.notInstalled) {
      root.cursorActive = true
      root.setupIndex = root.prebuiltAvailable ? 1 - root.setupIndex : 1
      return
    }
    if (root.rows.length === 0) return
    root.cursorActive = true
    root.selectedIndex = Model.nextStop(root.rows, root.selectedIndex, delta < 0 ? -1 : 1, root.state)
  }

  // ---- Actions. Choosing an effect leaves the panel up: the point is to see
  //      the change land, and the next thing a user does is often adjust it.
  function chooseSelected() {
    if (root.notInstalled) {
      if (root.setupIndex === 0 && !root.prebuiltAvailable) return
      if (root.host) root.host.runSetup(root.setupIndex === 0 ? "release" : "build")
      return
    }
    if (!root.host || !root.currentRow) return
    if (root.currentRow.kind === "effect") {
      root.host.setEffect(root.currentRow.effect)
    } else if (root.currentRow.kind === "toggle") {
      root.host.setToggle(root.currentRow.key,
                          !Model.toggleValue(root.state, root.currentRow.key))
    } else if (root.currentRow.kind === "choice") {
      root.adjustSelected(1)
    }
  }

  // Left and right adjust whatever row the cursor is on. On the effects they
  // move along the row of buttons instead, because that is the direction the
  // row runs -- and they only move: enter chooses, as it does everywhere.
  function adjustSelected(direction) {
    if (!root.host || !root.currentRow) return
    if (root.currentRow.kind === "effect") {
      root.selectedIndex = Model.effectNeighbour(root.rows, root.selectedIndex, direction)
    } else if (root.currentRow.kind === "param") {
      root.host.setParam(root.currentRow.key,
                         Model.stepParam(root.state, root.currentRow.key, direction))
    } else if (root.currentRow.kind === "choice") {
      root.host.setChoice(root.currentRow.key,
                          Model.stepChoice(root.state, root.currentRow.key, direction))
    }
  }

  // Both start a unit the package installs, so with no package they can only
  // fail -- and fail as "systemd refused", which sends anyone looking in the
  // wrong place.
  function togglePower() {
    if (root.host && !root.notInstalled) root.host.toggleService()
  }

  function toggleVoice() {
    if (root.host && !root.notInstalled) root.host.toggleVoice()
  }

  function handleTextKey(text) {
    var key = String(text).toLowerCase()
    if (key === "i" && root.notInstalled && root.host) root.chooseSelected()
    else if (key === "r" && root.host) {
      if (root.notInstalled) root.host.checkPrebuilt(true)
      else root.host.refresh()
    }
    else if (key === "f" && root.host) root.host.toggle()
    else if (key === "c") root.togglePower()
    else if (key === "v") root.toggleVoice()
  }

  // What the keys do from here, as key and meaning.
  readonly property var keyHints: {
    if (root.notInstalled)
      return [["↑↓", "move"], ["enter", "set up"], ["r", "recheck"], ["esc", "close"]]
    if (!root.running)
      return [["c", "camera"], ["v", "voice"], ["esc", "close"]]
    var kind = root.currentRow ? root.currentRow.kind : ""
    var here = kind === "effect" ? [["←→", "pick"], ["enter", "apply"]]
             : kind === "param" ? [["←→", "adjust"]]
             : kind === "choice" ? [["←→", "switch"]]
             : kind === "toggle" ? [["enter", "switch"]]
             : []
    return [["↑↓", "move"]].concat(here, [["c", "off"], ["v", "voice"], ["esc", "close"]])
  }

  // The preview is asked for on open, but a daemon can arrive long after that:
  // pressing `c` stops and restarts it underneath a panel that stays open the
  // whole time, so open() never runs again and nothing would re-request the
  // frames. Watch the daemon coming back instead of the panel opening.
  onRunningChanged: {
    previewBox.givenUp = false
    if (root.running && root.opened) root.requestPreview()
  }

  // The box's size settles after the panel opens, and moves with the monitor
  // and its scale. The daemon rebuilds its encoder for the new size.
  onPreviewSizeChanged: {
    if (root.running && root.opened) Qt.callLater(root.requestPreview)
  }

  onPrebuiltAvailableChanged: {
    if (!root.prebuiltAvailable && root.setupIndex === 0) root.setupIndex = 1
  }

  // `replace` disappearing — the daemon restarted without a background — can
  // leave the cursor past the end of the list.
  onRowsChanged: {
    if (root.selectedIndex >= root.rows.length)
      root.selectedIndex = Math.max(0, root.rows.length - 1)
    // Rows come and go with the daemon's state, so the cursor can be left on a
    // hint that has just appeared under it.
    if (!Model.isSelectable(root.rows[root.selectedIndex]))
      root.selectedIndex = Model.nextSelectable(root.rows, root.selectedIndex, -1)
  }

  // ---- Pieces.

  component Label: Text {
    textFormat: Text.PlainText
    color: root.contentForeground
    font.family: root.contentFontFamily
    font.pixelSize: Style.font.body
  }

  component Card: Rectangle {
    width: parent ? parent.width : 0
    radius: root.cardRadius
    color: root.cardFill
  }

  // A round button face: the accent when on, a quiet tint when off. Square on a
  // theme with square corners.
  component Disc: Rectangle {
    id: disc
    property bool on: false
    property bool muted: false
    property string glyph: ""
    property real size: Style.space(38)
    width: size
    height: size
    radius: root.rounded ? size / 2 : 0
    color: disc.on ? root.onFill : root.quietFill
    opacity: disc.muted ? 0.45 : 1

    Behavior on color { ColorAnimation { duration: Style.duration(120) } }

    Label {
      anchors.centerIn: parent
      text: disc.glyph
      color: disc.on ? root.surface : root.contentForeground
      font.pixelSize: Math.round(disc.size * 0.46)
    }
  }

  // A small label in the header, beside the status.
  component Pill: Rectangle {
    property alias text: pillText.text
    width: pillText.implicitWidth + Style.space(14)
    height: pillText.implicitHeight + Style.space(6)
    radius: root.rounded ? height / 2 : 0
    color: root.quietFill

    Label {
      id: pillText
      anchors.centerIn: parent
      color: root.dim
      font.pixelSize: Style.font.caption
      font.bold: true
    }
  }

  // Camera and microphone: the two things that hold a device. Peers of each
  // other and of nothing else, so they sit side by side above the effects.
  component PowerTile: CursorSurface {
    id: tile
    property string label: ""
    property string status: ""
    property string glyph: ""
    property bool on: false
    property bool unavailable: false
    signal activated()

    height: Style.space(12) * 2 + Style.space(38)
    radius: root.cardRadius
    hasCursor: false
    color: root.cardFill
    foreground: root.contentForeground
    accent: Commons.Color.accent

    MouseArea {
      anchors.fill: parent
      hoverEnabled: true
      cursorShape: tile.unavailable ? Qt.ArrowCursor : Qt.PointingHandCursor
      onClicked: tile.activated()
    }

    Row {
      anchors.left: parent.left
      anchors.leftMargin: Style.space(12)
      anchors.right: parent.right
      anchors.verticalCenter: parent.verticalCenter
      spacing: Style.space(10)

      Disc {
        on: tile.on
        muted: tile.unavailable
        glyph: tile.glyph
      }
      Column {
        anchors.verticalCenter: parent.verticalCenter
        width: parent.width - Style.space(38) - Style.space(22)
        spacing: Style.space(1)

        Label {
          width: parent.width
          text: tile.label
          font.bold: true
          elide: Text.ElideRight
          color: tile.unavailable ? root.dim : root.contentForeground
        }
        Label {
          width: parent.width
          text: tile.status
          color: root.dim
          font.pixelSize: Style.font.bodySmall
          elide: Text.ElideRight
        }
      }
    }
  }

  // One effect in the row of them. A hint stands in for an effect that is not
  // set up yet: drawn like the others, dimmed, and not something to land on.
  component EffectTile: CursorSurface {
    id: effectTile
    required property var modelData
    readonly property var row: modelData.row
    readonly property bool isHint: row.kind === "hint"
    readonly property string effect: effectTile.isHint ? row.key : row.effect
    readonly property bool isCurrent: !effectTile.isHint && root.running && root.state.effect === effectTile.effect

    height: effectColumn.implicitHeight + Style.space(16)
    radius: Math.max(0, root.cardRadius - Style.space(4))
    hasCursor: !effectTile.isHint && root.cursorActive && root.selectedIndex === modelData.index
    foreground: root.contentForeground
    accent: Commons.Color.accent
    fill: root.hoverFill
    currentFill: root.selectedFill

    MouseArea {
      anchors.fill: parent
      enabled: !effectTile.isHint
      hoverEnabled: true
      cursorShape: Qt.PointingHandCursor
      onContainsMouseChanged: if (containsMouse) {
        root.cursorActive = true
        root.selectedIndex = effectTile.modelData.index
      }
      onClicked: {
        root.selectedIndex = effectTile.modelData.index
        root.chooseSelected()
      }
    }

    Column {
      id: effectColumn
      anchors.centerIn: parent
      spacing: Style.space(7)

      Disc {
        anchors.horizontalCenter: parent.horizontalCenter
        size: Style.space(46)
        on: effectTile.isCurrent
        muted: effectTile.isHint
        glyph: effectTile.effect === "none" ? "\u{F073A}"
             : effectTile.effect === "blur" ? "\u{F00B5}"
             : "\u{F02E9}"
      }
      Label {
        anchors.horizontalCenter: parent.horizontalCenter
        text: effectTile.effect === "none" ? "None"
            : effectTile.effect === "blur" ? "Blur"
            : "Replace"
        color: effectTile.isCurrent ? root.contentForeground : root.dim
        font.bold: effectTile.isCurrent
        font.pixelSize: Style.font.bodySmall
      }
    }
  }

  // A switch or a choice, as a row of the grouped list under the effects.
  component ListRow: CursorSurface {
    id: listRow
    required property var modelData
    required property int index
    readonly property var row: modelData.row
    readonly property bool isToggle: row.kind === "toggle"
    readonly property var spec: listRow.isToggle ? Model.toggleFor(row.key) : Model.choiceFor(row.key)

    width: parent ? parent.width : 0
    height: root.rowHeight + Style.space(4)
    radius: Math.max(0, root.cardRadius - Style.space(4))
    hasCursor: root.cursorActive && root.selectedIndex === modelData.index
    foreground: root.contentForeground
    accent: Commons.Color.accent
    fill: root.hoverFill
    currentFill: root.selectedFill

    PanelSeparator {
      visible: listRow.index > 0
      anchors.top: parent.top
      anchors.left: parent.left
      anchors.leftMargin: Style.space(40)
      width: parent.width - Style.space(40)
      foreground: root.contentForeground
      strength: 0.08
    }

    MouseArea {
      anchors.fill: parent
      hoverEnabled: true
      cursorShape: Qt.PointingHandCursor
      onContainsMouseChanged: if (containsMouse) {
        root.cursorActive = true
        root.selectedIndex = listRow.modelData.index
      }
      // A choice steps by halves: the left of the row goes back, the right on.
      onClicked: function (mouse) {
        root.selectedIndex = listRow.modelData.index
        if (listRow.isToggle) root.chooseSelected()
        else root.adjustSelected(mouse.x > listRow.width / 2 ? 1 : -1)
      }
    }

    Label {
      id: listGlyph
      anchors.left: parent.left
      anchors.leftMargin: Style.space(12)
      anchors.verticalCenter: parent.verticalCenter
      width: Style.space(18)
      text: listRow.row.key === "framing" ? "\u{F019E}"
          : listRow.row.key === "model" ? "\u{F061A}"
          : "\u{F02F9}"
      color: root.dim
      font.pixelSize: Style.font.icon
    }
    Label {
      anchors.left: listGlyph.right
      anchors.leftMargin: Style.space(10)
      anchors.verticalCenter: parent.verticalCenter
      text: listRow.spec ? listRow.spec.label : listRow.row.key
    }

    ToggleSwitch {
      visible: listRow.isToggle
      anchors.right: parent.right
      anchors.rightMargin: Style.space(10)
      anchors.verticalCenter: parent.verticalCenter
      interactive: false
      trackHeight: Style.space(18)
      checked: Model.toggleValue(root.state, listRow.row.key)
      foreground: root.contentForeground
    }
    // The daemon's own name for it, not a prettier one: it is what the config
    // file takes and what `studio-effects model` takes, and a second name for
    // the same thing is a thing to get wrong.
    Label {
      visible: !listRow.isToggle
      anchors.right: parent.right
      anchors.rightMargin: Style.space(12)
      anchors.verticalCenter: parent.verticalCenter
      width: Math.min(implicitWidth, listRow.width / 2)
      horizontalAlignment: Text.AlignRight
      elide: Text.ElideMiddle
      text: {
        var v = Model.choiceValue(root.state, listRow.row.key)
        return listRow.hasCursor ? "‹ " + v + " ›" : v
      }
      color: listRow.hasCursor ? root.accentColor : root.dim
    }
  }

  // A filling capsule: the part up to the value is solid, with the setting's
  // glyph cut into its left end. It is dragged to the same steps the arrow
  // keys take, and draws where the pointer is while it is held rather than
  // waiting for the daemon to answer each step.
  component Capsule: Item {
    id: capsule
    required property string key
    property string glyph: ""
    readonly property var spec: Model.paramFor(capsule.key)
    readonly property real value: Model.paramValue(root.state, capsule.key)
    property real live: 0
    property bool dragging: false
    readonly property real progress: capsule.spec
      ? Math.max(0, Math.min(1, ((capsule.dragging ? capsule.live : capsule.value) - capsule.spec.min)
                                / Math.max(1, capsule.spec.max - capsule.spec.min)))
      : 0

    width: parent ? parent.width : 0
    height: Style.space(30)

    function apply(next) {
      if (next === capsule.live) return
      capsule.live = next
      if (root.host) root.host.setParam(capsule.key, next)
    }

    Rectangle {
      id: capsuleTrack
      anchors.fill: parent
      radius: root.rounded ? height / 2 : 0
      color: root.quietFill
    }
    Rectangle {
      anchors.left: parent.left
      anchors.top: parent.top
      anchors.bottom: parent.bottom
      width: capsule.height + (capsule.width - capsule.height) * capsule.progress
      radius: capsuleTrack.radius
      color: root.contentForeground

      Behavior on width {
        enabled: !capsule.dragging
        NumberAnimation { duration: Style.duration(140); easing.type: Easing.OutCubic }
      }
    }
    Label {
      x: Math.round((capsule.height - width) / 2)
      anchors.verticalCenter: parent.verticalCenter
      text: capsule.glyph
      color: root.surface
      font.pixelSize: Style.font.body
    }

    MouseArea {
      anchors.fill: parent
      cursorShape: Qt.PointingHandCursor

      function valueAt(x) {
        var span = Math.max(1, capsule.width - capsule.height)
        var p = Math.max(0, Math.min(1, (x - capsule.height / 2) / span))
        return Model.snapParam(capsule.key, capsule.spec.min + p * (capsule.spec.max - capsule.spec.min))
      }

      onPressed: function (mouse) {
        capsule.live = capsule.value
        capsule.dragging = true
        capsule.apply(valueAt(mouse.x))
      }
      onPositionChanged: function (mouse) {
        if (capsule.dragging) capsule.apply(valueAt(mouse.x))
      }
      onReleased: capsule.dragging = false
      onCanceled: capsule.dragging = false
      onWheel: function (wheel) {
        if (!root.host) return
        root.host.setParam(capsule.key,
                           Model.stepParam(root.state, capsule.key, wheel.angleDelta.y > 0 ? 1 : -1))
      }
    }
  }

  // Smoothness has three values, so it is three buttons rather than a slider
  // pretending to be continuous.
  component Segments: Rectangle {
    id: segments
    required property string key
    readonly property var spec: Model.paramFor(segments.key)
    readonly property int value: Model.paramValue(root.state, segments.key)

    width: parent ? parent.width : 0
    height: Style.space(30)
    radius: root.rounded ? Style.space(10) : 0
    color: root.quietFill

    Row {
      anchors.fill: parent
      anchors.margins: Style.space(3)

      Repeater {
        model: segments.spec ? segments.spec.max - segments.spec.min + 1 : 0

        Rectangle {
          id: segment
          required property int index
          readonly property int stepValue: segments.spec.min + segment.index
          readonly property bool on: segments.value === segment.stepValue
          width: parent.width / (segments.spec.max - segments.spec.min + 1)
          height: parent.height
          radius: root.rounded ? Style.space(8) : 0
          color: segment.on ? Util.alpha(root.contentForeground, 0.2) : "transparent"

          Behavior on color { ColorAnimation { duration: Style.duration(120) } }

          Label {
            anchors.centerIn: parent
            text: String(segment.stepValue + (segments.spec.displayOffset || 0))
            color: segment.on ? root.contentForeground : root.dim
            font.bold: segment.on
          }
          MouseArea {
            anchors.fill: parent
            cursorShape: Qt.PointingHandCursor
            onClicked: if (root.host) root.host.setParam(segments.key, segment.stepValue)
          }
        }
      }
    }
  }

  // A setting: its name and value over the control that changes it.
  component ParamBlock: CursorSurface {
    id: paramBlock
    required property var modelData
    readonly property string key: modelData.row.key
    readonly property var spec: Model.paramFor(paramBlock.key)

    width: parent ? parent.width : 0
    height: paramColumn.implicitHeight + Style.space(14)
    radius: Math.max(0, root.cardRadius - Style.space(2))
    hasCursor: root.cursorActive && root.selectedIndex === modelData.index
    foreground: root.contentForeground
    accent: Commons.Color.accent
    fill: root.hoverFill
    currentFill: root.selectedFill

    HoverHandler {
      onHoveredChanged: if (hovered) {
        root.cursorActive = true
        root.selectedIndex = paramBlock.modelData.index
      }
    }

    Column {
      id: paramColumn
      x: Style.space(6)
      y: Style.space(7)
      width: parent.width - Style.space(12)
      spacing: Style.space(6)

      Item {
        width: parent.width
        height: paramName.implicitHeight

        Label {
          id: paramName
          x: Style.space(4)
          text: paramBlock.spec ? paramBlock.spec.label : paramBlock.key
          color: root.dim
          font.pixelSize: Style.font.bodySmall
          font.bold: true
        }
        // Arrows on the selected setting, so it is discoverable that it is
        // adjusted rather than chosen.
        Label {
          anchors.right: parent.right
          anchors.rightMargin: Style.space(4)
          text: {
            var v = Model.paramText(root.state, paramBlock.key)
            return paramBlock.hasCursor ? "‹ " + v + " ›" : v
          }
          color: paramBlock.hasCursor ? root.accentColor : root.dim
          font.pixelSize: Style.font.bodySmall
          font.bold: true
        }
      }

      Segments {
        visible: paramBlock.key === "passes"
        key: paramBlock.key
      }
      Capsule {
        visible: paramBlock.key !== "passes"
        key: paramBlock.key
        glyph: paramBlock.key === "zoom" ? "\u{F0349}"
             : paramBlock.key === "dim" ? "\u{F0594}"
             : paramBlock.key === "desat" ? "\u{F0301}"
             : "\u{F00B5}"
      }
    }
  }

  // An install route, as a row of the grouped list on the first run.
  component SetupRow: CursorSurface {
    id: setup
    required property int index
    required property string label
    required property string detail
    required property string glyph
    property bool selectable: true
    enabled: setup.selectable
    width: parent ? parent.width : 0
    height: Math.max(root.rowHeight, setupText.implicitHeight) + Style.space(16)
    radius: Math.max(0, root.cardRadius - Style.space(4))
    hasCursor: setup.selectable && root.cursorActive && root.setupIndex === setup.index
    foreground: root.contentForeground
    accent: Commons.Color.accent
    fill: root.hoverFill
    currentFill: root.selectedFill

    MouseArea {
      anchors.fill: parent
      enabled: setup.selectable
      hoverEnabled: true
      cursorShape: Qt.PointingHandCursor
      onContainsMouseChanged: if (containsMouse) {
        root.cursorActive = true
        root.setupIndex = setup.index
      }
      onClicked: {
        root.setupIndex = setup.index
        root.chooseSelected()
      }
    }

    Rectangle {
      id: setupIcon
      anchors.left: parent.left
      anchors.leftMargin: Style.space(10)
      anchors.verticalCenter: parent.verticalCenter
      width: Style.space(28)
      height: width
      radius: root.rounded ? Style.space(7) : 0
      color: setup.selectable ? root.onFill : root.quietFill

      Label {
        anchors.centerIn: parent
        text: setup.glyph
        color: setup.selectable ? root.surface : root.dim
        font.pixelSize: Style.font.icon
      }
    }

    Column {
      id: setupText
      anchors.left: setupIcon.right
      anchors.leftMargin: Style.space(12)
      anchors.right: setupTrail.left
      anchors.rightMargin: Style.space(8)
      anchors.verticalCenter: parent.verticalCenter
      spacing: Style.space(2)

      Label {
        width: parent.width
        text: setup.label
        font.bold: true
        color: setup.selectable ? root.contentForeground : root.dim
        elide: Text.ElideRight
      }
      Label {
        width: parent.width
        text: setup.detail
        color: root.dim
        font.pixelSize: Style.font.bodySmall
        wrapMode: Text.WordWrap
      }
    }

    Label {
      id: setupTrail
      anchors.right: parent.right
      anchors.rightMargin: Style.space(12)
      anchors.verticalCenter: parent.verticalCenter
      text: setup.hasCursor ? "enter" : setup.selectable ? "\u{F0142}" : ""
      color: setup.hasCursor ? root.accentColor : root.dim
      font.pixelSize: setup.hasCursor ? Style.font.caption : Style.font.icon
    }
  }

  component KeyHint: Row {
    id: keyHint
    required property var modelData
    spacing: Style.space(4)

    Rectangle {
      anchors.verticalCenter: parent.verticalCenter
      width: keyText.implicitWidth + Style.space(8)
      height: keyText.implicitHeight + Style.space(2)
      radius: root.rounded ? Style.space(4) : 0
      color: root.quietFill

      Label {
        id: keyText
        anchors.centerIn: parent
        text: keyHint.modelData[0]
        color: root.contentForeground
        font.pixelSize: Style.font.caption
        font.bold: true
      }
    }
    Label {
      anchors.verticalCenter: parent.verticalCenter
      text: keyHint.modelData[1]
      color: root.dim
      font.pixelSize: Style.font.caption
    }
  }

  KeyboardPanel {
    id: panel
    anchorItem: root.anchorItem
    owner: root.barIdentity
    bar: root.bar
    open: root.opened
    focusTarget: keyCatcher
    contentWidth: panel.fittedContentWidth(Style.space(380))
    contentHeight: panel.fittedContentHeight(content.implicitHeight)

    PanelKeyCatcher {
      id: keyCatcher
      anchors.fill: parent

      onCloseRequested: root.close()
      onActivateRequested: root.chooseSelected()
      onTabRequested: function(direction) { root.switchPanel(direction) }
      onTextKey: function(t) { root.handleTextKey(t) }
      // Down walks the list; left and right have something real to do here, so
      // unlike a plain list they are bound rather than left alone.
      onMoveRequested: function(dx, dy) {
        if (dy !== 0) root.moveCursor(dy)
        else if (dx !== 0) root.adjustSelected(dx > 0 ? 1 : -1)
      }

      Column {
        id: content
        width: parent.width
        spacing: Style.space(10)

        // ---- Title, and whether the camera is held.
        Item {
          width: parent.width
          height: Math.max(titleRow.implicitHeight, statusPill.height)
          visible: !root.notInstalled

          Row {
            id: titleRow
            anchors.verticalCenter: parent.verticalCenter
            spacing: Style.space(7)

            Label {
              anchors.verticalCenter: parent.verticalCenter
              text: Model.GLYPH
              color: root.running ? Commons.Color.accent : root.dim
              font.pixelSize: Style.font.icon
            }
            Label {
              anchors.verticalCenter: parent.verticalCenter
              text: "Studio Effects"
              font.bold: true
              font.pixelSize: Style.font.subtitle
            }
          }

          // Not the blur radius: that has its own slider. This is the part
          // nothing else says. In the header rather than over the preview, so
          // nothing is drawn on the picture the call actually sees.
          Pill {
            anchors.right: statusPill.left
            anchors.rightMargin: Style.space(6)
            anchors.verticalCenter: parent.verticalCenter
            visible: root.running
            text: root.state.height + "p · " + root.state.device
          }

          // Live means the camera is open, which is the one thing a glance at
          // this panel should never get wrong.
          Rectangle {
            id: statusPill
            anchors.right: parent.right
            anchors.verticalCenter: parent.verticalCenter
            width: statusRow.implicitWidth + Style.space(14)
            height: statusText.implicitHeight + Style.space(6)
            radius: root.rounded ? height / 2 : 0
            color: root.running ? Util.alpha(Commons.Color.urgent, 0.16) : root.quietFill

            Row {
              id: statusRow
              anchors.centerIn: parent
              spacing: Style.space(5)

              Rectangle {
                visible: root.running
                anchors.verticalCenter: parent.verticalCenter
                width: Style.space(6)
                height: width
                radius: width / 2
                color: Commons.Color.urgent
              }
              Label {
                id: statusText
                text: root.switching ? "…" : root.running ? "LIVE" : "OFF"
                color: root.running ? Commons.Color.urgent : root.dim
                font.pixelSize: Style.font.caption
                font.bold: true
                font.letterSpacing: 1
              }
            }
          }
        }

        // ---- First run. No daemon: offer a release install or a source
        //      build. Both open in a terminal because installing asks for
        //      sudo, neither of which a widget inside the shell should do --
        //      and neither of which a person should have to take on trust from
        //      a window they cannot see.
        Column {
          width: parent.width
          visible: root.notInstalled
          spacing: Style.space(12)

          Rectangle {
            anchors.horizontalCenter: parent.horizontalCenter
            width: Style.space(52)
            height: width
            radius: root.rounded ? Style.space(14) : 0
            color: root.onFill

            Label {
              anchors.centerIn: parent
              text: Model.GLYPH
              color: root.surface
              font.pixelSize: Style.space(26)
            }
          }

          Column {
            width: parent.width
            spacing: Style.space(4)

            Label {
              width: parent.width
              horizontalAlignment: Text.AlignHCenter
              text: "Studio Effects"
              font.bold: true
              font.pixelSize: Style.font.heading
            }
            Label {
              width: parent.width
              horizontalAlignment: Text.AlignHCenter
              text: "The background service that runs the camera effects and Voice Focus "
                  + "is not installed yet. Choose how to install it."
              color: root.dim
              font.pixelSize: Style.font.bodySmall
              wrapMode: Text.WordWrap
            }
          }

          Card {
            height: setupList.implicitHeight + Style.space(8)

            Column {
              id: setupList
              x: Style.space(4)
              y: Style.space(4)
              width: parent.width - Style.space(8)
              spacing: Style.space(2)

              SetupRow {
                index: 0
                glyph: "\u{F01DA}"
                label: "Use prebuilt"
                selectable: root.prebuiltAvailable
                detail: Model.prebuiltHint(root.prebuiltStatus)
              }
              SetupRow {
                index: 1
                glyph: "\u{F05B7}"
                label: "Build from source"
                detail: "Built for your installed libraries. Downloads build tools and models; takes a few minutes."
              }
            }
          }

          Label {
            width: parent.width
            text: "Both open a terminal and ask for your password there. The camera "
                + "and microphone stay off until you turn them on."
            color: root.dim
            font.pixelSize: Style.font.caption
            wrapMode: Text.WordWrap
            leftPadding: Style.space(4)
            rightPadding: Style.space(4)
          }

          // Shown once it has been started. There is no handle on the
          // terminal, so it cannot say when the window closes -- only that
          // this panel will change by itself when the daemon appears, and that
          // asking again is safe if the window was closed early.
          Label {
            width: parent.width
            visible: root.settingUp
            text: "Working in the terminal window. This panel changes by itself when it "
                + "finishes; if you closed the window, set up again."
            color: root.accentColor
            font.pixelSize: Style.font.caption
            wrapMode: Text.WordWrap
            leftPadding: Style.space(4)
            rightPadding: Style.space(4)
          }
        }

        // ---- What the other end of the call actually sees. Reading the
        //      daemon's published JPEG rather than opening Studio Camera
        //      directly, because a second reader on that device invalidates
        //      the first one's buffers and would break the call this is
        //      previewing.
        Item {
          id: previewBox
          width: parent.width
          visible: root.running
          height: root.running ? Math.round(width * 9 / 16) : 0

          property int tick: 0
          // Which of the two images is the one being shown.
          property bool showingB: false
          // Whether a frame has ever arrived. The placeholder is for the wait
          // before the first one, not for the gap between every pair.
          property bool everReady: false
          property bool live: root.opened && root.previewLive
          property bool framePending: false

          function oversized(image) {
            return image.implicitWidth > 1.5 * previewBox.width * root.previewScale
          }

          function requestFrame() {
            if (!live) return
            framePending = true
            loadNextFrame()
          }

          // Leave the displayed image alone while decoding its replacement.
          // If frames arrive faster than decoding, remember only the newest
          // file instead of repeatedly cancelling a load that cannot finish.
          function loadNextFrame() {
            if (!live || !framePending) return
            var next = showingB ? previewA : previewB
            if (next.status === Image.Loading) return
            framePending = false
            tick++
            next.source = "file://" + root.previewPath + "?t=" + tick
          }

          // A daemon too old to know the `preview` command leaves the flag
          // false forever, and "starting preview…" then sits there implying
          // something is on its way that never is. After a grace period long
          // enough to cover the round trip, say what is actually true.
          property bool givenUp: false

          // A closed panel leaves nothing loaded, and the daemon deletes the
          // file, so the next open starts from the placeholder rather than
          // from a frame of a camera that may since have been turned off.
          onLiveChanged: {
            if (live) {
              previewBox.givenUp = false
            } else {
              previewBox.framePending = false
              previewA.source = ""
              previewB.source = ""
              previewBox.everReady = false
            }
          }

          // Restarted whenever the panel opens, so a daemon that starts
          // answering later is not written off from an earlier attempt.
          Timer {
            interval: 2500
            running: root.opened && root.running && !previewBox.live
            onTriggered: previewBox.givenUp = true
          }

          Rectangle {
            anchors.fill: parent
            color: Qt.darker(root.contentForeground, 8.0)
          }

          // Two images, loaded alternately. The one on screen is never touched
          // until its replacement has finished decoding, so there is no moment
          // where neither has a picture -- which is what made the preview
          // flicker, and what kept re-showing the placeholder on each frame.
          Image {
            id: previewA
            anchors.fill: parent
            fillMode: Image.PreserveAspectCrop
            cache: false
            asynchronous: true
            smooth: true
            // The daemon encodes at the box's physical size, which is drawn 1:1
            // and wants no mipmaps: they would blend in a half-size level and
            // soften it. Only a frame much bigger than the box -- a daemon too
            // old to take the size -- is reduced through them, so fine hair and
            // fabric do not alias.
            mipmap: previewBox.oversized(previewA)
            opacity: previewBox.showingB ? 0 : 1
            onStatusChanged: if (status === Image.Ready) {
              previewBox.showingB = false
              previewBox.everReady = true
              previewBox.loadNextFrame()
            }
          }

          Image {
            id: previewB
            anchors.fill: parent
            fillMode: Image.PreserveAspectCrop
            cache: false
            asynchronous: true
            smooth: true
            mipmap: previewBox.oversized(previewB)
            opacity: previewBox.showingB ? 1 : 0
            onStatusChanged: if (status === Image.Ready) {
              previewBox.showingB = true
              previewBox.everReady = true
              previewBox.loadNextFrame()
            }
          }

          Label {
            anchors.centerIn: parent
            width: parent.width - Style.space(24)
            horizontalAlignment: Text.AlignHCenter
            visible: !previewBox.everReady
            text: previewBox.givenUp
                ? "No preview from the daemon.\nIt may be older than this widget."
                : "starting preview…"
            color: root.dim
            font.pixelSize: Style.font.caption
            wrapMode: Text.WordWrap
          }

          // Rounded corners without re-rendering the frame through a layer:
          // the panel's own color is painted over the corners instead, so the
          // preview keeps its 1:1 physical pixels. A see-through panel would
          // show the paint, so there the corners stay square.
          Item {
            visible: root.cardRadius > 0 && root.surface.a > 0.99
            anchors.fill: parent
            clip: true

            Rectangle {
              anchors.fill: parent
              anchors.margins: -root.cardRadius
              radius: root.cardRadius * 2
              color: "transparent"
              border.width: root.cardRadius
              border.color: root.surface
            }
          }

          // Atomic JPEG replacements drive the display directly, at the
          // camera's delivered cadence. No polling timer and no repeated
          // decoding of an unchanged file. The Image reads the JPEG itself;
          // FileView only watches it, including creation after camera restart.
          FileView {
            path: previewBox.live ? root.previewPath : ""
            preload: false
            watchChanges: previewBox.live
            onFileChanged: Qt.callLater(previewBox.requestFrame)
          }
        }

        // ---- Off. Said as status, not as a caution: an earlier wording
        //      spelled out that turning the camera on would light the
        //      recording LED, which is both obvious and alarming to read while
        //      everything is switched off.
        Card {
          visible: !root.running && !root.notInstalled
          height: Math.round(width * 9 / 16)

          Column {
            anchors.centerIn: parent
            width: parent.width - Style.space(40)
            spacing: Style.space(10)

            Disc {
              anchors.horizontalCenter: parent.horizontalCenter
              size: Style.space(52)
              glyph: "\u{F0568}"

              MouseArea {
                anchors.fill: parent
                cursorShape: Qt.PointingHandCursor
                onClicked: root.togglePower()
              }
            }
            Label {
              width: parent.width
              horizontalAlignment: Text.AlignHCenter
              text: root.switching ? "Starting the camera…" : "Camera off"
              font.bold: true
              font.pixelSize: Style.font.subtitle
            }
            Label {
              width: parent.width
              horizontalAlignment: Text.AlignHCenter
              text: "The camera is closed and the NPU is idle."
              color: root.dim
              font.pixelSize: Style.font.bodySmall
              wrapMode: Text.WordWrap
            }
          }
        }

        // ---- The two switches. Off is not "effects disabled" but "the camera
        //      is released": while the daemon runs it holds the camera open,
        //      the recording light stays lit, and nothing else can open the
        //      real camera. Voice Focus is a peer of it, not one of the
        //      effects: it filters the microphone, works with the camera off,
        //      and is a separate unit that fails on its own.
        Row {
          width: parent.width
          visible: !root.notInstalled
          spacing: Style.space(10)

          PowerTile {
            width: (parent.width - parent.spacing) / 2
            label: "Camera"
            glyph: root.running ? "\u{F0567}" : "\u{F0568}"
            on: root.running
            // Saying so while it happens matters here: starting the daemon
            // takes a moment to open the camera, and a button that looks
            // inert gets pressed again.
            status: root.switching ? "…" : Model.powerLabel(root.state)
            onActivated: root.togglePower()
          }
          PowerTile {
            width: (parent.width - parent.spacing) / 2
            label: "Voice Focus"
            glyph: root.voice === "on" ? "\u{F036C}" : "\u{F036D}"
            on: root.voice === "on"
            unavailable: root.voice === "missing"
            // "Not installed" rather than "Off": one is a switch you can flip,
            // the other is a package that is not there, and offering to toggle
            // something that cannot start is how a control loses trust.
            status: root.voiceSwitching ? "…"
                  : root.voice === "on" ? "On"
                  : root.voice === "off" ? "Off"
                  : "Not installed"
            onActivated: root.toggleVoice()
          }
        }

        // A power change systemd accepted but the daemon did not follow. Shown
        // rather than swallowed: the alternative is a switch that appears to do
        // nothing, which sends anyone looking straight at the widget.
        Label {
          width: parent.width
          visible: root.powerNote.length > 0
          text: root.powerNote
          color: root.accentColor
          font.pixelSize: Style.font.caption
          wrapMode: Text.WordWrap
          leftPadding: Style.space(4)
          rightPadding: Style.space(4)
        }

        // ---- The effects, as one row of buttons.
        Card {
          visible: root.running && root.effectTiles.length > 0
          height: effectRow.height + Style.space(8)

          Row {
            id: effectRow
            x: Style.space(4)
            y: Style.space(4)
            width: parent.width - Style.space(8)

            Repeater {
              model: root.effectTiles
              delegate: EffectTile {
                width: effectRow.width / Math.max(1, root.effectTiles.length)
              }
            }
          }
        }

        // How to make the dimmed effect real. Under the row rather than in it:
        // the instruction names a file and does not fit in a button.
        Label {
          width: parent.width
          visible: root.running && root.hintSpec !== null
          text: root.hintSpec ? root.hintSpec.detail : ""
          color: root.dim
          font.pixelSize: Style.font.caption
          wrapMode: Text.WordWrap
          leftPadding: Style.space(4)
          rightPadding: Style.space(4)
        }

        // ---- Switches and choices that govern every effect.
        Card {
          visible: root.running && root.listRows.length > 0
          height: settingsList.implicitHeight + Style.space(8)

          Column {
            id: settingsList
            x: Style.space(4)
            y: Style.space(4)
            width: parent.width - Style.space(8)

            Repeater {
              model: root.listRows
              delegate: ListRow {}
            }
          }
        }

        // ---- The settings the chosen effect uses.
        Column {
          width: parent.width
          visible: root.running && root.paramRows.length > 0
          spacing: Style.space(2)

          Repeater {
            model: root.paramRows
            delegate: ParamBlock {}
          }
        }

        // The daemon's last refusal, if it made one. Shown rather than
        // swallowed, because the alternative is a choice that silently did
        // not take.
        Label {
          width: parent.width
          visible: root.running && root.state.error.length > 0
          text: root.state.error
          color: root.accentColor
          font.pixelSize: Style.font.caption
          wrapMode: Text.WordWrap
          leftPadding: Style.space(4)
          rightPadding: Style.space(4)
        }

        PanelSeparator {
          width: parent.width
          foreground: root.contentForeground
          strength: 0.08
        }

        Flow {
          width: parent.width
          spacing: Style.space(10)

          Repeater {
            model: root.keyHints
            delegate: KeyHint {}
          }
        }
      }
    }
  }
}
