//! Benchmarks for the core's hot paths: the block decision every request waits on, the tags
//! added to every page, generic class/id rules, `$jsonprune` on X's timeline, AdGuard syntax
//! translation, address checks, and building and loading the engine.
//!
//! Uses this PC's filter lists, copied once (read-only) from %LOCALAPPDATA%\CoffeeFilter to
//! %TEMP%\cf-bench, so the numbers match what the proxy really runs with. Without them a small
//! built-in list is used. Run with `cargo bench` (local only, nothing is uploaded).
//!
//! The source files are included as modules of this bench, the same as in the core exe, so
//! `crate::` paths inside them resolve the same way.

#![allow(dead_code, unused_imports)]

#[path = "../src/apps.rs"]
mod apps;
#[path = "../src/ca.rs"]
mod ca;
#[path = "../src/config.rs"]
mod config;
#[path = "../src/cosmetic.rs"]
mod cosmetic;
#[path = "../src/filter.rs"]
mod filter;
#[path = "../src/lists.rs"]
mod lists;
#[path = "../src/log.rs"]
mod log;
#[path = "../src/net.rs"]
mod net;
#[path = "../src/pressure.rs"]
mod pressure;
#[path = "../src/problems.rs"]
mod problems;
#[path = "../src/prune.rs"]
mod prune;
#[path = "../src/sysproxy.rs"]
mod sysproxy;
#[path = "../src/upstream.rs"]
mod upstream;
#[path = "../src/userscripts.rs"]
mod userscripts;

use adblock::{Engine, request::Request as AdRequest};
use divan::{AllocProfiler, Bencher, black_box};
use http_body_util::BodyExt;
use hudsucker::{Body, hyper::Response};
use serde_json::json;
use std::{
    fs,
    path::PathBuf,
    sync::{Arc, LazyLock},
};

#[global_allocator]
static ALLOC: AllocProfiler = AllocProfiler::system();

// =========================================================================
// INPUTS
// =========================================================================

/// Rules used when this PC has no downloaded lists.
const FALLBACK_LIST: &str = "||doubleclick.net^\n||google-analytics.com^\n||googletagmanager.com^$third-party\n##.ad-banner\n###ad-slot\nreddit.com##shreddit-ad-post\nyoutube.com##+js(set-constant, ytInitialPlayerResponse.adPlacements, undefined)\n";

/// The real lists' text (with whether each is trusted), rule count, and `$jsonprune` lines.
type Lists = (Vec<(String, bool)>, usize, String);

/// This PC's lists (see the file header).
static LISTS: LazyLock<Lists> = LazyLock::new(|| {
    let (config, _) = config::Config::load();
    let (texts, rules, prune) = lists::read_lists(&config);
    if texts.is_empty() {
        return (vec![(FALLBACK_LIST.to_string(), false)], 7, String::new());
    }
    (texts, rules, prune)
});

/// The engine the proxy would run, with the scriptlet library when it's there.
static ENGINE: LazyLock<Engine> = LazyLock::new(|| {
    let mut engine = lists::engine_from(LISTS.0.clone());
    if let Some(json) = config::data_dir().ok().and_then(|d| fs::read(d.join("scriptlets.json")).ok()).filter(|b| cosmetic::verify_library(b).is_ok()) {
        engine.use_resources(cosmetic::scriptlet_resources(&String::from_utf8_lossy(&json)));
    }
    engine
});

static SERIALIZED: LazyLock<Vec<u8>> = LazyLock::new(|| ENGINE.serialize());

/// Requests a typical news page makes: (url, page, Sec-Fetch-Dest type).
const REQUESTS: [(&str, &str, &str); 12] = [
    ("https://www.cnn.com/", "https://www.cnn.com/", "document"),
    ("https://www.cnn.com/media/sites/cnn/cnn-fusion.js", "https://www.cnn.com/", "script"),
    ("https://media.cnn.com/api/v1/images/stellar/prod/hero.jpg?c=16x9&q=h_540,w_960,c_fill", "https://www.cnn.com/", "image"),
    ("https://securepubads.g.doubleclick.net/tag/js/gpt.js", "https://www.cnn.com/", "script"),
    ("https://www.googletagmanager.com/gtm.js?id=GTM-ABC123", "https://www.cnn.com/", "script"),
    ("https://www.google-analytics.com/g/collect?v=2&tid=G-XYZ&cid=123.456&en=page_view", "https://www.cnn.com/", "xmlhttprequest"),
    ("https://fonts.gstatic.com/s/roboto/v30/KFOmCnqEu92Fr1Mu4mxK.woff2", "https://www.cnn.com/", "font"),
    ("https://cdn.jsdelivr.net/npm/react@18.3.1/umd/react.production.min.js", "https://www.cnn.com/", "script"),
    ("https://c.amazon-adsystem.com/aax2/apstag.js", "https://www.cnn.com/", "script"),
    ("https://www.reddit.com/svc/shreddit/feeds/popular-feed?after=t3_abc", "https://www.reddit.com/", "xmlhttprequest"),
    ("https://x.com/i/api/graphql/abc123/HomeTimeline?variables=%7B%22count%22%3A20%7D", "https://x.com/home", "xmlhttprequest"),
    ("https://i.ytimg.com/vi/dQw4w9WgXcQ/hqdefault.jpg", "https://www.youtube.com/", "image"),
];

/// Pages that get cosmetic filtering.
const PAGES: [&str; 4] = ["https://www.youtube.com/watch?v=dQw4w9WgXcQ", "https://www.reddit.com/r/rust/", "https://www.cnn.com/", "https://x.com/home"];

/// What generic.js sends for a busy page: 400 class names and 60 ids, some of them ad-like.
fn seen_names() -> Vec<u8> {
    let mut classes: Vec<String> = (0..380).map(|i| format!("css-{i:x}k9 layout-col-{i}")).flat_map(|s| s.split(' ').map(str::to_string).collect::<Vec<_>>()).take(380).collect();
    classes.extend(["ad", "ad-banner", "adsbygoogle", "sponsored", "promo", "banner-ad", "ad-container", "advert", "dfp-ad", "outbrain", "taboola", "native-ad", "sidebar-ad", "top-ad", "ad-slot", "adunit", "ad-wrapper", "teaser", "header", "footer"].map(String::from));
    let mut ids: Vec<String> = (0..50).map(|i| format!("section-{i}")).collect();
    ids.extend(["ad-slot", "google_ads_iframe", "taboola-below", "banner", "sponsor", "main", "nav", "content", "sidebar", "footer"].map(String::from));
    serde_json::to_vec(&json!({ "url": "https://www.cnn.com/", "classes": classes, "ids": ids })).unwrap()
}

/// An X home timeline the size X sends (about 40 posts, a few of them promoted).
fn home_timeline() -> Vec<u8> {
    let entry = |i: usize, promoted: bool| {
        let id = if promoted { format!("promoted-tweet-{i}-abc") } else { format!("tweet-{i}") };
        let mut item = json!({
            "itemType": "TimelineTweet",
            "tweet_results": { "result": {
                "rest_id": i.to_string(),
                "core": { "user_results": { "result": { "rest_id": "44196397", "legacy": { "name": "Some User", "screen_name": "someuser", "description": "x".repeat(160), "followers_count": 1234567, "profile_image_url_https": "https://pbs.twimg.com/profile_images/1/abc_normal.jpg" } } } },
                "legacy": {
                    "full_text": "Post text that is about as long as a real one, with a link https://t.co/abcdefghij and some #tags".repeat(2),
                    "entities": { "hashtags": [{ "text": "tags", "indices": [80, 85] }], "urls": [{ "url": "https://t.co/abcdefghij", "expanded_url": "https://example.com/article", "indices": [50, 73] }] },
                    "favorite_count": 1000 + i, "retweet_count": 10 + i, "reply_count": i, "created_at": "Wed Nov 26 18:43:45 +0000 2026"
                },
                "views": { "count": "123456", "state": "EnabledWithCount" }
            } }
        });
        if promoted {
            item["promotedMetadata"] = json!({ "advertiser_results": { "result": { "rest_id": "99" } }, "impressionId": "abc", "disclosureType": "NoDisclosure" });
        }
        json!({ "entryId": id, "sortIndex": (1_900_000_000 - i).to_string(), "content": { "entryType": "TimelineTimelineItem", "itemContent": item } })
    };
    let entries: Vec<_> = (0..40).map(|i| entry(i, i % 13 == 5)).collect();
    serde_json::to_vec(&json!({ "data": { "home": { "home_timeline_urt": { "instructions": [
        { "type": "TimelineClearCache" },
        { "type": "TimelineAddEntries", "entries": entries }
    ] } } } }))
    .unwrap()
}

/// A news-site-sized page: a long `<head>`, then about 150 KB of markup.
fn sample_page() -> String {
    let head: String = (0..40).map(|i| format!("<meta name=\"m{i}\" content=\"{}\">\n", "v".repeat(60))).collect();
    let body: String = (0..1500).map(|i| format!("<div class=\"card css-{i:x}\"><a href=\"/story/{i}\">Story {i}</a><p>{}</p></div>\n", "text ".repeat(10))).collect();
    format!("<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\"><title>News</title>\n{head}</head><body>{body}</body></html>")
}

// =========================================================================
// BENCHMARKS
// =========================================================================

fn main() {
    // Point the core's data folder at a scratch copy of this PC's lists, never the real one
    let source = std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("CoffeeFilter"));
    let copy   = std::env::temp_dir().join("cf-bench");
    let target = copy.join("CoffeeFilter");
    if let Some(source) = source.filter(|s| s.join("config.json").exists())
        && !target.join("config.json").exists()
    {
        let _ = fs::create_dir_all(target.join("lists"));
        for name in ["config.json", "scriptlets.json"] {
            let _ = fs::copy(source.join(name), target.join(name));
        }
        for entry in fs::read_dir(source.join("lists")).into_iter().flatten().flatten() {
            let _ = fs::copy(entry.path(), target.join("lists").join(entry.file_name()));
        }
    }
    // SAFETY: nothing else is running yet; set before any thread starts
    unsafe { std::env::set_var("LOCALAPPDATA", &copy) };
    divan::main();
}

/// The block decision for one request, as `Blocker::filter_request` makes it.
#[divan::bench(args = REQUESTS)]
fn block_decision(bencher: Bencher, (url, source, kind): (&str, &str, &str)) {
    let engine = &*ENGINE;
    bencher.bench(|| AdRequest::new(black_box(url), black_box(source), kind, "GET").is_ok_and(|r| engine.check_network_request(&r).should_block()));
}

/// The `<style>` and `<script>` tags for one page (site rules, scriptlets, procedural rules).
#[divan::bench(args = PAGES)]
fn page_tags(bencher: Bencher, url: &str) {
    let engine = &*ENGINE;
    bencher.bench(|| cosmetic::page_tags(engine, &[], black_box(url), "0123456789abcdef0123456789abcdef", Some("/.coffee-filter-x/generic")));
}

/// adblock-rust's own part of `page_tags`: the rules for one page, before they become tags.
#[divan::bench(args = PAGES)]
fn cosmetic_resources(bencher: Bencher, url: &str) {
    let engine = &*ENGINE;
    bencher.bench(|| engine.url_cosmetic_resources(black_box(url)));
}

/// A whole page through `cosmetic::inject`, streamed and collected.
#[divan::bench]
fn inject_page(bencher: Bencher) {
    let engine  = &*ENGINE;
    let html    = sample_page();
    let trace   = log::Trace::default();
    let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
    let page    = cosmetic::Page { url: "https://www.cnn.com/", navigation: true, endpoint: "/.coffee-filter-x/generic" };
    bencher
        .with_inputs(|| Response::builder().header("content-type", "text/html; charset=utf-8").body(Body::from(html.clone())).unwrap())
        .bench_values(|res| runtime.block_on(async { cosmetic::inject(res, engine, Vec::new, &page, &trace).into_body().collect().await.unwrap().to_bytes() }));
}

/// Generic class/id selectors for the names a busy page reports.
#[divan::bench]
fn generic_selectors(bencher: Bencher) {
    let engine = &*ENGINE;
    let body   = seen_names();
    bencher.bench(|| cosmetic::generic_selectors(engine, "www.cnn.com", black_box(&body)));
}

/// Matching a request against the `$jsonprune` rules (runs on every allowed request).
#[divan::bench(args = REQUESTS)]
fn prune_match(bencher: Bencher, (url, source, kind): (&str, &str, &str)) {
    let rules = Arc::new(prune::Rules::parse(&LISTS.2));
    bencher.bench(|| prune::Matched::find(&rules, black_box(url), source, kind).is_some());
}

/// Pruning promoted posts out of an X home timeline response.
#[divan::bench]
fn prune_home_timeline(bencher: Bencher) {
    let text    = if LISTS.2.contains("HomeTimeline") { LISTS.2.clone() } else { r"/i/api/graphql/*/HomeTimeline$jsonprune=\$.data.home.home_timeline_urt.instructions..entries..[?(key-substr 'entryId' 'promoted-tweet-')],domain=twitter.com|x.com".to_string() };
    let rules   = Arc::new(prune::Rules::parse(&text));
    let matched = prune::Matched::find(&rules, REQUESTS[10].0, REQUESTS[10].1, REQUESTS[10].2).expect("HomeTimeline rules");
    let body    = home_timeline();
    let trace   = log::Trace::default();
    let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
    bencher
        .with_inputs(|| Response::builder().header("content-type", "application/json").body(Body::from(body.clone())).unwrap())
        .bench_values(|res| runtime.block_on(async { prune::prune_response(res, &matched, &trace).await.into_body().collect().await.unwrap().to_bytes() }));
}

/// AdGuard syntax translation of the biggest list that needs it.
#[divan::bench(sample_count = 10, sample_size = 1)]
fn translate_adguard(bencher: Bencher) {
    let text = LISTS.0.iter().map(|(t, _)| t).filter(|t| t.contains("#%#") || t.contains("#$#")).max_by_key(|t| t.len()).cloned().unwrap_or_else(|| FALLBACK_LIST.to_string());
    bencher.with_inputs(|| text.clone()).bench_values(cosmetic::translate_adguard);
}

/// The checks every connection gets before it's decrypted (private, Windows, Zen, exclusions).
#[divan::bench]
fn host_checks(bencher: Bencher) {
    let hosts      = ["www.cnn.com", "securepubads.g.doubleclick.net", "router", "192.168.1.1", "nas.local", "update.microsoft.com", "accounts.google.com", "x.com"];
    let exclusions = ["bank.com".to_string(), "nextdns.io".to_string(), "example.org".to_string()];
    bencher.bench(|| {
        hosts.iter().filter(|h| {
            net::is_private_host(black_box(h))
                || net::on_domain_list(&sysproxy::SYSTEM_HOSTS, h)
                || net::on_domain_list(sysproxy::never_decrypted(), h)
                || net::on_domain_list(&exclusions, h)
        })
        .count()
    });
}

/// Compiling every list into an engine (an Update, or a first start).
#[divan::bench(sample_count = 3, sample_size = 1)]
fn engine_build(bencher: Bencher) {
    let texts = LISTS.0.clone();
    bencher.with_inputs(|| texts.clone()).bench_values(lists::engine_from);
}

/// Loading the saved engine (every start).
#[divan::bench(sample_count = 5, sample_size = 1)]
fn engine_load(bencher: Bencher) {
    let bytes = &*SERIALIZED;
    // A saved engine that won't load means every start compiles the lists again
    if let Err(e) = Engine::default().deserialize(bytes) {
        println!("engine.dat doesn't load: {e:?}");
    }
    bencher.bench(|| {
        let mut engine = Engine::default();
        let _ = engine.deserialize(black_box(bytes));
        engine
    });
}

/// Size of the saved engine, which is about what it takes in memory once loaded.
#[divan::bench(sample_count = 1, sample_size = 1)]
fn engine_size(bencher: Bencher) {
    println!("engine.dat: {:.1} MB, {} rules", SERIALIZED.len() as f64 / 1_048_576.0, LISTS.1);
    bencher.bench(|| SERIALIZED.len());
}
