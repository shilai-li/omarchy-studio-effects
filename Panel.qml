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
  readonly property var rows: Model.availableEffects(root.state)

  property int selectedIndex: 0
  property bool cursorActive: false

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
    // The daemon may have been changed by the CLI or a keybinding since the
    // glyph last polled, so this is the one moment it has to be right.
    if (root.host) root.host.refresh()
    root.syncCursorToEffect()
    root.controller.show()
    Qt.callLater(function() {
      if (root.opened) root.setCenterHoverRevealSuppressed(true)
    })
  }

  function close() {
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
    if (!root.host || root.selectedIndex >= root.rows.length) return
    root.host.setEffect(root.rows[root.selectedIndex])
  }

  function togglePower() {
    if (root.host) root.host.toggleService()
  }

  function stepBlur(direction) {
    if (!root.host) return
    root.host.stepBlur(direction)
  }

  function handleTextKey(text) {
    var key = String(text).toLowerCase()
    if (key === "r" && root.host) root.host.refresh()
    else if (key === "f" && root.host) root.host.toggle()
    else if (key === "p") root.togglePower()
  }

  // `replace` disappearing — the daemon restarted without a background — can
  // leave the cursor past the end of the list.
  onRowsChanged: {
    if (root.selectedIndex >= root.rows.length)
      root.selectedIndex = Math.max(0, root.rows.length - 1)
  }

  // ---- One row: a marker for the effect that is on, and its name.
  component EffectRow: CursorSurface {
    id: row

    required property int index
    required property var modelData

    readonly property string effect: String(modelData)
    readonly property bool isCurrent: root.running && root.state.effect === effect

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

      onClicked: {
        root.selectedIndex = row.index
        root.chooseSelected()
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
      anchors.left: marker.right
      anchors.leftMargin: Style.space(4)
      anchors.right: parent.right
      anchors.rightMargin: Style.space(8)
      anchors.verticalCenter: parent.verticalCenter
      textFormat: Text.PlainText
      text: row.effect === "none" ? "Off"
          : row.effect === "blur" ? "Blur background"
          : "Replace background"
      color: root.contentForeground
      font.family: root.contentFontFamily
      font.pixelSize: Style.font.body
      elide: Text.ElideRight
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
        else if (dx !== 0) root.stepBlur(dx > 0 ? 1 : -1)
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
          text: root.running
              ? "↑↓ move   enter choose   ←→ blur   p off   esc close"
              : "p turn on   esc close"
          color: root.dim
          font.family: root.contentFontFamily
          font.pixelSize: Style.font.caption
          wrapMode: Text.WordWrap
        }
      }
    }
  }
}
