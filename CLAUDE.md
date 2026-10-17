# Coffee Filter: Notes for Agents

Everything an agent needs to work on this repo: what it is, how to build, test, install, and release it, the owner's rules, and the traps that already cost time. Read this before changing anything. `README.md` is the user-facing description; this file is for working on the code.

## What It Is

A local HTTPS ad blocker for Windows, like Zen. Two programs:

- **`core/`** — Rust, `coffee-filter-core.exe`. A MITM proxy (hudsucker 0.25, HTTP/2 both legs) on `127.0.0.1:18228` that blocks with adblock-rust 0.13, injects cosmetic rules, scriptlets, and userscripts into pages, and serves its own PAC script. Windows is pointed at it with a PAC URL (`AutoConfigURL`, `ProxyEnable=0`), never a fixed proxy.
- **`app/`** — WinUI 3, .NET 10, Native AOT, packaged as MSIX. Starts the core as a child process and talks to it over stdin/stdout, one JSON object per line (no control port). Each command gets one reply line; between replies the core may also write `{"event":"problem"}` when a host starts having connection problems, which `CoreClient` raises as `ProblemFound` instead of taking it for a reply. Settings-style window, tray icon with a flyout, global shortcuts, notifications, sub-pages.

The core restores the user's proxy settings on stop, crash, logoff, and shutdown, and on the next start if anything was left behind. If the core dies, the app restarts it (three times a minute at most).

### Layout

- `core/src/main.rs` — command loop (`Command` enum, JSON-lines), start/stop, shutdown, status reply
- `core/src/filter.rs` — `Shared` state, list downloads and engine build, the request handler (`Blocker`), DNS-rebinding defenses, live log, t.co resolver, blocked page, built-in rules
- `core/src/ca.rs` — per-PC CA (DPAPI key), elevated certutil install/remove, site certificates
- `core/src/cosmetic.rs` — page injection, CSP nonces, AdGuard syntax, scriptlet library (SHA-256 pinned)
- `core/src/userscripts.rs`, `userscripts.js` — Tampermonkey-style scripts and the install page
- `core/src/apps.rs` — excluded apps: which program owns a connection (TCP table)
- `core/src/sysproxy.rs` — PAC script, `SYSTEM_HOSTS` (Windows Update/Store, always direct), proxy backup and restore
- `core/src/pressure.rs` — efficiency mode off, below-normal parsing, low-memory handling
- `core/src/problems.rs`, `config.rs` — connection problems, settings (`config.json`), atomic writes
- `core/build.rs` — puts the coffee icon into the core exe (needs `rc.exe` from the Windows SDK)
- `app/App.xaml.cs` — core lifecycle, `coffee-filter://` links, tray, flyout, shortcuts, problem notifications, efficiency mode while hidden
- `app/FlyoutWindow.xaml(.cs)`, `FlyoutChrome.cs` — the tray flyout (left-click or shortcut): see-through window, always-active acrylic, slides out of the taskbar on any edge (ported from MagicBuds)
- `app/Shortcuts.cs`, `Hotkey.cs` — global shortcuts (`RegisterHotKey` on a message-only window) and the picker's keyboard hook
- `app/ShortcutControl.xaml(.cs)`, `ShortcutDialogContentControl.xaml(.cs)`, `HotkeySettings.cs`, `KeyVisual.cs`, `KeyVisual.xaml` — PowerToys' shortcut picker and key caps, ported by way of Leaf Calendar and MagicBuds
- `app/Notifications.cs` — Windows notifications; a click opens the page named in its `open` argument
- `app/MainWindow.xaml(.cs)` — the window: cards, sub-pages, dialogs, Exclusions, Store app picker
- `app/CoreClient.cs` — the JSON-lines link to the core (UTF-8 pipes, snake_case source-generated JSON)
- `app/ClickCard.cs`, `TrayMenu.cs` (right-click menu), `AppSettings.cs`, `Program.cs`, `Package.appxmanifest`
- `tests/CoffeeFilter.Tests/` — app-to-core tests against the real core exe in a temp profile
- `tests/CoffeeFilter.UITests/` — FlaUI UI tests that drive the **installed** app
- `scripts/` — build, install, test, release (see below)

User data lives in `%LOCALAPPDATA%\CoffeeFilter\` (`config.json`, `app.json`, `ca.cer`, `ca.key.dpapi`, `lists\`, `engine.dat`, `userscripts\`, `app-errors.log`, `problems.json`). The MSIX turns off file and registry virtualization, so it's the real folder.

## Working Here

### Build

```powershell
scripts\build.ps1
```

Builds the core in release mode, then `dotnet publish -c Release` in `app/`, which compiles Native AOT, bundles the core, and makes a signed MSIX in `app\AppPackages\CoffeeFilter_<version>_x64_Test\`. The AOT linker needs `vswhere.exe` on `PATH` (the script adds it). The package is signed with the owner's certificate (thumbprint in `app\CoffeeFilter.csproj`); on another PC pass `-p:PackageCertificateThumbprint=<yours>`.

For a quick compile check of the app only: `dotnet build -c Debug` in `app/`.

### Install

```powershell
scripts\install.ps1
```

**Windows refuses to install changed content under the same version.** Before every build you intend to install, bump the **fourth** number in both `app\CoffeeFilter.csproj` (`<Version>`) and `app\Package.appxmanifest` (`Identity Version`), e.g. `0.1.1.0` → `0.1.1.1`. Installing force-closes the running app.

### Test

```powershell
scripts\test.ps1          # everything, UI tests included
scripts\test.ps1 -SkipUi  # quick check while working (never for a release)
```

In order, stopping at the first failure:

1. `cargo clippy --release --all-targets -- -D warnings` (no warnings allowed)
2. `cargo test --release` in `core/` (unit tests)
3. `cargo test --release -- --ignored` in `core/` (end-to-end: real proxy on port 18229, real Chrome, briefly changes and restores the system proxy)
4. `dotnet test` in `tests/CoffeeFilter.Tests` (app-to-core)
5. `dotnet test` in `tests/CoffeeFilter.UITests` (UI, against the installed app; the script refuses if the installed version isn't the one in the manifest)

UI tests open pages, groups, and dialogs and cancel them. One test (`ExcludingAStoreAppListsItAndDeleteRemovesIt`) excludes a Store app the user hasn't excluded and deletes it again, undoing in `finally` and failing loudly if it can't. Launching the app also turns blocking on if it was on last time. They need a few Store apps installed.

Every change gets a test that would have caught it: a core unit test for logic, an app-to-core test for command behavior, a UI test for anything on screen.

### Commit and Push

Only when the owner says so. Never comment on, review, or change the status of PRs or issues unless told to.

## Versioning

- **Each release goes up by one in the third number** (0.1.1 → 0.1.2) unless the owner names another version. Never bump the first two on your own.
- **Between releases, only the fourth number goes up**, once per build you install (see Install).
- `core/Cargo.toml` `version` matches the three-part release number (it's stamped on the core exe).
- Releases, tags, and file names use **three numbers only**: tag `v0.1.1`, title "Coffee Filter 0.1.1", asset `CoffeeFilter_0.1.1_x64.msix`. The fourth number is never shown.

## Release Gate

**No release goes out until every test passes, UI tests included.** That means `scripts\test.ps1` run to the end with no failures, against the build being released, installed: clippy, core unit tests, the end-to-end test, the app-to-core tests, and all UI tests. No skipped suites (`-SkipUi` is never allowed for a release), no "flaky, ignore it", no releasing first and fixing after. If a test fails, fix the cause (or, if the test itself is wrong, fix the test and say so), rebuild, reinstall, and run everything again. `scripts\release.ps1` enforces this by running the full suite first and stopping on any failure.

## Releasing

Only when the owner asks for a release, with the version they name, and only through the Release Gate above.

1. Set the version (three numbers) in `app\CoffeeFilter.csproj`, `app\Package.appxmanifest` (fourth number `0`), and `core\Cargo.toml`.
2. `scripts\build.ps1`, then `scripts\install.ps1`.
3. Commit and push to `main` (the owner asking for a release is the go-ahead for this).
4. Write release notes (what changed, install steps, link to the README).
5. `scripts\release.ps1 -Notes <notes.md>`. It runs `scripts\test.ps1` first and refuses to continue on any failure, checks that `main` is pushed and the tree is clean, then creates the release with the installer (renamed to three numbers) and `CoffeeFilter-signing.cer` (the public half of the signing certificate, exported fresh; never commit it).

## The Owner's Rules

- **US English** everywhere: code, comments, docs, commit messages ("canceled", "color", "behavior").
- **Replies are short and plain**: what changed and why, in everyday words, no jargon, no filler.
- **Copy content verbatim.** Never reword text the owner wrote; ask if something looks wrong.
- **Don't run bulk commands**; edit files.
- **Ask only for real decisions** (licensing, anything public, data loss); otherwise pick the sensible option and say so.

### Code Style

Match the surrounding code:

- 4-space indent (2 in CSS and YAML).
- A Title Case label comment before each block (`// Load While Open`, `<!-- Exclusions -->`); a closing comment on long XAML blocks (`<!-- /Exclusions -->`).
- Comments say what a block is or why, never restate the syntax.
- Align the `=` of related assignments into a column.
- Guard clauses first, then the work.
- Section banners in long files: `// ====...` / `// SECTION NAME` / `// ====...`.
- Docs (README) are second person and plain ("you'll need", "That's it!"), with `>` blockquotes for asides.
- PowerShell scripts start with a header block: what they output, what they're used for, and what they need, with links.

## Traps (Already Hit)

Each of these cost real time; don't relearn them.

### WinUI and Native AOT

- **No data templates for lists.** `DataTemplate` (XAML or `XamlReader`), data binding, and even `ItemsSource` of plain strings came out blank or crashed with "Specified cast is not valid" under Native AOT. Build each row in code as a `ListViewItem` (or card) and add it directly (`MainWindow.OnAddStoreAppClicked`).
- **`SettingsExpander.Items` doesn't redraw when edited in place.** `Clear()`/`Add()` on a group already on screen does nothing visible. Build a new list and assign it: `ReplaceItems(group, items)`.
- **One `ContentDialog` at a time, and a closing one still counts.** Open every dialog through `ShowDialogAsync`, which waits out a closing dialog. Read results (like a list's selection) **before** the dialog closes; after `ShowAsync` returns the selection is gone.
- **Toolkit `SettingsCard` wraps its button below the text under 476 px**, and app-level resources can't change that. `OneRow`/`DisableWrap` turn off its `ControlSizeTrigger`s per card. Call `Row(card)` on every card you create.
- **Toolkit `SettingsCard` claims UI Automation Invoke but doesn't implement it** ("Specified cast is not valid" from screen readers and UI tests). Clickable cards use `ClickCard` (`app/ClickCard.cs`).
- **Name every control for screen readers.** Header buttons inside a `SettingsCard` inherit the card's name ("Add app" read as "Excluded apps"); set `AutomationProperties.Name`. The UI test `EveryButtonAndSwitchHasASpokenName` checks this.
- **`GetDpiForWindow` says 100% before a window is shown.** Use the monitor's DPI (`Scale()` in `MainWindow`).
- **Tray icon from a ready `System.Drawing.Icon`**, not `IconSource`: the library converts `IconSource` in the background and the tray icon sometimes appeared blank.
- **Activation args belong to the launching process.** Read `AppActivationArguments.Data` immediately in the `Activated` handler; after the second instance exits it throws RPC errors.
- **The packaged app needs every logo size** (`Square44x44Logo.targetsize-*`, `_altform-unplated`, `_altform-lightunplated`, `scale-*` for every tile). Without them the taskbar, Start, and Task Manager show a generic icon. Sources are in `app\Assets\source\` (not packaged).
- **Hand focus to the taskbar only after opening another window.** A closing flyout gives the foreground to `Shell_TrayWnd`; once it has, Windows refuses to bring the main window forward. Open the window first (`FlyoutWindow.OnSettingsClicked`).
- **`GetForegroundWindow` can return a WinUI window's inner input window** (`InputSiteWindowClass`), not the window itself. Compare with `PopupWindow.Foreground`, which returns the owning top-level window; a raw comparison made the shortcut picker ignore every key.
- **The app has more than one top-level window** (main window, flyout, tray menu). Find the main window by its name "Coffee Filter", never by "the main window" of the process (`AppFixture`).
- **Windhawk injected into the app can freeze its window** when a launcher like Raycast opens another app while Coffee Filter has focus (Windows App SDK input code spins). Not our bug; README tells users to exclude `CoffeeFilter.exe` in Windhawk.

### Windows and Networking

- **Sandboxed Store apps can't reach `127.0.0.1`.** The PAC returns `PROXY 127.0.0.1:18228; DIRECT` so they fall back to direct instead of losing the internet.
- **Windows Update and Store downloads use the user's proxy and reject any MITM** (0x80240437). `SYSTEM_HOSTS` in `sysproxy.rs` always go direct and are never decrypted.
- **Qt apps (Stream Deck) reject a fixed proxy**; that's why Windows gets a PAC URL, not `ProxyServer`.
- **`certutil -user -delstore` shows a confirmation dialog** that hung the core when run hidden. Removal runs one elevated `cmd` for both stores, and every elevated step has a time limit.
- **Never decrypt private or local addresses**, and refuse public names that resolve to them (DNS rebinding): `PublicOnly` resolver, `resolve_public` for tunnels, `points_local` for WebSockets.
- **Excluded apps** are found from the TCP table (`apps.rs`): match only established rows, cache briefly.

### Editing Files Here

- **Line endings are mixed (CRLF and LF).** Scripted edits must keep each file's own ending: read with `newline=""`, detect `\r\n`, and write the same. Editing a CRLF file as LF breaks exact-match replacements.
- **Backslashes and quotes in scripted edits** (Rust byte literals like `b'\t'`, Windows paths, `'''` next to an apostrophe) have broken edits several times. Prefer the Edit tool, or write the edit script to a file with raw strings and check every replacement matched exactly once.
- **Don't reuse scratch file names** across sessions: a leftover `readme3.py` from an earlier session once ran instead of the new one and duplicated a README section.

## Security Invariants

Keep these true; the reviews checked each one.

- The CA is per-PC, path length 0, server-auth only; its key is DPAPI-encrypted and never leaves the PC. `check_ours` validates `ca.cer` before any elevated step.
- Lists, userscripts, and `@require` files are `https://` only, including after redirects. The scriptlet library is pinned by SHA-256.
- Anything a web page can trigger (`coffee-filter://install`, `coffee-filter://exclude`) needs the browser's prompt and the app's dialog, with no default button and Add/Exclude disabled for a second. Links never start the certificate UAC prompt. Site names from links must look like site names (with a dot).
- Injected code is escaped so it can't close its `<script>` (`escape_script`), and CSP nonces are added only where inline code is blocked.
- No secrets in the repo: no certificates, keys, or `.cer` files are ever committed (`.gitignore` covers them).
