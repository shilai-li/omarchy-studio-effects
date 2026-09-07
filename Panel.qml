import QtQuick
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
  readonly property var rows: Model.panelRows(root.state)
  readonly property var currentRow: selectedIndex >= 0 && selectedIndex < rows.length
    ? rows[selectedIndex] : null

  property int selectedIndex: 0
  property bool cursorActive: false

  // Bumped on a timer to re-read the preview file. The daemon rewrites it in
  // place, and an Image will not notice a file changing underneath a URL it
  // has already loaded.
  property int previewTick: 0
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
    // Voice focus is a different unit with a life of its own -- a keybinding or
    // systemctl may have changed it since the last poll.
    if (root.host) root.host.readVoice()
    root.syncCursorToEffect()
    root.controller.show()
    Qt.callLater(function() {
      if (root.opened) root.setCenterHoverRevealSuppressed(true)
    })
  }

  function close() {
    // Stop the encoder on the way out, not on the way in to the next open: a
    // panel closed on one monitor should cost nothing, and the daemon has no
    // other way to know nobody is watching.
    if (root.host) root.host.setPreview(false)
    root.setCenterHoverRevealSuppressed(false)
    root.controller.hide()
  }

  function toggle() { root.opened ? root.close() : root.open() }

  function switchPanel(direction) {
    if (root.bar && typeof root.bar.switchPanelFrom === "function")
      return root.bar.switchPanelFrom(root.barIdentity, direction)
    return false
  }

  // Summoning by hotkey moves no pointer, so a hover the bar was still holding
  // must not keep the center indicators revealed behind the panel.
  function setCenterHoverRevealSuppressed(value) {
    if (root.bar && "centerHoverRevealSuppressed" in root.bar)
      root.bar.centerHoverRevealSuppressed = value
  }

  // ---- Cursor. It follows the daemon rather than staying where it was: the
  //      row that is on is the one worth landing on.
  function syncCursorToEffect() {
    root.selectedIndex = Model.indexOfEffect(root.state, root.rows)
  }

  function moveCursor(delta) {
    if (root.rows.length === 0) return
    root.cursorActive = true
    var next = (root.selectedIndex + delta) % root.rows.length
    root.selectedIndex = next < 0 ? next + root.rows.length : next
  }

  // ---- Actions. Choosing an effect leaves the panel up: the point is to see
  //      the change land, and the next thing a user does is often adjust it.
  function chooseSelected() {
    if (!root.host || !root.currentRow) return
    if (root.currentRow.kind === "effect") {
      root.host.setEffect(root.currentRow.effect)
    } else if (root.currentRow.kind === "toggle") {
      root.host.setToggle(root.currentRow.key,
                          !Model.toggleValue(root.state, root.currentRow.key))
    }
  }

  // Left and right adjust whatever row the cursor is on, and nothing at all on
  // an effect row. Having them fall back to some other setting would mean the
  // same keypress did different things depending on where you were, without
  // saying which.
  function adjustSelected(direction) {
    if (!root.host || !root.currentRow || root.currentRow.kind !== "param") return
    root.host.setParam(root.currentRow.key,
                       Model.stepParam(root.state, root.currentRow.key, direction))
  }

  function togglePower() {
    if (root.host) root.host.toggleService()
  }

  function toggleVoice() {
    if (root.host) root.host.toggleVoice()
  }

  function handleTextKey(text) {
    var key = String(text).toLowerCase()
    if (key === "r" && root.host) root.host.refresh()
    else if (key === "f" && root.host) root.host.toggle()
    else if (key === "p") root.togglePower()
    else if (key === "v") root.toggleVoice()
  }

  // The preview is asked for on open, but a daemon can arrive long after that:
  // pressing `p` stops and restarts it underneath a panel that stays open the
  // whole time, so open() never runs again and nothing would re-request the
  // frames. Watch the daemon coming back instead of the panel opening.
  onRunningChanged: {
    previewBox.givenUp = false
    if (root.running && root.opened && root.host) root.host.setPreview(true)
  }

  // `replace` disappearing — the daemon restarted without a background — can
  // leave the cursor past the end of the list.
  onRowsChanged: {
    if (root.selectedIndex >= root.rows.length)
      root.selectedIndex = Math.max(0, root.rows.length - 1)
  }

  // ---- One row. Either an effect to choose or a setting to adjust; they share
  //      a component so the cursor walks one list rather than two.
  component EffectRow: CursorSurface {
    id: row

    required property int index
    required property var modelData

    readonly property bool isParam: modelData && modelData.kind === "param"
    readonly property bool isToggle: modelData && modelData.kind === "toggle"
    readonly property var toggleSpec: row.isToggle ? Model.toggleFor(modelData.key) : null
    readonly property string effect: modelData && modelData.effect ? modelData.effect : ""
    readonly property string paramKey: modelData && modelData.key ? modelData.key : ""
    readonly property var spec: row.isParam ? Model.paramFor(row.paramKey) : null
    readonly property bool isCurrent: !row.isParam && root.running && root.state.effect === effect

    width: parent ? parent.width : 0
    height: root.rowHeight
    hasCursor: root.cursorActive && root.selectedIndex === index
    foreground: root.contentForeground
    accent: Color.accent
    fill: root.hoverFill
    currentFill: root.selectedFill

    MouseArea {
      anchors.fill: parent
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
        if (row.isParam) root.adjustSelected(mouse.x > row.width / 2 ? 1 : -1)
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
      anchors.verticalCenter: parent.verticalCenter
      textFormat: Text.PlainText
      text: row.isParam ? (row.spec ? row.spec.label : row.paramKey)
          : row.isToggle ? (row.toggleSpec ? row.toggleSpec.label : row.paramKey)
          : row.effect === "none" ? "Off"
          : row.effect === "blur" ? "Blur background"
          : "Replace background"
      color: root.contentForeground
      font.family: root.contentFontFamily
      font.pixelSize: Style.font.body
      elide: Text.ElideRight
    }

    // The value. Sliders get arrows on the selected row so it is discoverable
    // that they are adjusted rather than chosen; a toggle just reads On or Off.
    Text {
      anchors.right: parent.right
      anchors.rightMargin: Style.space(8)
      anchors.verticalCenter: parent.verticalCenter
      visible: row.isParam || row.isToggle
      textFormat: Text.PlainText
      text: {
        if (row.isToggle) return Model.toggleValue(root.state, row.paramKey) ? "On" : "Off"
        if (!row.isParam) return ""
        var v = Model.paramValue(root.state, row.paramKey)
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

        // The real switch. Off is not "effects disabled" but "the camera is
        // released": while the daemon runs it holds the camera open, the
        // recording light stays lit, and nothing else can open the real
        // camera. That is worth a row of its own above the effects, not a
        // choice buried among them.
        CursorSurface {
          id: powerRow
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
            text: "Studio Effects"
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

        // A peer of the power switch, not one of the effects: it filters the
        // microphone, works with the camera off, and is a separate unit that
        // fails on its own.
        CursorSurface {
          id: voiceRow
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

        Text {
          width: parent.width
          visible: !root.running && !root.switching
          textFormat: Text.PlainText
          text: "The camera is released and the NPU is idle. Turning this on "
              + "opens your camera; the recording light will come on."
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
          // flicker, and what kept re-showing the placeholder ten times a
          // second.
          Image {
            id: previewA
            anchors.fill: parent
            fillMode: Image.PreserveAspectCrop
            cache: false
            asynchronous: true
            smooth: true
            opacity: previewBox.showingB ? 0 : 1
            onStatusChanged: if (status === Image.Ready) {
              previewBox.showingB = false
              previewBox.everReady = true
            }
          }

          Image {
            id: previewB
            anchors.fill: parent
            fillMode: Image.PreserveAspectCrop
            cache: false
            asynchronous: true
            smooth: true
            opacity: previewBox.showingB ? 1 : 0
            onStatusChanged: if (status === Image.Ready) {
              previewBox.showingB = true
              previewBox.everReady = true
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

          // The daemon rewrites the file in place, and an Image will not notice
          // a file changing under a URL it has already loaded. A query string
          // makes each read a new URL; QUrl drops it when resolving a file:
          // path, so the same file is what actually gets opened.
          Timer {
            interval: Model.PREVIEW_INTERVAL_MS
            running: previewBox.live
            repeat: true
            onTriggered: {
              previewBox.tick++
              var url = "file://" + root.previewPath + "?t=" + previewBox.tick
              if (previewBox.showingB) previewA.source = url
              else previewB.source = url
            }
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
          text: "blur " + root.state.blur + "   ·   " + root.state.device
          color: root.dim
          font.family: root.contentFontFamily
          font.pixelSize: Style.font.caption
        }

        Text {
          width: parent.width
          textFormat: Text.PlainText
          text: !root.running ? "p camera   v voice   esc close"
              : root.currentRow && root.currentRow.kind === "param"
              ? "↑↓ move   ←→ adjust   p off   v voice   esc close"
              : root.currentRow && root.currentRow.kind === "toggle"
              ? "↑↓ move   enter switch   p off   v voice   esc close"
              : "↑↓ move   enter choose   p off   v voice   esc close"
          color: root.dim
          font.family: root.contentFontFamily
          font.pixelSize: Style.font.caption
          wrapMode: Text.WordWrap
        }
      }
    }
  }
}
