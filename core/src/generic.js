// Generic Hiding
// Rules like `##.ad-banner` apply on every site, so they can't all be sent with the page.
// This asks Coffee Filter which ones match the class names and ids the page actually uses,
// as they appear, and adds what comes back to a style of its own. Names already asked about
// are never sent again, so a page that stops changing costs nothing.
(() => {
    const endpoint = __CF_ENDPOINT__;
    const nonce    = document.currentScript ? document.currentScript.nonce : "";
    const send     = window.fetch.bind(window);

    // Most names per request, and in all, so a page can't grow this without end
    const BATCH    = 500;
    const MAX_SEEN = 20000;

    const seen    = new Set();
    const roots   = new Set();
    const changed = new Set();
    let queued    = false;
    let style     = null;

    // Note a node's own names that weren't asked about yet
    function visit(node, classes, ids) {
        if (seen.size >= MAX_SEEN) {
            return;
        }
        if (node.classList) {
            for (const name of node.classList) {
                if (!seen.has("." + name)) {
                    seen.add("." + name);
                    classes.push(name);
                }
            }
        }
        if (typeof node.id === "string" && node.id && !seen.has("#" + node.id)) {
            seen.add("#" + node.id);
            ids.push(node.id);
        }
    }

    function add(selectors) {
        if (!Array.isArray(selectors) || !selectors.length) {
            return;
        }
        if (!style) {
            style       = document.createElement("style");
            style.nonce = nonce;
            (document.head || document.documentElement).appendChild(style);
        }
        style.textContent += selectors.map((s) => s + "{display:none!important}\n").join("");
    }

    function ask(classes, ids) {
        send(endpoint, { method: "POST", body: JSON.stringify({ url: location.href, classes, ids }), credentials: "omit", cache: "no-store" })
            .then((r) => (r.ok ? r.json() : []))
            .then(add, () => {});
    }

    function flush() {
        queued = false;
        const classes = [];
        const ids     = [];

        // New parts of the page with everything inside them; changed names on their own
        for (const root of roots) {
            visit(root, classes, ids);
            if (root.querySelectorAll) {
                for (const node of root.querySelectorAll("[class],[id]")) {
                    visit(node, classes, ids);
                }
            }
        }
        for (const node of changed) {
            visit(node, classes, ids);
        }
        roots.clear();
        changed.clear();

        for (let at = 0; at < Math.max(classes.length, ids.length); at += BATCH) {
            ask(classes.slice(at, at + BATCH), ids.slice(at, at + BATCH));
        }
    }

    function schedule() {
        if (!queued) {
            queued = true;
            setTimeout(flush, 100);
        }
    }

    // Bind to Event Listeners
    new MutationObserver((records) => {
        for (const record of records) {
            if (record.type === "attributes") {
                changed.add(record.target);
            } else {
                for (const node of record.addedNodes) {
                    if (node.nodeType === 1) {
                        roots.add(node);
                    }
                }
            }
        }
        if (roots.size || changed.size) {
            schedule();
        }
    }).observe(document.documentElement, { childList: true, subtree: true, attributes: true, attributeFilter: ["class", "id"] });

    roots.add(document.documentElement);
    schedule();
})();
