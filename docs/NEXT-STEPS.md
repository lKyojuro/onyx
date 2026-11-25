# Onyx Checkliste für die nächsten Schritte

Die Umsetzung pausiert. Beim Wiederaufnehmen zuerst den aktuellen Windows-Build im Alltag prüfen und danach das flexible Fensterlayout angehen. Die bisherige Entwicklung steht in der [Roadmap](ROADMAP.md).

## Aktueller Build

- [x] Release-EXE gebaut und Windows-GUI-Subsystem geprüft.
- [x] Formatierung, Workspace-Check, Clippy und alle 79 Tests bestanden. `--version` und `--smoke-test` lieferten in PowerShell die erwartete Ausgabe.
- [x] Aurora im dunklen Fensterinhalt sichtbar gemacht, doppelten ONYX-Schriftzug entfernt und die Speicheranzeige überarbeitet.
- [ ] EXE per Doppelklick starten und prüfen, dass nur das Onyx-Fenster erscheint. App-Name und Icon in der Titelleiste kontrollieren.
- [ ] Dunkles und helles Theme im laufenden Fenster prüfen. Auf den Aurora-Übergang, lesbare Speicherzahlen und die Laufwerksfarben achten. Ein schmales Fenster sowie die Ansichten mit Vorschau und zwei Dateibereichen testen.

## Flexibles Fensterlayout

### 1. Seitenleiste ein- und ausblenden

- [ ] Einen dauerhaft erreichbaren Schalter neben der Vorschau in der Werkzeugleiste ergänzen. Das vorhandene Muster für Beschriftung, aktiven Zustand und Callback verwenden. Den Dateibereich beim Ausblenden den Platz nutzen lassen.
- [ ] Die Sichtbarkeit über `SessionState` beim Beenden speichern. Sitzungen ohne das neue Feld öffnen die Seitenleiste wie bisher. Die Vorschau behält ihren vorhandenen Schalter und ihre Konfiguration.
- [ ] Schalter per Maus und Tastatur prüfen. Beim Ausblenden den Fokus zum Schalter zurückführen, falls er im ausgeblendeten Bereich lag. Neustart mit sichtbarer und ausgeblendeter Seitenleiste prüfen. Einen automatisierten Test für neue und ältere Sitzungsdateien ergänzen.

Dateien: [main.slint](../crates/onyx-ui/ui/main.slint), [toolbar.slint](../crates/onyx-ui/ui/components/toolbar.slint), [controller.rs](../crates/onyx-app/src/controller.rs), [ui_sync.rs](../crates/onyx-app/src/ui_sync.rs), [state.rs](../crates/onyx-app/src/state.rs), [model.rs](../crates/onyx-config/src/model.rs) und [store.rs](../crates/onyx-config/src/store.rs).

### 2. Breiten per Ziehen ändern und speichern

- [ ] Eine gemeinsame Slint-Komponente für die drei Ziehtrenner ergänzen. Zeigerpositionen aus der `TouchArea` in einem festen Koordinatensystem erfassen, damit der mitwandernde Trenner das Delta nicht verfälscht. Loslassen und Abbruch behandeln. Eine Tastaturbedienung mit sichtbarem Fokus und Pfeiltasten vorsehen.
- [ ] Mit 180–400 logischen px für die Seitenleiste, 220–480 für die Vorschau und mindestens 240 pro Dateibereich beginnen. Bei 960 px passen die bisherigen Standardbreiten im Zweibereichsmodus zusammen mit diesen Mindestbreiten nicht. Seitenleiste und Vorschau bei Platzmangel innerhalb ihrer Grenzen temporär schmaler darstellen und die gespeicherten Wunschbreiten bei mehr Platz wieder anwenden.
- [ ] Wunschbreiten der Seitenleiste und Vorschau sowie das Verhältnis zwischen den Dateibereichen beim Beenden speichern. Layoutwerte beim Wiederherstellen begrenzen. Ohne gespeicherte Breite gelten 226 px für die Seitenleiste und 310 px beziehungsweise 264 px für die Vorschau. Die Dateibereiche teilen sich den Platz standardmäßig gleichmäßig.
- [ ] Alle Trenner mit Maus und Tastatur ausprobieren. Bei 960 px mit zwei Dateibereichen, Seitenleiste und Vorschau prüfen, dass kein Bereich unter seine Mindestbreite fällt. Fenster anschließend vergrößern und gespeicherte Breiten nach einem Neustart kontrollieren.

Dateien: [main.slint](../crates/onyx-ui/ui/main.slint), [sidebar.slint](../crates/onyx-ui/ui/components/sidebar.slint), neue Trennerkomponente in `ui/components/` und dieselben Sitzungsdateien wie in Schritt 1.

### 3. Gruppen in der Seitenleiste einklappen

- [ ] Favoriten, Schnellzugriff und Laufwerke über ihre Gruppenüberschriften unabhängig ein- und ausklappbar machen. Stabile Gruppen-IDs in der Sitzung speichern. Ohne gespeicherte Auswahl sind alle Gruppen geöffnet.
- [ ] Tastaturbedienung, sichtbaren Fokus und den gemeldeten Auf- oder Zuklappzustand prüfen. Nach einem Neustart müssen die Gruppen ihren vorherigen Zustand wieder anzeigen.

Dateien: [sidebar.slint](../crates/onyx-ui/ui/components/sidebar.slint), bei Bedarf [icons.slint](../crates/onyx-ui/ui/components/icons.slint) und die Sitzungspersistenz.

### 4. Vorschau links oder rechts andocken

- [ ] Als erste Anordnungsoption die vorhandene Vorschau links oder rechts von den Dateibereichen platzieren. Die Seite speichern und bei älteren Sitzungen rechts öffnen. Dieselbe Vorschau-Logik und denselben Trenner weiterverwenden.
- [ ] In der Befehlspalette einen Wechsel der Vorschauseite und „Layout zurücksetzen“ anbieten. Tastaturkürzel erst nach Prüfung der vorhandenen Belegung vergeben. Beim Zurücksetzen Sichtbarkeit, Gruppen, Wunschbreiten, Teilungsverhältnis und Vorschauseite auf ihre Standardwerte setzen. Tabs und aktuelle Pfade bleiben erhalten.
- [ ] Andocken, Schließen und Wiederöffnen der Vorschau mit einem und zwei Dateibereichen prüfen. Fokus, Auswahl und Vorschauinhalt kontrollieren. Nach Neustart und Layout-Reset die erwartete Anordnung prüfen.

Dateien: [main.slint](../crates/onyx-ui/ui/main.slint), [commands.rs](../crates/onyx-app/src/commands.rs) und die Sitzungspersistenz.

### 5. Seitentasten der Maus für Zurück und Vorwärts

- [ ] Abfangen entscheiden und begründen. Slint 1.19 übersetzt die Seitentasten bereits (`PointerEventButton.Back` und `.Forward`, gemappt in `i-slint-backend-winit-1.19.1`), liefert Zeigerereignisse aber nur an das Element unter dem Zeiger. Ein fensterweiter Callback existiert nicht, und ein darübergelegtes `TouchArea` würde die Bedienung darunter blockieren. Deshalb `WM_XBUTTONDOWN` und `WM_XBUTTONUP` in [onyx-windows](../crates/onyx-windows/src/lib.rs) über `SetWindowLongPtrW` abfangen und die ursprüngliche Fensterprozedur per `CallWindowProcW` weiterreichen. Kein neues Crate.
- [ ] XBUTTON1 auf Zurück und XBUTTON2 auf Vorwärts des aktiven Dateibereichs legen, gleiche Pfade wie `nav-back` und `nav-forward`. Bei offener Befehlspalette, offenem Dialog oder laufender Adressbearbeitung nichts tun. Den Unterbau nach einem neuen Fensterhandle erneut setzen und die `// SAFETY:`-Kommentare nach [AGENTS.md](../AGENTS.md) ergänzen.
- [ ] Prüfen: Seitentasten über Dateiliste, Seitenleiste, Werkzeugleiste, Tableiste und Vorschau; bei zwei Bereichen mit unterschiedlicher Historie nur der aktive Bereich; ohne verfügbare Historie keine Wirkung; ohne installierten Unterbau stürzt nichts ab.

Dateien: [lib.rs](../crates/onyx-windows/src/lib.rs), [controller.rs](../crates/onyx-app/src/controller.rs), [ui_sync.rs](../crates/onyx-app/src/ui_sync.rs) und [main.slint](../crates/onyx-ui/ui/main.slint).

### 6. Dateibereiche und Tabs tauschen

- [ ] Einen Befehl „Dateibereiche tauschen“ ergänzen, der Pfad, Verlauf, Auswahl und Fokus von primärem und sekundärem Bereich tauscht. Auf `tab`/`tab_mut` in [state.rs](../crates/onyx-app/src/state.rs) aufsetzen: der primäre Bereich ist der aktive Tab im `TabManager`, der sekundäre ist `secondary_tab`. Dann sichert die vorhandene Sitzungsspeicherung den getauschten Zustand ohne neues Feld.
- [ ] Einen Tab in den anderen Bereich verschieben anbieten, mindestens als Befehl für den aktiven Tab, dazu ein Eintrag im Kontextmenü der Tableiste. Der vorhandene `reorder-tab` deckt nur die Reihenfolge innerhalb einer Tableiste ab ([tab_bar.slint](../crates/onyx-ui/ui/components/tab_bar.slint)); Verlauf, Auswahl und Scrollposition des Tabs müssen mitwandern.
- [ ] Tastenkürzel erst nach Prüfung der bestehenden Belegung in [main.slint](../crates/onyx-ui/ui/main.slint) vergeben. Belegt sind dort bereits Ctrl+T, Ctrl+W, Ctrl+L, Ctrl+F, Ctrl+`, Alt+T, F5, Alt+Pfeil sowie Space, Enter, Backspace, F2, Delete und die Ctrl-Buchstaben für die Dateiaktionen.
- [ ] Prüfen: Tausch mit einem und mit zwei Bereichen, mit gefülltem Verlauf und nach Neustart; Verschieben eines Tabs erhält Verlauf und Auswahl; bei einem einzelnen Bereich bleibt der Befehl ohne Wirkung oder wird ausgegraut. Der Tausch muss auch mit links oder rechts angedockter Vorschau stimmen.

Dateien: [commands.rs](../crates/onyx-app/src/commands.rs), [controller.rs](../crates/onyx-app/src/controller.rs), [state.rs](../crates/onyx-app/src/state.rs), [tab_bar.slint](../crates/onyx-ui/ui/components/tab_bar.slint) und [main.slint](../crates/onyx-ui/ui/main.slint).

Nach jedem Meilenstein die Qualitätschecks aus [AGENTS.md](../AGENTS.md) ausführen. Die Abnahme im laufenden Fenster umfasst beide Themes, Tastaturbedienung und die Kombination aus schmalem Fenster, zwei Dateibereichen und Vorschau.
