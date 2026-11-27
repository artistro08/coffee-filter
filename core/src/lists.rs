//! Filter lists: downloading them, caching them, and compiling them into the blocking engine.

use crate::{
    config::{Config, FilterList, data_dir, temp_path, write_atomic},
    cosmetic, prune,
};
use adblock::{
    Engine,
    lists::{FilterFormat, FilterSet, ParseOptions},
    resources::PermissionMask,
};
use hudsucker::{futures::future::join_all, hyper::body::Bytes};
use std::{
    collections::HashSet,
    fs,
    io,
    path::{Path, PathBuf},
    time::{Duration, UNIX_EPOCH},
};

/// Lists bigger than this are refused, so a bad custom list can't exhaust memory.
const MAX_LIST_BYTES: usize = 64 * 1024 * 1024;

/// A failed download or file write, as shown in the app.
pub type Error = Box<dyn std::error::Error + Send + Sync>;

// =========================================================================
// DOWNLOADS
// =========================================================================

/// Cached copy of a list. The name keeps it readable; the URL hash keeps two lists whose
/// names differ only in punctuation from sharing a file.
fn cache_path(list: &FilterList) -> io::Result<PathBuf> {
    let name: String = list.name.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    let hash = sha1_smol::Sha1::from(&list.url).digest().to_string();
    let dir  = data_dir()?.join("lists");
    fs::create_dir_all(&dir)?;
    Ok(dir.join(format!("{name}-{}.txt", &hash[..8])))
}

/// Delete cached list files no configured list uses (turned off ones keep theirs): lists
/// that were removed, files from before the URL hash was part of the name, and temp files
/// a crash left behind. Nothing else lives in the lists folder.
fn prune_lists(dir: &Path, keep: &[PathBuf]) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for path in entries.flatten().map(|e| e.path()).filter(|p| p.is_file() && !keep.contains(p)) {
        let _ = fs::remove_file(path);
    }
}

/// Start downloading one URL with a size cap. https only (redirects too, see `client`), so a
/// list or script can't be swapped on the way, even one saved by hand in config.json.
async fn start_download(client: &reqwest::Client, url: &str, max: usize) -> Result<reqwest::Response, Error> {
    if reqwest::Url::parse(url).map_or(true, |u| u.scheme() != "https") {
        return Err("only https:// addresses can be downloaded".into());
    }
    let response = client.get(url).send().await?.error_for_status()?;
    if response.content_length().is_some_and(|len| len as usize > max) {
        return Err("file is too large".into());
    }
    Ok(response)
}

/// Fetch one URL into memory, for small files (userscripts, the scriptlet library).
pub async fn fetch(client: &reqwest::Client, url: &str, max: usize) -> Result<Vec<u8>, Error> {
    let mut response = start_download(client, url, max).await?;
    let mut body     = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        body.extend_from_slice(&chunk);
        if body.len() > max {
            return Err("file is too large".into());
        }
    }
    Ok(body)
}

/// Download one URL straight to `path`, so a big list is never held in memory whole while
/// the others download too.
async fn download_to(client: &reqwest::Client, url: &str, max: usize, path: &Path) -> Result<(), Error> {
    let mut response = start_download(client, url, max).await?;
    save_chunks(path, max, async || Ok(response.chunk().await?)).await
}

/// Write chunks to `path` the way `write_atomic` does (temp file, on disk, then renamed), so
/// readers only ever see a whole file. Past `max` bytes, or on any error, the temp file is
/// removed and the old file stays.
async fn save_chunks(path: &Path, max: usize, mut next: impl AsyncFnMut() -> Result<Option<Bytes>, Error>) -> Result<(), Error> {
    use std::io::Write;
    let temp   = temp_path(path);
    let result = async {
        let mut file    = fs::File::create(&temp)?;
        let mut written = 0;
        while let Some(chunk) = next().await? {
            written += chunk.len();
            if written > max {
                return Err("file is too large".into());
            }
            file.write_all(&chunk)?;
        }
        file.sync_all()?;
        Ok::<_, Error>(())
    }
    .await;
    match result {
        Ok(()) => Ok(fs::rename(&temp, path)?),
        Err(e) => {
            let _ = fs::remove_file(&temp);
            Err(e)
        }
    }
}

/// Download client with time limits, so one slow server can't hold up the rest. Uses a
/// direct connection so it never loops through our own proxy.
pub fn client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .no_proxy()
        // https all the way: a redirect to plain http could let lists or scripts be swapped on the way
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.url().scheme() != "https" {
                attempt.error("redirected to a plain http address")
            } else if attempt.previous().len() >= 10 {
                attempt.error("too many redirects")
            } else {
                attempt.follow()
            }
        }))
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .build()
}

/// Download every enabled list and the scriptlet library, all at once.
pub async fn download_lists(config: &Config) -> Vec<String> {
    let client = match client() {
        Ok(c) => c,
        Err(e) => return vec![e.to_string()],
    };

    let lists = config.lists.iter().filter(|l| l.enabled).map(|list| {
        let client = &client;
        async move { download_to(client, &list.url, MAX_LIST_BYTES, &cache_path(list)?).await }
    });
    let library = async {
        let body = fetch(&client, cosmetic::SCRIPTLETS_URL, MAX_LIST_BYTES).await?;
        cosmetic::verify_library(&body)?;
        write_atomic(&data_dir()?.join("scriptlets.json"), &body)?;
        Ok::<_, Error>(())
    };

    let names = config.lists.iter().filter(|l| l.enabled).map(|l| l.name.clone());
    let (results, library) = tokio::join!(join_all(lists), library);

    let mut errors: Vec<String> = names.zip(results).filter_map(|(name, r)| r.err().map(|e| format!("{name}: {e}"))).collect();
    if let Err(e) = library {
        errors.push(format!("Scriptlets: {e}"));
    }
    errors
}

// =========================================================================
// ENGINE
// =========================================================================

/// What compiling the lists gives.
pub struct Built {
    pub engine: Engine,
    /// Rule count for the app.
    pub rules:  usize,
    /// The `$jsonprune` rules, which adblock-rust doesn't handle (see prune.rs).
    pub prune:  prune::Rules,
    /// Downloads that failed, shown in the app.
    pub errors: Vec<String>,
}

/// Build an engine from cached lists, downloading any that are missing. Compiling is
/// CPU-heavy, so it runs off the proxy's worker threads.
pub async fn build_engine(config: &Config) -> Built {
    let library = data_dir().map(|d| d.join("scriptlets.json"));
    let missing = config.lists.iter().any(|l| l.enabled && !cache_path(l).is_ok_and(|p| p.exists()))
        || !library.as_ref().is_ok_and(|p| p.exists());
    let errors = if missing { download_lists(config).await } else { vec![] };

    // Files no list uses anymore only take up disk
    if let Ok(dir) = data_dir() {
        let keep: Vec<PathBuf> = config.lists.iter().filter_map(|l| cache_path(l).ok()).collect();
        prune_lists(&dir.join("lists"), &keep);
    }

    let config = config.clone();
    let (engine, rules, prune) = tokio::task::spawn_blocking(move || compile(&config, library.ok()))
        .await
        .unwrap_or_else(|_| (Engine::default(), 0, String::new()));
    Built { engine, rules, prune: prune::Rules::parse(&prune), errors }
}

/// The engine, its rule count, and the lists' `$jsonprune` lines.
fn compile(config: &Config, library: Option<PathBuf>) -> (Engine, usize, String) {
    // Reuse the compiled engine when the enabled lists haven't changed; parsing ~1M rules costs about a second of CPU
    let key = engine_key(config);
    let (mut engine, rules, prune) = match load_cached_engine(&key) {
        Some(cached) => cached,
        None => {
            // Parsing ~1M rules is heavy and can wait, so it gives way to everything else on the PC
            let (engine, rules, prune) = crate::pressure::below_normal(|| {
                let (texts, rules, prune) = read_lists(config);
                (engine_from(texts), rules, prune)
            });

            if let Ok(dir) = data_dir() {
                let _ = write_atomic(&dir.join("engine.dat"), &engine.serialize());
                let _ = write_atomic(&dir.join("engine.prune"), prune.as_bytes());
                let _ = write_atomic(&dir.join("engine.key"), format!("{rules}\n{key}").as_bytes());
            }
            (engine, rules, prune)
        }
    };

    // Scriptlets aren't part of the saved engine; load them every start (only if the file is the pinned version)
    if let Some(json) = library.and_then(|p| fs::read(p).ok()).filter(|b| cosmetic::verify_library(b).is_ok()) {
        engine.use_resources(cosmetic::scriptlet_resources(&String::from_utf8_lossy(&json)));
    }
    (engine, rules, prune)
}

/// The enabled lists' cached text (each with whether its list is trusted), their rule count,
/// and their `$jsonprune` lines. Lists not downloaded yet are skipped.
pub fn read_lists(config: &Config) -> (Vec<(String, bool)>, usize, String) {
    let mut texts = vec![];
    let mut rules = 0;
    let mut prune = String::new();
    for list in config.lists.iter().filter(|l| l.enabled) {
        if let Ok(text) = cache_path(list).and_then(fs::read_to_string) {
            rules += text.lines().filter(|l| is_rule(l)).count();
            prune.push_str(&prune_lines(&text));
            texts.push((text, list.trusted));
        }
    }
    (texts, rules, prune)
}

/// A list's `$jsonprune` lines, one per line. adblock-rust drops them as unknown, so they're
/// kept here for prune.rs (which decides which of them it can apply).
fn prune_lines(text: &str) -> String {
    text.lines().filter(|l| l.contains("jsonprune") && !l.trim_start().starts_with('!')).flat_map(|l| [l, "\n"]).collect()
}

/// Identifies the exact list files an engine was built from (name, size, modified time).
fn engine_key(config: &Config) -> String {
    config
        .lists
        .iter()
        .filter(|l| l.enabled)
        .map(|l| {
            let meta = cache_path(l).and_then(fs::metadata).ok();
            let size = meta.as_ref().map_or(0, |m| m.len());
            let time = meta.and_then(|m| m.modified().ok()).and_then(|t| t.duration_since(UNIX_EPOCH).ok());
            format!("{}|{}|{size}|{}", l.name, l.trusted, time.map_or(0, |t| t.as_nanos()))
        })
        .collect::<Vec<_>>()
        .join("\n")
        // Bump when the way lists are compiled changes, so old engine.dat files are rebuilt
        // (6: $jsonprune rules are kept in engine.prune; 7: rules repeated across lists are left out)
        + "\nformat:7"
}

/// The saved engine, its rule count, and its `$jsonprune` lines, if they were built from
/// exactly these lists. A missing engine.prune means it has to be built again.
fn load_cached_engine(key: &str) -> Option<(Engine, usize, String)> {
    let dir          = data_dir().ok()?;
    let saved        = fs::read_to_string(dir.join("engine.key")).ok()?;
    let (rules, old) = saved.split_once('\n')?;
    if old != key {
        return None;
    }
    let prune      = fs::read_to_string(dir.join("engine.prune")).ok()?;
    let mut engine = Engine::default();
    engine.deserialize(&fs::read(dir.join("engine.dat")).ok()?).ok()?;
    Some((engine, rules.parse().ok()?, prune))
}

/// Compile list texts (each with whether its list is trusted) into an engine. Network and
/// cosmetic rules are both kept; AdGuard-only syntax is translated first.
///
/// adblock-rust keeps every copy of a rule, and lists share a lot of theirs (AdGuard DNS
/// filter repeats most of AdGuard's Base and Spyware filters, hosts files repeat each other):
/// about a quarter of the default lists' rules. A rule an earlier list already has is left out,
/// which gives the same blocking from a smaller engine (less memory for as long as it runs).
pub fn engine_from(lists: Vec<(String, bool)>) -> Engine {
    // Compared as adblock-rust reads them: hosts files as they are, the rest translated
    let lists: Vec<(String, bool, bool)> = lists
        .into_iter()
        .map(|(text, trusted)| {
            let hosts = is_hosts(&text);
            let text  = if hosts { text } else { cosmetic::translate_adguard(text) };
            (text, hosts, trusted)
        })
        .collect();

    let mut set  = FilterSet::new(false);
    let mut seen = HashSet::new();
    for (text, hosts, trusted) in &lists {
        let unique: String = text.lines().filter(|line| rule_key(line, *hosts, *trusted).is_none_or(|key| seen.insert(key))).flat_map(|l| [l, "\n"]).collect();
        let format         = if *hosts { FilterFormat::Hosts } else { FilterFormat::Standard };
        let permissions    = if *trusted { cosmetic::TRUSTED } else { PermissionMask::default() };
        set.add_filter_list(unique, ParseOptions { format, permissions, ..Default::default() });
    }

    // The originals aren't needed for compiling, which is when memory peaks
    drop(seen);
    drop(lists);
    Engine::new_with_filter_set(set)
}

/// Whether a list is a hosts file (`0.0.0.0 domain`), from its first rule.
fn is_hosts(text: &str) -> bool {
    let first = text.lines().map(|l| l.trim().trim_start_matches('\u{feff}')).find(|l| !l.is_empty() && !l.starts_with(['#', '!', '[']));
    first.is_some_and(|l| l.starts_with("0.0.0.0") || l.starts_with("127.0.0.1") || l.starts_with("::"))
}

/// What makes two rules the same (see `rule_key`).
#[derive(PartialEq, Eq, Hash)]
enum RuleKey<'a> {
    /// `||site^`, or a hosts file's `0.0.0.0 site`, which adblock-rust reads as exactly that.
    Site(&'a str),
    /// Any other network rule, which means the same in every list.
    Network(&'a str),
    /// Cosmetic rules and scriptlets mean the same only between lists of the same trust: a
    /// trusted list's scriptlets can do more.
    Cosmetic(bool, &'a str),
}

/// A line's rule, to compare with other lists' rules, or None for anything else (comments,
/// headers, and lines only adblock-rust's own parser should judge), which is always kept.
fn rule_key(line: &str, hosts: bool, trusted: bool) -> Option<RuleKey<'_>> {
    let line = line.trim();
    if hosts {
        // Only an address and a plain name; comments and anything unusual stay as written
        let mut parts = line.split_whitespace();
        return match (parts.next(), parts.next(), parts.next()) {
            (Some(ip), Some(site), None) if ip.parse::<std::net::IpAddr>().is_ok() && plain_site(site) => Some(RuleKey::Site(site)),
            _ => None,
        };
    }
    if !is_rule(line) {
        return None;
    }
    // Every cosmetic rule has a `#`; a network rule with one is only compared more narrowly
    if line.contains('#') {
        return Some(RuleKey::Cosmetic(trusted, line));
    }
    match line.strip_prefix("||").and_then(|l| l.strip_suffix('^')) {
        Some(site) if plain_site(site) => Some(RuleKey::Site(site)),
        _ => Some(RuleKey::Network(line)),
    }
}

/// A lowercase site name with a dot in it and nothing else, which both rule forms read the
/// same way (`localhost`, names with uppercase or odd characters are left to adblock-rust).
fn plain_site(site: &str) -> bool {
    site.contains('.')
        && !site.starts_with('.')
        && !site.ends_with('.')
        && site.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'-' | b'_'))
}

/// Rough count for the UI: skips blanks, comments, and headers.
fn is_rule(line: &str) -> bool {
    let line = line.trim();
    !line.is_empty() && !line.starts_with(['!', '[']) && !(line.starts_with('#') && !line.starts_with("##") && !line.starts_with("#@#"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use adblock::request::Request as AdRequest;

    #[tokio::test]
    async fn downloads_go_straight_to_disk_and_stay_whole() {
        let dir  = std::env::temp_dir().join(format!("cf-save-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("list.txt");
        fs::write(&path, "old").unwrap();

        // Too big partway through: the old file stays and no temp file is left
        let mut chunks = vec![Bytes::from("12345"), Bytes::from("67890")].into_iter();
        assert!(save_chunks(&path, 8, async || Ok(chunks.next())).await.is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "old");
        assert!(!temp_path(&path).exists());

        // Within the cap: replaced whole
        let mut chunks = vec![Bytes::from("new "), Bytes::from("list")].into_iter();
        save_chunks(&path, 8, async || Ok(chunks.next())).await.unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "new list");
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn fetch_refuses_plain_http() {
        let client = client().unwrap();
        for url in ["http://example.com/list.txt", "HTTP://example.com", "file:///C:/secret.txt", "not a url"] {
            let error = fetch(&client, url, 1024).await.unwrap_err();
            assert!(error.to_string().contains("only https://"), "{url}: {error}");
        }
    }

    #[test]
    fn blocks_listed_and_passes_clean() {
        let mut set = FilterSet::new(false);
        set.add_filter_list("||ads.example.com^\n@@||ads.example.com/ok.js".into(), ParseOptions::default());
        let engine = Engine::new_with_filter_set(set);
        let check  = |u: &str| engine.check_network_request(&AdRequest::new(u, "https://site.com", "script", "GET").unwrap()).should_block();

        assert!(check("https://ads.example.com/x.js"));
        assert!(!check("https://ads.example.com/ok.js"));
        assert!(!check("https://site.com/app.js"));
    }

    #[test]
    fn hosts_file_lists_block_their_domains() {
        let engine = engine_from(vec![("# comment\n0.0.0.0 tracker.example.net\n127.0.0.1 ads.example.org".into(), false)]);
        let check  = |u: &str| engine.check_network_request(&AdRequest::new(u, "https://site.com", "image", "GET").unwrap()).should_block();

        assert!(check("https://tracker.example.net/pixel.gif"));
        assert!(check("https://ads.example.org/banner.png"));
        assert!(!check("https://example.net/"));
    }

    #[test]
    fn saved_engine_blocks_the_same() {
        let built      = engine_from(vec![("||ads.example.com^".into(), false), ("0.0.0.0 hosts.example.net".into(), false)]);
        let mut engine = Engine::default();
        engine.deserialize(&built.serialize()).unwrap();
        let check = |u: &str| engine.check_network_request(&AdRequest::new(u, "https://site.com", "script", "GET").unwrap()).should_block();

        assert!(check("https://ads.example.com/a.js"));
        assert!(check("https://hosts.example.net/a.js"));
        assert!(!check("https://site.com/a.js"));
    }

    #[test]
    fn rules_repeated_across_lists_are_left_out() {
        let lists = || vec![
            ("||ads.example.com^\n||cdn.example.com/ad.js\nexample.com##.ad\n! comment".to_string(), false),
            ("! comment\n||ads.example.com^\n||cdn.example.com/ad.js\nexample.com##.ad\n||new.example.org^".to_string(), false),
            ("# hosts\n0.0.0.0 ads.example.com\n0.0.0.0 tracker.example.net\n127.0.0.1 localhost".to_string(), false),
        ];

        // Each repeat costs space when kept
        let mut set = FilterSet::new(false);
        for (text, _) in lists() {
            let format = if is_hosts(&text) { FilterFormat::Hosts } else { FilterFormat::Standard };
            set.add_filter_list(text, ParseOptions { format, ..Default::default() });
        }
        let every_copy = Engine::new_with_filter_set(set).serialize().len();
        let engine     = engine_from(lists());
        assert!(engine.serialize().len() < every_copy, "repeats are left out");

        // And everything still blocks and hides as before
        let check = |u: &str| engine.check_network_request(&AdRequest::new(u, "https://site.com", "script", "GET").unwrap()).should_block();
        for url in ["https://ads.example.com/a.js", "https://cdn.example.com/ad.js", "https://new.example.org/x", "https://tracker.example.net/p"] {
            assert!(check(url), "{url}");
        }
        assert!(!check("https://site.com/a.js"));
        assert!(engine.url_cosmetic_resources("https://example.com/").hide_selectors.contains(".ad"));
    }

    #[test]
    fn a_hosts_line_is_the_same_rule_as_its_site_rule() {
        use adblock::filters::network::NetworkFilter;
        let site  = NetworkFilter::parse("||ads.example.com^", false, Default::default()).unwrap();
        let hosts = NetworkFilter::parse_hosts_style("ads.example.com", false).unwrap();
        assert_eq!(site.mask, hosts.mask, "same request types and parties");
        assert_eq!(site.get_id(), hosts.get_id());
        assert!(rule_key("0.0.0.0 ads.example.com", true, false) == rule_key("||ads.example.com^", false, true));

        // Only plain names: anything else is left to adblock-rust as written
        for (line, hosts) in [("127.0.0.1 localhost", true), ("# 0.0.0.0 a.com", true), ("0.0.0.0 a.com # note", true), ("0.0.0.0 Ads.example.com", true), ("! comment", false), ("", false)] {
            assert!(!matches!(rule_key(line, hosts, false), Some(RuleKey::Site(_))), "{line}");
        }
        assert!(rule_key("||ads.example.com^$third-party", false, false) == Some(RuleKey::Network("||ads.example.com^$third-party")));
    }

    #[test]
    fn trusted_scriptlets_survive_an_untrusted_copy() {
        // The same scriptlet in an untrusted list first, then a trusted one: only the trusted copy may run it
        let rule       = "a.com##+js(trusted-click-element, button)";
        let mut engine = engine_from(vec![(rule.into(), false), (rule.into(), true)]);
        let library    = r#"{"version":"1","scriptlets":[{"names":["trusted-click-element"],"scriptlet":"function t(source,args){window.__clicked=args}"}]}"#;
        engine.use_resources(cosmetic::scriptlet_resources(library));
        let tags = cosmetic::page_tags(&engine, &[], "https://a.com/", "N", None).expect("the trusted copy runs");
        assert!(String::from_utf8_lossy(&tags).contains("cf_trusted_click_element"));
    }

    #[test]
    fn a_saved_engine_with_a_million_rules_loads() {
        // adblock-rust 0.13.3 refused to load a saved engine past a million rules (TooManyTables),
        // so every start compiled the lists again (1.5 s of CPU, 200 MB at the peak)
        let list: String = (0..1_100_000).map(|i| format!("||s{i}.example.com^\n")).collect();
        let saved        = engine_from(vec![(list, false)]).serialize();
        let mut engine   = Engine::default();
        engine.deserialize(&saved).expect("a big saved engine loads");
        assert!(engine.check_network_request(&AdRequest::new("https://s1099999.example.com/", "https://site.com", "script", "GET").unwrap()).should_block());
    }

    #[test]
    fn standard_lists_still_parse_as_standard() {
        let engine = engine_from(vec![("! Title: test\n||ads.example.com^$third-party".into(), false)]);
        let req    = AdRequest::new("https://ads.example.com/a.js", "https://site.com", "script", "GET").unwrap();
        assert!(engine.check_network_request(&req).should_block());
    }

    #[test]
    fn stale_list_files_are_pruned() {
        let dir = std::env::temp_dir().join(format!("cf-lists-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        for name in ["EasyList-71bee46f.txt", "EasyList.txt", "Removed_List-00000000.txt", "EasyList-71bee46f.txt.tmp"] {
            fs::write(dir.join(name), b"x").unwrap();
        }

        prune_lists(&dir, &[dir.join("EasyList-71bee46f.txt")]);

        let left: Vec<String> = fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(left, ["EasyList-71bee46f.txt"], "only files of current lists stay");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn jsonprune_lines_are_kept_for_prune() {
        let list = "! Title: x\n||ads.com^\n/i/api/graphql/*/HomeTimeline$jsonprune=\\$.a\n! /old$jsonprune=\\$.b\n@@||a.com^$jsonprune\n";
        assert_eq!(prune_lines(list), "/i/api/graphql/*/HomeTimeline$jsonprune=\\$.a\n@@||a.com^$jsonprune\n");
        assert_eq!(prune::Rules::parse(&prune_lines(list)).count(), 1);
    }

    #[test]
    fn rule_count_skips_comments() {
        let lines = ["! comment", "# hosts comment", "[Adblock Plus 2.0]", "", "example.com##.ad", "##.generic", "example.com#@#.ad", "||ads.com^", "0.0.0.0 x.com"];
        assert_eq!(lines.iter().filter(|l| is_rule(l)).count(), 5);
    }
}
