// Userscript runner. Each userscript gets its own copy of the common
// Greasemonkey/Tampermonkey helpers (GM_*), and runs in the page like "@grant none".
(() => {
    // Our <script> tag's nonce, so GM_addStyle works under a strict Content-Security-Policy
    const nonce = document.currentScript ? document.currentScript.nonce : "";

    // Helpers For One Script
    const helpers = (name, version) => {
        const prefix = "us:" + name + ":";
        const GM_info = { script: { name, version }, scriptHandler: "Userscripts", version: "1" };

        const GM_getValue = (key, fallback) => {
            try {
                const value = localStorage.getItem(prefix + key);
                return value === null ? fallback : JSON.parse(value);
            } catch (e) {
                return fallback;
            }
        };
        const GM_setValue = (key, value) => {
            try { localStorage.setItem(prefix + key, JSON.stringify(value)); } catch (e) {}
        };
        const GM_deleteValue = (key) => {
            try { localStorage.removeItem(prefix + key); } catch (e) {}
        };
        const GM_listValues = () => {
            try {
                return Object.keys(localStorage).filter((k) => k.startsWith(prefix)).map((k) => k.slice(prefix.length));
            } catch (e) {
                return [];
            }
        };
        const GM_addStyle = (css) => {
            const style = document.createElement("style");
            style.nonce       = nonce;
            style.textContent = css;
            (document.head || document.documentElement).appendChild(style);
            return style;
        };
        const GM_addElement = (parent, tag, attributes) => {
            if (typeof parent === "string") {
                [parent, tag, attributes] = [document.head || document.documentElement, parent, tag];
            }
            const element = document.createElement(tag);
            element.nonce = nonce;
            for (const [key, value] of Object.entries(attributes || {})) {
                if (key === "textContent") { element.textContent = value; } else { element.setAttribute(key, value); }
            }
            return parent.appendChild(element);
        };
        const GM_openInTab = (url) => window.open(url, "_blank");
        const GM_setClipboard = (text) => navigator.clipboard && navigator.clipboard.writeText(String(text));
        const GM_registerMenuCommand = () => 0;
        const GM_unregisterMenuCommand = () => {};
        const GM_log = (...args) => console.log("[" + name + "]", ...args);

        // Runs as the page, so the browser's normal cross-site rules apply (no extension privileges)
        const GM_xmlhttpRequest = (details) => {
            const controller = new AbortController();
            fetch(details.url, { method: details.method || "GET", headers: details.headers, body: details.data, signal: controller.signal })
                .then(async (r) => {
                    const text     = await r.text();
                    const response = {
                        status:          r.status,
                        statusText:      r.statusText,
                        responseText:    text,
                        response:        details.responseType === "json" ? JSON.parse(text) : text,
                        finalUrl:        r.url,
                        readyState:      4,
                        responseHeaders: [...r.headers].map((h) => h.join(": ")).join("\r\n"),
                    };
                    details.onload && details.onload(response);
                    details.onloadend && details.onloadend(response);
                })
                .catch((e) => details.onerror && details.onerror(e));
            return { abort: () => controller.abort() };
        };

        const GM = {
            info:              GM_info,
            getValue:          async (k, d) => GM_getValue(k, d),
            setValue:          async (k, v) => GM_setValue(k, v),
            deleteValue:       async (k) => GM_deleteValue(k),
            listValues:        async () => GM_listValues(),
            addStyle:          GM_addStyle,
            addElement:        GM_addElement,
            openInTab:         GM_openInTab,
            setClipboard:      GM_setClipboard,
            registerMenuCommand: GM_registerMenuCommand,
            xmlHttpRequest:    GM_xmlhttpRequest,
        };
        return {
            GM, GM_info, GM_getValue, GM_setValue, GM_deleteValue, GM_listValues, GM_addStyle, GM_addElement,
            GM_openInTab, GM_setClipboard, GM_registerMenuCommand, GM_unregisterMenuCommand, GM_log, GM_xmlhttpRequest,
        };
    };

    // Run now (document-start) or once the page's HTML has loaded (document-end, the default)
    const when = (start, run) => {
        if (start || document.readyState !== "loading") { run(); } else { document.addEventListener("DOMContentLoaded", run, { once: true }); }
    };

    __CF_SCRIPTS__
})();
