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

## Nächste Prioritäten (v0.2-Abschluss & Phase 8)

- [ ] **Ordnergrößen-Cache & CPU-Optimierung (hohe Priorität):**
  - Berechnete Ordnergrößen dauerhaft in SQLite speichern (`path`, `total_bytes`, `file_count`, `mtime`, `updated_at`), sodass nach Neustarts sofort `< 1 ms` ohne Neu-Scan geladen wird.
  - Gezielte Invalidierung: Änderungen über den `notify`-Watcher markieren nur den betroffenen Ordner + Elternpfade als „dirty“ und berechnen nur diese neu (nach Neustart Abgleich via `mtime` und perspektivisch NTFS USN Journal).
  - Scan-CPU-Last drastisch senken: in [folder_size.rs](../crates/onyx-fs/src/folder_size.rs) direkt `dir_entry.metadata()` statt extra `symlink_metadata()`-Syscall nutzen, Parallelität auf 1–2 Hintergrund-Worker mit niedriger Priorität begrenzen und in den Einstellungen (`Ctrl+,`) den Modus (*Automatisch / Nur auf Abruf / Aus*) wählbar machen.
- [ ] **Zieh-Auswahl (Marquee / Gummiband-Auswahl) & Mehrfachauswahl-Komfort:**
  - Auf freier Fläche in der Dateiliste (Details- und Grid-Ansicht) klicken und ein Auswahlrechteck aufziehen, um mehrere Dateien/Ordner gleichzeitig zu markieren (inkl. Live-Highlight während des Ziehens und sauberem Scroll-Verhalten).
  - Ergänzend dazu `Ctrl+Klick` (einzelne Einträge hinzufügen/entfernen), `Shift+Klick` (Bereichsauswahl), `Ctrl+A` (Alles auswählen) und direktes Inline-Umbenennen in der Zeile.
- [ ] **Konfliktdialog-UI für Kopieren/Verschieben:** Slint-Modal (*Ersetzen / Überspringen / Beide behalten / Für alle anwenden*) an die fertige Engine in `onyx-operations` anbinden.
- [ ] **UI-Redesign & Frontend-Evaluation (`gpui-kit` + `gpui-fast` vs. Slint):**
  - Vor einem großen Slint-Redesign einen gezielten **`gpui-kit`-Spike mit `features = ["gpui-fast"]`** in einem separaten Prototyp-Crate testen (DirectX 11 + DirectWrite, Retained/Incremental-Caching von `gpui-fast` für minimale CPU-Last & 120–144 FPS, 75+ fertige Desktop-Widgets inkl. Data Tables, Docking/Splits, Inputs, Modals) und direkt mit `measure-perf.ps1` gegen Slint messen (EXE-Größe, Leerlauf-RAM, Kaltstart, Scroll-CPU-Last).
  - Basierend auf dem Ergebnis entscheiden, ob das UI-Redesign auf **`gpui-kit` (`gpui-fast`)** (bei Erhalt aller 8 UI-freien Core-Crates) oder auf **Slint** umgesetzt wird.
  - Grundlegende gestalterische Überarbeitung der Oberfläche (Farb-/Kontrastsystem, Typografie, Abstände/Dichte, Header-/Tab-/Toolbar-Aufbau, Seitenleiste, Dateiliste und Dark/Light-Theme), um ein wirklich stimmiges, eigenes und vertrautes Design zu finden.
  - Vor der Umsetzung gezielt Design-Entwürfe/Varianten abstimmen, statt blind umzubauen.
- [ ] **Weitere Phase-8-Ideen** siehe [Roadmap](ROADMAP.md) (Inline-Filter & Gruppierung, Baumansicht, anpassbare Spalten, USB/Netzwerk-Hotplug, `.7z`/`.tar.gz`, RAM-Optimierung, USN-Journal, signierte Releases).

## GPUI-Spike: Lücken bis zur Slint-Parität

Stand: [onyx-ui-gpui](../crates/onyx-ui-gpui/src) deckt die Kernoberfläche bereits ab (Tabs, Dual-Pane, Seitenleiste, Vorschau, Quick-Look, Einstellungen, Command-Palette, natives Kontextmenü, ZIP-Overlay, internes Drag & Drop, Watcher, Sitzungspersistenz, Ordnergrößen, Thumbnails). Beide Oberflächen haben 31 Befehle, aber unterschiedliche Sätze: dem Spike fehlen `move-tab-pane`, `preview-side`, `powershell`, `command-prompt` und `index`, dafür hat er `up`, `view-mode`, `copy-path`, `system-files` und `folder-sizes` voraus. Die folgenden Pakete schließen die Lücke; sie sind Implementierungsarbeit, keine Framework-Grenze.

- [x] **1. Indexierte Suche anbinden (`onyx-search`, 1–2 Tage):**
  - `onyx-search` als Dependency des Spike-Crates aufnehmen. Heute fehlt sie dort, die Suche filtert nur die bereits geladenen Einträge im Speicher und kennt keine Treffer außerhalb des aktuellen Ordners.
  - Indexer-Lebenszyklus wie in der Slint-App übernehmen: Indexdatei, Update-Debounce auf Watcher-Ereignisse und sauberes Herunterfahren per `CancellationToken`.
  - Treffer als eigenen Listenmodus einspeisen, mit eigener `GenerationId` pro Suchlauf und Abbruch beim Weitertippen; Ergebnisse über `Entity::update` auf den GPUI-Thread zustellen, nie blockierend.
  - Befehl `index` (Indexer starten/anhalten/neu aufbauen) und einen Suchmodus in der Command-Palette ergänzen, dazu den Einstellungs-Schalter für die indexierte Suche an `AppConfig` anbinden.
  - Abnahme: Suche über 50k Dateien liefert Treffer ohne Ruckeln, ein neuer Tastendruck bricht den alten Lauf ab, der Index übersteht einen Neustart.
- [x] **2. Drag-out über OLE (`DoDragDrop`, ~1 Tag):**
  - `onyx_windows::start_external_file_drag` auslösen, sobald die Ziehgeste das Fenster verlässt. Heute gibt es nur internes Drag & Drop, Dateien lassen sich nicht in Explorer, Desktop oder andere Anwendungen ziehen.
  - Den Drag auf einem eigenen STA-Thread starten und das `DragOutcome` zurückmelden (bei Move die Quelle nach Erfolg entfernen, bei Copy nichts), ohne den UI-Thread zu blockieren; einen laufenden internen Drag dabei sauber abbrechen.
  - Abnahme: Ziehen in Explorer, Desktop und Outlook funktioniert, Move verschiebt tatsächlich, `Escape` bricht ab, ohne dass die Auswahl verloren geht.
- [x] **3. Barrierefreiheit nachziehen (2–3 Tage):**
  - Rollen und Labels in Dateiliste (`list`/`list-item`), Tabellenkopf, Toolbar, Tableiste, Seitenleiste und allen Dialogen ergänzen. Vorbild ist die Slint-Oberfläche, die `accessible-label` in zwölf Dateien setzt; der Spike hat heute keine `aria`-Attribute.
  - Fokusreihenfolge und Fokusfalle in den Modals (Command-Palette, Einstellungen, Prompt- und Konfliktdialog, Archive-Overlay), `Escape` und `Tab` überall gleich.
  - Abnahme: Narrator liest Dateiliste, Auswahl und Dialoge sinnvoll vor; ein kompletter Durchlauf ist nur mit Tastatur und Screenreader möglich.
- [x] **4. Kleinere Befehle für volle Command-Parität (0,5–1 Tag):**
  - `reset-layout`, `compress-zip`, `extract-here`, `extract-to-folder` und `browse-archive` sind als Command-Palette-Einträge ergänzt; der Spike hat damit 31 Befehle wie die Slint-App.
  - Offen bleiben `move-tab-pane` (braucht ein Tab-Modell pro Bereich, der Spike hat heute einen globalen `TabManager`) und `preview-side` (braucht ein Feld für die Vorschauseite plus Render-Reihenfolge in [splitter.rs](../crates/onyx-ui-gpui/src/components/splitter.rs)) — beides Modellarbeit, kein Paletten-Eintrag.

Stand der Umsetzung: Paket 1–4 sind im Spike implementiert (`search_index.rs`, externer Drag über `start_external_file_drag`, `components/a11y.rs`, erweiterte Command-Palette). Automatisiert prüfbar ist der Drag-out über [smoke-gpui-drag.ps1](../scripts/smoke-gpui-drag.ps1) (synthetische Mausgeste, Drop auf den Desktop, Prüfung der Log-Zeile). Manuell zu prüfen bleiben Ziehen in fremde Anwendungen wie Outlook, ein Narrator-Durchlauf und das Scroll-/Suchverhalten bei 50k Dateien mit `measure-perf.ps1`. Offen ist außerdem der Streaming-Modus der Suche (`onyx-search::search_streaming`) für große, noch nicht indizierte Ordner.

Dateien: [Cargo.toml](../crates/onyx-ui-gpui/Cargo.toml), [explorer.rs](../crates/onyx-ui-gpui/src/explorer.rs), [file_pane.rs](../crates/onyx-ui-gpui/src/components/file_pane.rs), [command_palette.rs](../crates/onyx-ui-gpui/src/components/command_palette.rs), [settings_overlay.rs](../crates/onyx-ui-gpui/src/components/settings_overlay.rs) und [context_menu.rs](../crates/onyx-ui-gpui/src/components/context_menu.rs).

Diese Pakete lohnen erst nach der Entscheidung aus „UI-Redesign & Frontend-Evaluation“ oben. Bis dahin bleibt der Spike ein Prototyp und wird von CI und Release nicht gebaut.
