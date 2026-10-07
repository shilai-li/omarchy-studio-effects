import QtQuick
import Quickshell.Io
import qs.Commons
import qs.Ui
import "Model.js" as Model

// The effect list: off, blurred, and replaced when an image is loaded.
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

  property int selectedIndex: 0
  property int setupIndex: 1
  property bool cursorActive: false

  readonly property string previewPath: root.state.previewPath || ""
  readonly property bool previewLive: root.running && root.state.preview && previewPath.length > 0

  // ---- Theme. Nothing here names a color; the palette does.
  readonly property color contentForeground: bar ? bar.foreground : Color.foreground
  readonly property string contentFontFamily: bar ? bar.fontFamily : Style.font.family
  readonly property color dim: Qt.darker(contentForeground, 1.5)
  readonly property color accentColor: Style.selectedStateColor(contentForeground, Color.accent)
  readonly property color hoverFill: Style.hoverFillFor(contentForeground, Color.accent)
  readonly property color selectedFill: Style.selectedFillFor(contentForeground, Color.accent)

  readonly property int rowHeight: Math.max(Style.space(24), Style.font.body + Style.space(12))

  // ---- Lifecycle.
  function open() {
    root.cursorActive = true
    // Turning the preview on doubles as the refresh: every reply carries the
    // daemon's whole state. Asking for both would be one request too many --
    // only one command is in flight at a time, so the second would be dropped.
    if (root.host) root.host.setPreview(true)
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

  // Walks past hints: they are there to be read, and a cursor that could stop on
  // one would show a highlight on a row that enter does nothing on.
  function moveCursor(delta) {
    if (root.notInstalled) {
      root.cursorActive = true
      root.setupIndex = root.prebuiltAvailable ? 1 - root.setupIndex : 1
      return
    }
    if (root.rows.length === 0) return
    root.cursorActive = true
    root.selectedIndex = Model.nextSelectable(root.rows, root.selectedIndex, delta < 0 ? -1 : 1)
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

  // Left and right adjust whatever row the cursor is on, and nothing at all on
  // an effect row. Having them fall back to some other setting would mean the
  // same keypress did different things depending on where you were, without
  // saying which.
  function adjustSelected(direction) {
    if (!root.host || !root.currentRow) return
    if (root.currentRow.kind === "param") {
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

  // The preview is asked for on open, but a daemon can arrive long after that:
  // pressing `c` stops and restarts it underneath a panel that stays open the
  // whole time, so open() never runs again and nothing would re-request the
  // frames. Watch the daemon coming back instead of the panel opening.
  onRunningChanged: {
    previewBox.givenUp = false
    if (root.running && root.opened && root.host) root.host.setPreview(true)
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

  // ---- One row. Either an effect to choose or a setting to adjust; they share
  //      a component so the cursor walks one list rather than two.
  component SetupRow: CursorSurface {
    id: setup
    required property int index
    required property string label
    required property string detail
    property bool selectable: true
    enabled: setup.selectable
    visible: root.notInstalled
    width: parent ? parent.width : 0
    height: root.rowHeight + setupDetail.implicitHeight + Style.space(6)
    hasCursor: setup.selectable && root.cursorActive && root.setupIndex === setup.index
    foreground: root.contentForeground
    accent: Color.accent
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

    Text {
      x: Style.space(8)
      y: Math.round((root.rowHeight - height) / 2)
      width: parent.width - Style.space(64)
      textFormat: Text.PlainText
      text: setup.label
      color: setup.selectable ? root.contentForeground : root.dim
      font.family: root.contentFontFamily
      font.pixelSize: Style.font.body
      elide: Text.ElideRight
    }
    Text {
      anchors.right: parent.right
      anchors.rightMargin: Style.space(8)
      y: Math.round((root.rowHeight - height) / 2)
      textFormat: Text.PlainText
      text: setup.hasCursor ? "enter" : ""
      color: root.accentColor
      font.family: root.contentFontFamily
      font.pixelSize: Style.font.caption
    }
    Text {
      id: setupDetail
      x: Style.space(8)
      y: root.rowHeight
      width: parent.width - Style.space(16)
      textFormat: Text.PlainText
      text: setup.detail
      color: root.dim
      font.family: root.contentFontFamily
      font.pixelSize: Style.font.caption
      wrapMode: Text.WordWrap
    }
  }

  component EffectRow: CursorSurface {
    id: row

    required property int index
    required property var modelData

    readonly property bool isParam: modelData && modelData.kind === "param"
    readonly property bool isToggle: modelData && modelData.kind === "toggle"
    readonly property bool isChoice: modelData && modelData.kind === "choice"
    readonly property bool isHint: modelData && modelData.kind === "hint"
    readonly property var hintSpec: row.isHint ? Model.hintFor(modelData.key) : null
    readonly property var toggleSpec: row.isToggle ? Model.toggleFor(modelData.key) : null
    readonly property var choiceSpec: row.isChoice ? Model.choiceFor(modelData.key) : null
    readonly property string effect: modelData && modelData.effect ? modelData.effect : ""
    readonly property string paramKey: modelData && modelData.key ? modelData.key : ""
    readonly property var spec: row.isParam ? Model.paramFor(row.paramKey) : null
    readonly property bool isCurrent: !row.isParam && !row.isHint && root.running && root.state.effect === effect

    // First row of a new kind. Drawn inside the row rather than between rows so
    // grouping costs no height in a panel that is already tall.
    readonly property bool startsGroup: index > 0
      && root.rows[index - 1] && root.rows[index - 1].kind !== modelData.kind
      // The hint stands in for an effect, so it belongs with them.
      && !(row.isHint && root.rows[index - 1].kind === "effect")

    width: parent ? parent.width : 0
    // A hint is a row and a line of explanation, so it is taller than the rest.
    height: row.isHint ? root.rowHeight - Style.space(4) + hintDetail.implicitHeight + Style.space(6)
                       : root.rowHeight
    hasCursor: !row.isHint && root.cursorActive && root.selectedIndex === index
    foreground: root.contentForeground
    accent: Color.accent
    fill: root.hoverFill
    currentFill: root.selectedFill

    Rectangle {
      anchors.left: parent.left
      anchors.right: parent.right
      anchors.top: parent.top
      anchors.leftMargin: Style.space(8)
      anchors.rightMargin: Style.space(8)
      height: 1
      visible: row.startsGroup
      color: root.contentForeground
      opacity: 0.12
    }

    MouseArea {
      anchors.fill: parent
      // Nothing to hover or click on a hint, and the pointer should not say so.
      enabled: !row.isHint
      hoverEnabled: true
      cursorShape: Qt.PointingHandCursor

      onContainsMouseChanged: if (containsMouse) {
        root.cursorActive = true
        root.selectedIndex = row.index
      }

      // A setting is adjusted, not chosen, so clicking one of those halves
      // steps it rather than doing nothing.
      onClicked: function (mouse) {
        root.selectedIndex = row.index
        if (row.isParam || row.isChoice) root.adjustSelected(mouse.x > row.width / 2 ? 1 : -1)
        else root.chooseSelected()
      }
    }

    Text {
      id: marker
      anchors.left: parent.left
      anchors.leftMargin: Style.space(8)
      anchors.verticalCenter: parent.verticalCenter
      width: Style.space(14)
      textFormat: Text.PlainText
      text: row.isCurrent ? "\u{F012C}" : ""
      color: root.accentColor
      font.family: root.contentFontFamily
      font.pixelSize: Style.font.bodySmall
    }

    Text {
      id: rowLabel
      anchors.left: marker.right
      anchors.leftMargin: Style.space(4)
      anchors.verticalCenter: row.isHint ? undefined : parent.verticalCenter
      y: row.isHint ? Math.round((root.rowHeight - height) / 2) : 0
      textFormat: Text.PlainText
      text: row.isHint ? (row.hintSpec ? row.hintSpec.label : "")
          : row.isParam ? (row.spec ? row.spec.label : row.paramKey)
          : row.isChoice ? (row.choiceSpec ? row.choiceSpec.label : row.paramKey)
          : row.isToggle ? (row.toggleSpec ? row.toggleSpec.label : row.paramKey)
          : row.effect === "none" ? "No effect"
          : row.effect === "blur" ? "Blur background"
          : "Replace background"
      // Dimmed like anything unavailable: it is not a choice yet.
      color: row.isHint ? root.dim : root.contentForeground
      font.family: root.contentFontFamily
      font.pixelSize: Style.font.body
      elide: Text.ElideRight
    }

    // How to make the row real. Under the label rather than beside it: the
    // instruction names a file and does not fit on one line of a narrow panel.
    Text {
      id: hintDetail
      visible: row.isHint
      anchors.left: rowLabel.left
      anchors.right: parent.right
      anchors.rightMargin: Style.space(8)
      y: root.rowHeight - Style.space(4)
      textFormat: Text.PlainText
      text: row.hintSpec ? row.hintSpec.detail : ""
      color: root.dim
      font.family: root.contentFontFamily
      font.pixelSize: Style.font.caption
      wrapMode: Text.WordWrap
    }

    // The value. Sliders get arrows on the selected row so it is discoverable
    // that they are adjusted rather than chosen; a toggle just reads On or Off.
    Text {
      anchors.right: parent.right
      anchors.rightMargin: Style.space(8)
      anchors.verticalCenter: parent.verticalCenter
      visible: row.isParam || row.isToggle || row.isChoice
      textFormat: Text.PlainText
      text: {
        if (row.isToggle) return Model.toggleValue(root.state, row.paramKey) ? "On" : "Off"
        // The daemon's own name for it, not a prettier one: it is what the
        // config file takes and what `studio-effects model` takes, and a
        // second name for the same thing is a thing to get wrong.
        var v = row.isChoice ? Model.choiceValue(root.state, row.paramKey)
              : row.isParam ? Model.paramDisplayValue(root.state, row.paramKey)
              : ""
        if (!row.isParam && !row.isChoice) return ""
        return row.hasCursor ? "\u2039 " + v + " \u203a" : String(v)
      }
      color: row.isToggle && Model.toggleValue(root.state, row.paramKey) ? root.accentColor
           : row.hasCursor ? root.accentColor : root.dim
      font.family: root.contentFontFamily
      font.pixelSize: Style.font.body
      font.bold: row.isToggle
    }
  }

  KeyboardPanel {
    id: panel
    anchorItem: root.anchorItem
    owner: root.barIdentity
    bar: root.bar
    open: root.opened
    focusTarget: keyCatcher
    contentWidth: panel.fittedContentWidth(Style.space(360))
    contentHeight: panel.fittedContentHeight(rowColumn.implicitHeight)

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
        id: rowColumn
        width: parent.width
        spacing: Style.space(6)

        Text {
          width: parent.width
          textFormat: Text.PlainText
          text: "Studio Effects"
          color: root.dim
          font.family: root.contentFontFamily
          font.pixelSize: Style.font.caption
          font.letterSpacing: 1
          font.bold: true
        }

        PanelSeparator {
          width: parent.width
          foreground: root.contentForeground
        }

        // No daemon: offer a release install or a source build. Both open in a
        // terminal because installing asks for sudo, neither of
        // which a widget inside the shell should do -- and neither of which a
        // person should have to take on trust from a window they cannot see.
        Text {
          width: parent.width
          visible: root.notInstalled
          textFormat: Text.PlainText
          text: "The Studio Effects daemon is not installed yet.\n\n"
              + "Choose how to install it. Both options open a terminal and ask "
              + "for your password before installation. The camera and microphone "
              + "stay off until you turn them on."
          color: root.contentForeground
          font.family: root.contentFontFamily
          font.pixelSize: Style.font.caption
          wrapMode: Text.WordWrap
          leftPadding: Style.space(8)
          rightPadding: Style.space(8)
          bottomPadding: Style.space(6)
        }

        SetupRow {
          index: 0
          label: "Use prebuilt"
          selectable: root.prebuiltAvailable
          detail: Model.prebuiltHint(root.prebuiltStatus)
        }
        SetupRow {
          index: 1
          label: "Build from source"
          detail: "Build for your installed libraries.\nDownloads build tools and models; takes a few minutes."
        }

        // Shown once it has been started. There is no handle on the terminal, so
        // it cannot say when the window closes -- only that this panel will
        // change by itself when the daemon appears, and that asking again is
        // safe if the window was closed early.
        Text {
          width: parent.width
          visible: root.notInstalled && root.settingUp
          textFormat: Text.PlainText
          text: "Working in the terminal window. This panel changes by itself when it "
              + "finishes; if you closed the window, set up again."
          color: root.dim
          font.family: root.contentFontFamily
          font.pixelSize: Style.font.caption
          wrapMode: Text.WordWrap
          leftPadding: Style.space(8)
          rightPadding: Style.space(8)
          topPadding: Style.space(4)
        }

        // The real switch. Off is not "effects disabled" but "the camera is
        // released": while the daemon runs it holds the camera open, the
        // recording light stays lit, and nothing else can open the real
        // camera. That is worth a row of its own above the effects, not a
        // choice buried among them.
        CursorSurface {
          id: powerRow
          visible: !root.notInstalled
          width: parent.width
          height: root.rowHeight
          hasCursor: false
          foreground: root.contentForeground
          accent: Color.accent
          fill: root.hoverFill
          currentFill: root.selectedFill

          MouseArea {
            anchors.fill: parent
            hoverEnabled: true
            cursorShape: Qt.PointingHandCursor
            onClicked: root.togglePower()
          }

          Text {
            anchors.left: parent.left
            anchors.leftMargin: Style.space(8)
            anchors.verticalCenter: parent.verticalCenter
            textFormat: Text.PlainText
            // "Camera", not "Studio Effects": that is the panel's name, and
            // repeating it here cost a row and left two unrelated "Off"s on
            // screen at once -- this switch releasing the camera, and the
            // background effect being none.
            text: "Camera"
            color: root.contentForeground
            font.family: root.contentFontFamily
            font.pixelSize: Style.font.body
          }

          Text {
            anchors.right: parent.right
            anchors.rightMargin: Style.space(8)
            anchors.verticalCenter: parent.verticalCenter
            textFormat: Text.PlainText
            // Saying so while it happens matters here: starting the daemon
            // takes a moment to open the camera, and a button that looks
            // inert gets pressed again.
            text: root.switching ? "…" : Model.powerLabel(root.state)
            color: root.running ? root.accentColor : root.dim
            font.family: root.contentFontFamily
            font.pixelSize: Style.font.body
            font.bold: true
          }
        }

        // A power change systemd accepted but the daemon did not follow. Shown
        // rather than swallowed: the alternative is a switch that appears to do
        // nothing, which sends anyone looking straight at the widget.
        Text {
          width: parent.width
          visible: root.powerNote.length > 0
          textFormat: Text.PlainText
          text: root.powerNote
          color: root.accentColor
          font.family: root.contentFontFamily
          font.pixelSize: Style.font.caption
          wrapMode: Text.WordWrap
          leftPadding: Style.space(8)
          rightPadding: Style.space(8)
          bottomPadding: Style.space(4)
        }

        // A peer of the power switch, not one of the effects: it filters the
        // microphone, works with the camera off, and is a separate unit that
        // fails on its own.
        CursorSurface {
          id: voiceRow
          visible: !root.notInstalled
          width: parent.width
          height: root.rowHeight
          hasCursor: false
          foreground: root.contentForeground
          accent: Color.accent
          fill: root.hoverFill
          currentFill: root.selectedFill

          MouseArea {
            anchors.fill: parent
            hoverEnabled: true
            cursorShape: root.voice === "missing" ? Qt.ArrowCursor : Qt.PointingHandCursor
            onClicked: root.toggleVoice()
          }

          Text {
            anchors.left: parent.left
            anchors.leftMargin: Style.space(8)
            anchors.verticalCenter: parent.verticalCenter
            textFormat: Text.PlainText
            text: "Voice Focus"
            color: root.voice === "missing" ? root.dim : root.contentForeground
            font.family: root.contentFontFamily
            font.pixelSize: Style.font.body
          }

          Text {
            anchors.right: parent.right
            anchors.rightMargin: Style.space(8)
            anchors.verticalCenter: parent.verticalCenter
            textFormat: Text.PlainText
            // "Missing" rather than "Off": one is a switch you can flip, the
            // other is a package that is not installed, and offering to toggle
            // something that cannot start is how a control loses trust.
            text: root.voiceSwitching ? "…"
                : root.voice === "on" ? "On"
                : root.voice === "off" ? "Off"
                : "not installed"
            color: root.voice === "on" ? root.accentColor : root.dim
            font.family: root.contentFontFamily
            font.pixelSize: root.voice === "missing" ? Style.font.caption : Style.font.body
            font.bold: root.voice !== "missing"
          }
        }

        Text {
          width: parent.width
          visible: !root.running && !root.switching && !root.notInstalled
          textFormat: Text.PlainText
          // A status line, not a caution. The earlier wording spelled out that
          // turning the camera on would light the recording LED, which is both
          // obvious and alarming to read while everything is switched off --
          // it was taken as a warning that something was wrong.
          text: "Nothing is running. The camera is closed and the NPU is idle."
          color: root.dim
          font.family: root.contentFontFamily
          font.pixelSize: Style.font.caption
          wrapMode: Text.WordWrap
          leftPadding: Style.space(8)
          rightPadding: Style.space(8)
          bottomPadding: Style.space(6)
        }

        PanelSeparator {
          width: parent.width
          visible: root.running
          foreground: root.contentForeground
        }

        // What the other end of the call actually sees. Reading the daemon's
        // published JPEG rather than opening Studio Camera directly, because a
        // second reader on that device invalidates the first one's buffers and
        // would break the call this is previewing.
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
            radius: Style.space(4)
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
            // The daemon supplies enough pixels for high-DPI screens. Filter
            // the reduction instead of aliasing fine hair and fabric detail.
            mipmap: true
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
            mipmap: true
            opacity: previewBox.showingB ? 1 : 0
            onStatusChanged: if (status === Image.Ready) {
              previewBox.showingB = true
              previewBox.everReady = true
              previewBox.loadNextFrame()
            }
          }

          Text {
            anchors.centerIn: parent
            width: parent.width - Style.space(24)
            horizontalAlignment: Text.AlignHCenter
            visible: !previewBox.everReady
            textFormat: Text.PlainText
            text: previewBox.givenUp
                ? "No preview from the daemon.\nIt may be older than this widget."
                : "starting preview…"
            color: root.dim
            font.family: root.contentFontFamily
            font.pixelSize: Style.font.caption
            wrapMode: Text.WordWrap
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

        Column {
          width: parent.width
          visible: root.running

          Repeater {
            model: root.rows
            delegate: EffectRow {}
          }
        }

        // The daemon's last refusal, if it made one. Shown rather than
        // swallowed, because the alternative is a choice that silently did
        // not take.
        Text {
          width: parent.width
          visible: root.running && root.state.error.length > 0
          textFormat: Text.PlainText
          text: root.state.error
          color: root.accentColor
          font.family: root.contentFontFamily
          font.pixelSize: Style.font.caption
          wrapMode: Text.WordWrap
          topPadding: Style.space(4)
        }

        PanelSeparator {
          width: parent.width
          foreground: root.contentForeground
        }

        Text {
          width: parent.width
          visible: root.running
          textFormat: Text.PlainText
          // Not the blur radius: that has its own row two lines up. This is
          // the part nothing else says.
          text: root.state.device + "   ·   " + root.state.width + "x" + root.state.height
          color: root.dim
          font.family: root.contentFontFamily
          font.pixelSize: Style.font.caption
        }

        Text {
          width: parent.width
          textFormat: Text.PlainText
          text: root.notInstalled ? "enter set up   esc close"
              : !root.running ? "c camera   v voice   esc close"
              : root.currentRow && root.currentRow.kind === "param"
              ? "↑↓ move   ←→ adjust   c off   v voice   esc close"
              : root.currentRow && root.currentRow.kind === "choice"
              ? "↑↓ move   ←→ switch   c off   v voice   esc close"
              : root.currentRow && root.currentRow.kind === "toggle"
              ? "↑↓ move   enter switch   c off   v voice   esc close"
              : "↑↓ move   enter choose   c off   v voice   esc close"
          color: root.dim
          font.family: root.contentFontFamily
          font.pixelSize: Style.font.caption
          wrapMode: Text.WordWrap
        }
      }
    }
  }
}
