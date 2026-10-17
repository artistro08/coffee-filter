# Coffee Filter

A local HTTPS ad blocker for Windows, in the spirit of Zen. A small Rust proxy built on [hudsucker](https://github.com/omjadas/hudsucker) filters every request against EasyList-style lists, and a WinUI 3 app turns it on and off.

## Introduction

When blocking is on, Windows sends web traffic through a proxy running on your own PC (`127.0.0.1:18228`). It does that with a PAC script the proxy serves itself: web traffic (`http`/`https`) goes to the proxy, while this PC, your home network, and every other kind of connection go direct. To read HTTPS traffic, the proxy uses a certificate authority (CA) made just for your PC. Requests matching a filter list get a `403` and never reach the ad server.

Filtering uses [adblock-rust](https://github.com/brave/adblock-rust), the engine inside Brave. Lists are shown in groups like Zen's (Ads, Privacy, Malware, General, Digital wellbeing, Regional, Custom) using each list's `kind` in `config.json`. It starts with five general lists, and you can add your own in Adblock Plus or hosts file format (`https://` addresses only, so a list can't be tampered with on the way).

### Blocked Pages

When a whole page is blocked, you get a Windows 11 styled page in your own accent color (read from Windows, since browsers don't share it) showing the address, with **Go back** and **Exclude this site**. Exclude opens Coffee Filter, which asks before adding the site to your excluded sites.

### Short Links

Clicking a `t.co` link doesn't send your browser to t.co. Coffee Filter asks t.co itself where the link goes (without your cookies or the page you came from), removes tracking parameters like `utm_*` and `fbclid` from the destination, and sends the browser straight there.

### Built-in Fixes

Coffee Filter carries a few rules of its own for ads the public lists don't catch yet. Today that's X's promoted posts, which are now labeled "Ad" instead of "Promoted".

### Cosmetic Filtering

Some ads (like Reddit's promoted posts) are part of the page itself, so there's no request to block. For those, Coffee Filter adds one `<style>` and one `<script>` right after `<head>` in each HTML page, the way Zen and AdGuard do:

- **Hide rules** (`##`) become CSS.
- **Style and advanced rules** (`#$#`, `:style()`, `:has-text()`, `:upward()`, `:xpath()`, `:remove()`, ...) run in a small script.
- **Scriptlets** (`##+js(...)` and AdGuard's `#%#//scriptlet(...)`) come from [AdGuard's scriptlet library](https://github.com/AdguardTeam/Scriptlets), pinned to v2.5.1 and its SHA-256, so a changed or tampered download is refused.
- **`trusted-*` scriptlets** only run for lists with `"trusted": true` in `config.json`. Lists you add in the app aren't trusted; set the flag by hand only for lists you trust (if you copy Zen's lists over, keep Zen's trusted flags, which mark the AdGuard lists).
- If a site's Content-Security-Policy would block inline code, a fresh nonce is added for these two tags only.

> AdGuard rules that run raw JavaScript (`#%#` without `//scriptlet`) are skipped on purpose. They're arbitrary code from a list.

### Userscripts

You can add userscripts, the `.user.js` scripts Tampermonkey, Greasemonkey, and AdGuard run (from Greasy Fork, OpenUserJS, and so on). Click **Add userscript** and paste the script's `https://` address.

- **Where they run:** HTML pages matching the script's `@match` or `@include` lines (wildcards and `/regex/`), and none of its `@exclude` or `@exclude-match` lines. A script with neither runs everywhere.
- **When:** `@run-at document-start` runs before the page; anything else runs once the page's HTML has loaded. `@noframes` skips frames.
- **`@require` files** are downloaded with the script (`https://` only) and run before it.
- **Each script gets its own `<script>` tag**, so one broken script can't stop the others or the ad blocking.
- **Updates:** scripts are downloaded when added and again when you click **Update now**.
- **`GM_*` helpers:** `GM_getValue`/`GM_setValue`/`GM_deleteValue`/`GM_listValues`, `GM_addStyle`, `GM_addElement`, `GM_xmlhttpRequest`, `GM_openInTab`, `GM_setClipboard`, `GM_info`, `GM_log`, the `GM.*` promise versions, and `unsafeWindow`.

> Userscripts run as the page itself (like `@grant none`), not with extension powers. So `GM_xmlhttpRequest` follows the browser's normal cross-site rules, saved values live in the site's own storage (the site can read them), and `GM_registerMenuCommand` does nothing. Only add scripts you trust: they can see and change everything on the pages they run on.

## Security

- **The CA is unique to your PC.** It's generated on first run and never leaves it. Nothing in this repo can decrypt your traffic.
- **The CA's private key is encrypted with Windows DPAPI** for your user account, in `%LOCALAPPDATA%\CoffeeFilter\ca.key.dpapi`. Other Windows users can't read it.
- **The CA can only sign website certificates.** No other CAs (path length 0), no code signing or email (Server Authentication only).
- **Site certificates use their own key**, made fresh each time blocking starts and never saved, so the CA key is never used in a connection. They're valid for 7 days.
- **Trust is system-wide.** The CA is added to the machine Root store with a UAC prompt, because some Chromium browsers (like Dia) ignore the user store and would show "Your connection is not private". Each new install or update sets this up once on first launch, and renews the CA when it's about to expire or came from an older, less restricted version.
- **Removing the certificate deletes the key**, from both the machine and user stores, so a new one is made if you reinstall.
- **The proxy only listens on `127.0.0.1`**, and holds its port exclusively so no other program can answer in its place. Other devices on your network can't use it. The PAC script lives at a random address that changes every start, so web pages can't read it. A page can still notice that something answers on that port; it just can't learn more.
- **Web pages can't reach your PC or home network through it.** A public site name that points at a local address (a trick called DNS rebinding) is refused, for web requests, secure tunnels on other ports, and WebSockets alike, so a page can't read your router, NAS, or local apps through the proxy. Names that are local to begin with (`nas.local`, `192.168.1.1`) and excluded sites work as usual.
- **No other certificate goes through its admin prompt.** Before any admin step, Coffee Filter checks that `ca.cer` is a Coffee Filter certificate with today's limits, so a program that swaps in some other CA, or a copy of a real root, can't get it trusted or removed through the prompt you expect. (A program already running as you could make a look-alike, but it could also read the real key; see the trust boundary below.)
- **Injected code stays in its tag.** Anything in a filter rule or userscript that could end Coffee Filter's `<script>` early (`</script` in any letter case, `<!--`) is neutralized, so a list can't add code to pages beyond its scriptlets.
- **Web pages can't raise an admin prompt.** When an Install or Exclude button on a web page opens the app (to start it, not when it's already running), it skips the automatic certificate setup, and the Add or Exclude button stays off for a second so a stray keystroke can't accept it. A site name from an Exclude link must look like a site name before it's shown.
- **No control port.** The app talks to the core over a private pipe (stdin/stdout).
- **Real certificates are still checked.** The proxy verifies every site with Windows' own certificate check, exactly like your browser would (work or custom roots included). Sites with bad certificates fail instead of being silently trusted.
- **Excluded apps are never decrypted or filtered.** Add a program (`.exe`) or an installed Microsoft Store app under **Exclusions**, and every connection it makes passes through untouched. Coffee Filter asks Windows which program owns each new connection (only while the list isn't empty), and matches Store apps by package family name, so an exclusion survives app updates.
- **Your home network is never decrypted.** Private and local addresses (`192.168.x.x`, `10.x.x.x`, `*.local`, Tailscale `100.64.x.x`, ...) are tunneled untouched, as are excluded sites. Plain `http://` requests to excluded sites are left alone too: no blocking, no page changes.
- **Crash safe.** Your old proxy settings are saved first. They're restored when blocking stops, when the app quits or crashes, when Windows shuts down or you sign out, and otherwise on the next launch. If the core crashes, the app restarts it and turns blocking back on.

> Any tool like this can see your HTTPS traffic. That's how it works. Add banking or work sites to the exclusions if you'd rather they're never touched.

## Install

Download `CoffeeFilter_<version>_x64.msix` and `CoffeeFilter-signing.cer` from the [latest release](https://github.com/artistro08/coffee-filter/releases/latest).

1. Trust the signing certificate once, in PowerShell **as administrator**:

```powershell
Import-Certificate -FilePath .\CoffeeFilter-signing.cer -CertStoreLocation Cert:\LocalMachine\TrustedPeople
```

2. Double-click the `.msix` and click **Install**.
3. Open Coffee Filter and say **Yes** when it asks for admin to install its own certificate.

> The package is signed with a certificate made for this project rather than one from a certificate authority, which is why Windows needs step 1. The `.cer` holds no private key, and it's not the certificate Coffee Filter uses to read HTTPS traffic; that one is made on your PC at first launch.

## Resource Use

- **Core:** about 52 MB private memory with 15 lists on (Zen's set, ~970k network and cosmetic rules), 4 worker threads, near-zero CPU when idle.
- **Fast connections:** HTTP/2 to your browser and to sites that support it (many requests over one connection), no Nagle delay on small writes, and site certificates are reused per site instead of being cleared in bulk.
- **Off the hot path:** list compiling, certificate work, and file writes run off the threads that serve your browsing. List downloads run in parallel with a 60-second limit and a 64 MB size cap.
- **Fast restarts:** the compiled filter engine is saved (`engine.dat`). Starting takes about 0.3s instead of 0.8s, and it's rebuilt only when your lists change.
- **Per page:** about 60 KB of injected CSS, plus scriptlets only on sites that have them. Pages stream; only the bytes before `<head>` are held back.
- **App:** Native AOT (no .NET JIT), trimmed. About 145 MB while the window is open.
- **In the tray:** closing the window destroys it and trims the app's working set to about 11 MB while blocking runs in the background.
- **Lazy UI:** filter list cards are only created when you open a group, and the app only polls the core while its window is focused.

### Prerequisites

- Windows 10 2004 (build 19041) or newer
- [Rust](https://rustup.rs/)
- [.NET 10 SDK](https://dotnet.microsoft.com/download)
- [Windows SDK](https://developer.microsoft.com/windows/downloads/windows-sdk/) (its `rc.exe` puts the icon into the core)

### Build

Build the core first, since the app bundles it:

```bash
cd core
cargo build --release
cd ../app
dotnet build -c Release
```

Publishing builds the Native AOT app and a signed MSIX (`dotnet publish -c Release`; the AOT linker needs `vswhere.exe` from the Visual Studio Installer folder on your `PATH`). It ends up in `app/AppPackages/`. Double-click it to install.

> The MSIX is signed with a local `CN=Devin Green` code signing certificate kept in your Windows certificate store (`CurrentUser\My`, private key not exportable). Its thumbprint is set in `app/CoffeeFilter.csproj`. On another PC, make your own with `New-SelfSignedCertificate -Type CodeSigningCert -Subject "CN=<your name>"`, put its thumbprint in the csproj (or pass `-p:PackageCertificateThumbprint=<thumbprint>` to `dotnet publish`), set `Publisher` in `app/Package.appxmanifest` to the same subject, and add its public `.cer` to `LocalMachine\TrustedPeople` so Windows will install the package.

The package turns off MSIX registry and file virtualization (`unvirtualizedResources`). Without that, Windows would hand the app a private copy of the proxy settings and blocking would silently do nothing.

> Uninstalling the MSIX doesn't remove the Coffee Filter certificate. Click **Remove** next to Certificate in the app first.

### Tests

```bash
# Unit tests (CA and site certificates, DPAPI, filtering, hosts lists, cosmetic injection, CSP, PAC, config, validation)
cd core
cargo test --release

# End-to-end: runs the real proxy on port 18229 in a temp profile, with real Chrome when installed.
# Briefly changes the system proxy and needs internet plus the installed certificate.
cargo test --release -- --ignored

# App-to-core link (CoreClient) against the real core
cd ../tests/CoffeeFilter.Tests
dotnet test

# The whole window through Windows UI Automation, against the installed app.
# Opens pages, groups, and dialogs (canceled). One test excludes a Store app you haven't
# excluded and deletes it again, so your settings end as they began. Launching it turns
# blocking on if it was on last time, and the Store app list test needs a few Store apps installed.
cd ../CoffeeFilter.UITests
dotnet test
```

### Use

1. Open Coffee Filter. On first launch it asks for admin to install its certificate; say **Yes**. (If you said no, click **Install** on the Certificate card.)
2. Turn on **Ad blocking**.

That's it! Closing the window keeps blocking running in the background, with a tray icon (you can hide it in **App** settings). Opening Coffee Filter again brings the window back. **Quit** stops blocking and puts your proxy settings back.

## Known Limits

- **The app doesn't start with Windows.** When you open it, blocking comes back on unless you turned it off last time.
- **Firefox** trusts the Windows certificate store by default on Windows. If you turned that off, set `security.enterprise_roots.enabled` back to `true` in `about:config`.
- **Apps with certificate pinning** (some games, banking apps, Discord, etc.) will fail through the proxy. They show up under **Connection problems** with an **Exclude** button.
- **Strict revocation checking**: tools that hard-fail when revocation can't be checked (like Windows `curl` without `--ssl-no-revoke`) will fail. Browsers don't.
- **Windows Update and Microsoft Store downloads always connect directly.** Windows Update uses your proxy setting and refuses connections it can't verify end to end, so Microsoft's update and Store servers (`*.mp.microsoft.com`, `*.update.microsoft.com`, `*.windowsupdate.com`, ...) are never sent through or decrypted by Coffee Filter.
- **Sandboxed Store apps aren't filtered.** Windows doesn't let sandboxed Store apps (the Store, Xbox, Photos, ...) connect to `127.0.0.1`, so they go direct instead of through the proxy. Desktop apps from the Store (Spotify, WhatsApp, and other full-trust packages) do go through it, and can be added to **Excluded apps**.
- **Windhawk can freeze the Coffee Filter window.** With Windhawk injecting into Coffee Filter, opening an app from a launcher like Raycast while Coffee Filter's window has focus can leave the window not responding (blocking keeps running). Add `CoffeeFilter.exe` to Windhawk's exclusions to avoid it.
- **A fixed work proxy** (`ProxyEnable=1`) isn't used while blocking is on; traffic goes direct through Coffee Filter instead.
- **Your account is the trust boundary.** Any program running as you can change files in `%LOCALAPPDATA%\CoffeeFilter` (lists, userscripts) and so add code to pages, or decrypt the CA key. The CA is trusted by the whole PC while its key is readable by your account, so it's only as safe as your account. That's true of any app like this.
- **WebSockets go direct** from browsers (the PAC sends `ws:`/`wss:` straight out), so filter rules don't apply to them.
- **Work intranets** whose public names point at private addresses are refused through the proxy. Add them to the exclusions.
- **Generic class/id hide rules** (like `##.ad-banner` on every site) aren't applied yet. Site-specific rules and generic complex selectors are.
- **Numbers and times** use invariant formatting (the app is built with invariant globalization to save memory).

## Layout

- `core/` — Rust proxy (`coffee-filter-core.exe`)
    - `build.rs` — puts the coffee icon into `coffee-filter-core.exe`
    - `main.rs` — JSON-lines command loop, start/stop, shutdown handling
    - `ca.rs` — CA generation, DPAPI key storage, trust store install and renewal, site certificates
    - `filter.rs` — filter lists, adblock engine, request handler
    - `cosmetic.rs` — page injection, AdGuard syntax translation, scriptlet library, CSP nonces
    - `procedural.js` — in-page runner for advanced cosmetic rules
    - `userscripts.rs`, `userscripts.js` — userscript downloads, `@match` rules, and the in-page `GM_*` helpers
    - `apps.rs` — excluded apps: which program owns a connection
    - `blocked.html` — the blocked page
    - `problems.rs` — tracks connections Coffee Filter broke
    - `sysproxy.rs` — PAC script and Windows proxy settings with backup and restore
    - `config.rs` — settings and atomic file writes
- `app/` — WinUI 3 app, packaged as MSIX
- `tests/CoffeeFilter.Tests/` — C# tests for the app-to-core link
- `tests/CoffeeFilter.UITests/` — UI tests that drive the installed app (FlaUI)

### Files Used

- `%LOCALAPPDATA%\CoffeeFilter\config.json` — lists, userscripts, excluded sites and apps, port (a damaged copy is kept as `config.json.bad`)
- `%LOCALAPPDATA%\CoffeeFilter\ca.cer` — public CA certificate
- `%LOCALAPPDATA%\CoffeeFilter\ca.key.dpapi` — encrypted CA private key
- `%LOCALAPPDATA%\CoffeeFilter\lists\` — cached filter lists
- `%LOCALAPPDATA%\CoffeeFilter\userscripts\` — cached userscripts and their `@require` files
- `%LOCALAPPDATA%\CoffeeFilter\scriptlets.json` — pinned AdGuard scriptlet library
- `%LOCALAPPDATA%\CoffeeFilter\engine.dat`, `engine.key` — saved compiled filter engine
- `%LOCALAPPDATA%\CoffeeFilter\proxy-backup.json` — your proxy settings while blocking is on
- `%LOCALAPPDATA%\CoffeeFilter\problems.json` — connections Coffee Filter broke, for troubleshooting
- `%LOCALAPPDATA%\CoffeeFilter\app.json` — app preferences (tray icon, blocking on or off, certificate setup version)
- `%LOCALAPPDATA%\CoffeeFilter\app-errors.log` — app errors, if any
