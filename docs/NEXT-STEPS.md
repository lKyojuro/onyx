# Onyx Checkliste für die nächsten Schritte

Die Umsetzung pausiert. Beim Wiederaufnehmen zuerst den aktuellen Windows-Build im Alltag prüfen und danach das flexible Fensterlayout angehen. Die bisherige Entwicklung steht in der [Roadmap](ROADMAP.md).

## Aktueller Build

- [x] Release-EXE gebaut und Windows-GUI-Subsystem geprüft.
- [x] Formatierung, Workspace-Check, Clippy und alle 79 Tests bestanden. `--version` und `--smoke-test` lieferten in PowerShell die erwartete Ausgabe.
- [x] Performance-Sprint abgeschlossen: `314` Workspace-Tests bestanden, `cargo clippy --workspace --all-targets -- -D warnings` ohne Befund, Release-Build `22.93 MB`. Die Messwerte stehen in [PERFORMANCE.md](PERFORMANCE.md).
- [x] Aurora im dunklen Fensterinhalt sichtbar gemacht, doppelten ONYX-Schriftzug entfernt und die Speicheranzeige überarbeitet.
- [ ] EXE per Doppelklick starten und prüfen, dass nur das Onyx-Fenster erscheint. App-Name und Icon in der Titelleiste kontrollieren.
- [ ] Dunkles und helles Theme im laufenden Fenster prüfen. Auf den Aurora-Übergang, lesbare Speicherzahlen und die Laufwerksfarben achten. Ein schmales Fenster sowie die Ansichten mit Vorschau und zwei Dateibereichen testen.

## Performance-Sprint: Phase A „Feel Instant“ & Phase B.1 (abgeschlossen)

Stand 2026-11-26, Branch `zeron/onyx-performance-feature-sprint-a94df3`. Abnahme: `314` Workspace-Tests bestanden, `cargo clippy --workspace --all-targets -- -D warnings` ohne Befund, Release-Build `22.93 MB`. Alle Zahlen samt Vorher-Nachher-Vergleich stehen in [PERFORMANCE.md](PERFORMANCE.md).

- [x] **Single-Instance-IPC über Named Pipe:** Ein zweiter Start öffnet kein zweites Fenster mehr, sondern übergibt Pfad oder `SHOW` an die laufende Instanz (`onyx-windows/src/single_instance.rs`, `\\.\pipe\onyx-explorer-ipc-v1-<user>`). Warmer Start im Median `14.83 ms`, das Ziel `< 40 ms` ist erreicht.
- [x] **Startflags `--new-window` und `--background`:** `--new-window` erzwingt eine eigene Instanz, `--background` startet warm, ohne den Fokus zu nehmen.
- [x] **Zurückgestellter Start nach Frame 1:** `IndexController`, `notify`-Watcher, Clipboard-Monitor, die versteckten `InputState`s und der globale Rayon-Pool starten erst nach dem ersten Frame (`schedule_deferred_startup`, gegen Doppelaufrufe über `deferred_started` abgesichert). Dadurch sinkt der GUI-Start von `194.94 ms` auf `181.07 ms` (`-7,1 %`).
- [x] **SSD-bewusste Kopier- und Verschiebe-Engine:** `StorageDeviceSeekPenaltyProperty` unterscheidet SSD und HDD, kleine Dateien laufen parallel, große über `CopyFile2` mit `COPY_FILE_NO_BUFFERING`; auf ReFS wird zuerst `FSCTL_DUPLICATE_EXTENTS_TO_FILE` (Block-Cloning) versucht.
- [x] **Niedrig priorisierter Ordnergrößen-Scanner:** `FindFirstFileExW` unter `THREAD_MODE_BACKGROUND_BEGIN`, Rückgabe als Bottom-up-Teilbaum-Rollup, Cache-Schreibung gesammelt über `FolderSizeStore::upsert_batch`.
- [x] **Native Shell-Thumbnails:** `IShellItemImageFactory::GetImage` für Videos, PDFs, Office-Dokumente, SVG und RAW, mit `SIIGBF_INCACHEONLY` gegen das ungefragte Nachladen von Cloud-Platzhaltern.
- [x] **Live-Hotplug für USB- und Netzlaufwerke:** `WM_DEVICECHANGE` in [hotplug.rs](../crates/onyx-windows/src/hotplug.rs), Ein- und Ausstecken inklusive `DBTF_NET`-Volumes; das Verbinden einzelner Freigaben bleibt manuell.

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

- [x] **Ordnergrößen-Cache & CPU-Optimierung (hohe Priorität):** Kern-Engine umgesetzt und gemessen.
  - [x] Berechnete Ordnergrößen dauerhaft in SQLite speichern (`path`, `total_bytes`, `file_count`, `mtime`, `updated_at`), sodass nach Neustarts ohne Neu-Scan geladen wird. Der Scan liefert dafür ein Bottom-up-Rollup aller Teilbäume, das [folder_size_store.rs](../crates/onyx-cache/src/folder_size_store.rs) gesammelt über `upsert_batch` in einer Transaktion schreibt.
  - [x] Gezielte Invalidierung: Änderungen über den `notify`-Watcher markieren nur den betroffenen Ordner und seine Elternpfade als „dirty“ und berechnen nur diese neu; nach einem Neustart erfolgt der Abgleich über `mtime`. Ein NTFS-USN-Journal-Abgleich bleibt offen.
  - [x] Scan-CPU-Last gesenkt: in [folder_size.rs](../crates/onyx-fs/src/folder_size.rs) wird `dir_entry.metadata()` ohne zusätzlichen `symlink_metadata()`-Syscall genutzt, die Aufzählung läuft über `FindFirstFileExW` unter `THREAD_MODE_BACKGROUND_BEGIN` ([fast_scan.rs](../crates/onyx-windows/src/fast_scan.rs)), und die Parallelität orientiert sich an `recommend_scan_workers`.
  - [ ] Optional offen: der Modusschalter (*Automatisch / Nur auf Abruf / Aus*) in `Ctrl+,`. Die Einstellungen kennen bisher nur den Schalter `calculate_folder_sizes`; ein dreistufiger Modus ist nicht implementiert.
- [ ] **Zieh-Auswahl (Marquee / Gummiband-Auswahl) & Mehrfachauswahl-Komfort:**
  - Auf freier Fläche in der Dateiliste (Details- und Grid-Ansicht) klicken und ein Auswahlrechteck aufziehen, um mehrere Dateien/Ordner gleichzeitig zu markieren (inkl. Live-Highlight während des Ziehens und sauberem Scroll-Verhalten).
  - Ergänzend dazu `Ctrl+Klick` (einzelne Einträge hinzufügen/entfernen), `Shift+Klick` (Bereichsauswahl), `Ctrl+A` (Alles auswählen) und direktes Inline-Umbenennen in der Zeile.
- [ ] **Konfliktdialog-UI für Kopieren/Verschieben:** Slint-Modal (*Ersetzen / Überspringen / Beide behalten / Für alle anwenden*) an die fertige Engine in `onyx-operations` anbinden.
- [x] **UI-Redesign & Frontend-Evaluation (`gpui-kit` + `gpui-fast` vs. Slint):**
  - Entschieden zugunsten von **`gpui-kit` + `gpui-fast`** (DirectX 11 / DirectWrite, ~67,4 MB Idle Working Set vs. ~144–269 MB unter Slint; siehe [ADR 0004](decisions/0004-gpui-main-ui.md)).
  - `crates/onyx-ui-gpui` ist in den Root-Workspace integriert und baut das Haupt-Binary `onyx.exe`; die alten Slint-Crates (`onyx-ui`, `onyx-app`) wurden entfernt.
- [ ] **Inline-Fuzzy-Filter & „Group By“ in GPUI verdrahten:** Die fertige Engine in `onyx-core` (`filter.rs`, `group.rs`) in die Dateiliste von `onyx-ui-gpui` einbinden.
- [ ] **Weitere Phase-8-Ideen** siehe [Roadmap](ROADMAP.md) (Inline-Baumansicht, anpassbare Spalten & Per-Folder-Views, USB/Netzwerk-Hotplug, `.7z`, RAM-Optimierung `< 50 MB`, USN-Journal, signierte Releases).

## GPUI-Haupt-UI: Abgeschlossene Paritätspakete & Restpunkte

Stand: [onyx-ui-gpui](../crates/onyx-ui-gpui/src) ist die Haupt-UI von Onyx (`onyx.exe`) und deckt die Kernoberfläche komplett ab (Tabs, Dual-Pane, Seitenleiste, Vorschau inkl. `preview-side`, `move-tab-pane`, Quick-Look, Einstellungen, Command-Palette, natives Kontextmenü, ZIP-Overlay, internes & externes Drag & Drop mit Drag-Image, indexierte & Streaming-Suche, Barrierefreiheit, Papierkorb inkl. Wiederherstellen/Leeren, Watcher, Sitzungspersistenz, Ordnergrößen, Thumbnails).

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
  - `reset-layout`, `compress-zip`, `extract-here`, `extract-to-folder`, `browse-archive`, `move-tab-pane` (mit eigenem Tab-Zustand pro Pane) und `preview-side` (Vorschau links/rechts andocken inkl. Render-Reihenfolge in [splitter.rs](../crates/onyx-ui-gpui/src/components/splitter.rs)) sind als Command-Palette-Einträge bzw. Aktionen ergänzt.

- [x] **5. Papierkorb anzeigen, wiederherstellen & leeren (~0,5 Tag):**
  - `onyx-windows` listet den Papierkorb über den Shell-Namespace auf (`FOLDERID_RecycleBinFolder` über `SHGetKnownFolderItem`, Elemente über `IShellItem2`) und liefert Name, Originalpfad, Größe und Löschzeitpunkt als `RecycleBinItem`; die Shell-Aufrufe laufen in einem STA-Abschnitt auf dem Blocking-Pool, nie auf dem UI-Thread.
  - Der Spike hat dafür den dritten Listenmodus `ListMode::RecycleBin`, einen Seitenleisten-Eintrag über der Laufwerksliste, die Spalten „Date deleted“ und „Original location“, einen eigenen Leerzustand und die Elementanzahl in der Statuszeile.
  - Symbol: der Seitenleisten-Eintrag nutzt die Windows-Standardikonen (`SHGetStockIconInfo`, `SIID_RECYCLER`/`SIID_RECYCLERFULL`) statt einer eigenen Glyphe, also leerer vs. voller Mülleimer je nach Inhalt. Die Spike-Exe trägt jetzt das Onyx-Logo als Ressource (`build.rs` + `winresource`, wie die Slint-App) und setzt es zur Laufzeit per `set_window_icon_from_exe` auf das Fenster, damit Taskleiste und Alt-Tab mitziehen.
  - Wiederherstellen („Restore“) über `IFileOperation::MoveItem` und „Papierkorb leeren“ (`SHEmptyRecycleBinW`) mit Bestätigungsdialog sind über Toolbar und Kontextmenü angebunden.

Stand der Umsetzung: Paket 1–5 sind im Spike implementiert (`search_index.rs` inkl. Streaming-Modus über `onyx-search::search_streaming`, externer Drag über `start_external_file_drag`, `components/a11y.rs`, erweiterte Command-Palette inkl. `preview-side` und `move-tab-pane`, Papierkorb-Aufzählung sowie Restore/Leeren über `onyx-windows`). Der Drag-out hängt ein Drag-Bild an (`IDragSourceHelper::InitializeFromWindow` mit Fensterhandle und Startpunkt), sodass Windows den Bereich unter dem Zeiger mitschiebt; die Slint-App löst den Drag über eine Toolbar-Aktion aus und übergibt deshalb `None`. Automatisiert prüfbar ist der Drag-out über [smoke-gpui-drag.ps1](../scripts/smoke-gpui-drag.ps1) (synthetische Mausgeste, Drop auf den Desktop, Prüfung der Log-Zeile). Manuell zu prüfen bleiben das sichtbare Drag-Bild beim Ziehen, Ziehen in fremde Anwendungen wie Outlook, ein Narrator-Durchlauf, ein Drop aus einer fremden Anwendung ins Fenster, das Scroll-/Suchverhalten bei 50k Dateien mit `measure-perf.ps1` sowie die Papierkorb-Ansicht inkl. Restore/Leeren mit echtem Inhalt.

Dateien: [Cargo.toml](../crates/onyx-ui-gpui/Cargo.toml), [explorer.rs](../crates/onyx-ui-gpui/src/explorer.rs), [file_pane.rs](../crates/onyx-ui-gpui/src/components/file_pane.rs), [command_palette.rs](../crates/onyx-ui-gpui/src/components/command_palette.rs), [settings_overlay.rs](../crates/onyx-ui-gpui/src/components/settings_overlay.rs) und [context_menu.rs](../crates/onyx-ui-gpui/src/components/context_menu.rs) sowie [recycle_bin.rs](../crates/onyx-windows/src/recycle_bin.rs) für die Papierkorb-Aufzählung.
