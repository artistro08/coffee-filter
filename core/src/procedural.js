// Runs procedural cosmetic rules (uBO/AdGuard extended syntax) that plain CSS can't express.
// The rules placeholder below is replaced with a JSON array of { selector: [{ type, arg }], action: { type, arg } | null }.
(() => {
    const rules = __CF_RULES__;
    if (!rules.length) return;

    // Text or /regex/flags argument to a predicate
    const matcher = (arg) => {
        const m = /^\/(.+)\/([a-z]*)$/s.exec(arg);
        if (m) {
            try { const re = new RegExp(m[1], m[2]); return (s) => re.test(s); } catch (e) { return () => false; }
        }
        return (s) => s.includes(arg);
    };

    // `prop: value` for matches-css, value may be a regex
    const cssMatcher = (arg) => {
        const i = arg.indexOf(':');
        const prop = arg.slice(0, i).trim();
        const test = matcher(arg.slice(i + 1).trim());
        return { prop, test };
    };

    // `name="value"` for matches-attr, both sides may be regexes or quoted
    const attrMatcher = (arg) => {
        const unquote = (s) => s.trim().replace(/^"(.*)"$/s, '$1');
        const i = arg.indexOf('=');
        const name = matcher(unquote(i < 0 ? arg : arg.slice(0, i)));
        const value = i < 0 ? () => true : matcher(unquote(arg.slice(i + 1)));
        return (el) => [...el.attributes].some((a) => name(a.name) && value(a.value));
    };

    const step = (nodes, op, first) => {
        const out = new Set();
        const add = (el) => el && el.nodeType === 1 && out.add(el);
        switch (op.type) {
            case 'css-selector': {
                const sel = op.arg.trim();
                if (first) { document.querySelectorAll(sel).forEach(add); break; }
                for (const n of nodes) {
                    if (sel[0] === '+') add(n.nextElementSibling?.matches(sel.slice(1).trim()) ? n.nextElementSibling : null);
                    else if (sel[0] === '~') { for (let s = n.nextElementSibling; s; s = s.nextElementSibling) if (s.matches(sel.slice(1).trim())) add(s); }
                    else n.querySelectorAll(':scope ' + sel).forEach(add);
                }
                break;
            }
            case 'has-text': { const t = matcher(op.arg); nodes.forEach((n) => t(n.textContent || '') && add(n)); break; }
            case 'min-text-length': { const len = +op.arg; nodes.forEach((n) => (n.textContent || '').length >= len && add(n)); break; }
            case 'matches-attr': { const t = attrMatcher(op.arg); nodes.forEach((n) => t(n) && add(n)); break; }
            case 'matches-path': { if (matcher(op.arg)(location.pathname + location.search)) nodes.forEach(add); break; }
            case 'matches-css':
            case 'matches-css-before':
            case 'matches-css-after': {
                const pseudo = op.type === 'matches-css' ? null : '::' + op.type.slice(12);
                const { prop, test } = cssMatcher(op.arg);
                nodes.forEach((n) => test(getComputedStyle(n, pseudo).getPropertyValue(prop)) && add(n));
                break;
            }
            case 'upward': {
                const levels = /^\d+$/.test(op.arg) ? +op.arg : 0;
                for (const n of nodes) {
                    if (levels) { let p = n; for (let i = 0; i < levels && p; i++) p = p.parentElement; add(p); }
                    else add(n.parentElement?.closest(op.arg));
                }
                break;
            }
            case 'xpath': {
                for (const n of first ? [document] : nodes) {
                    const r = document.evaluate(op.arg, n, null, XPathResult.ORDERED_NODE_SNAPSHOT_TYPE, null);
                    for (let i = 0; i < r.snapshotLength; i++) add(r.snapshotItem(i));
                }
                break;
            }
        }
        return [...out];
    };

    const apply = (el, action) => {
        if (!action) return el.style.setProperty('display', 'none', 'important');
        switch (action.type) {
            case 'remove': return el.remove();
            case 'remove-attr': return el.removeAttribute(action.arg);
            case 'remove-class': return el.classList.remove(action.arg);
            case 'style':
                for (const decl of action.arg.split(';')) {
                    const i = decl.indexOf(':');
                    if (i > 0) el.style.setProperty(decl.slice(0, i).trim(), decl.slice(i + 1).replace(/!important/i, '').trim(), 'important');
                }
        }
    };

    const run = () => {
        for (const rule of rules) {
            try {
                let nodes = [];
                rule.selector.forEach((op, i) => { nodes = step(nodes, op, i === 0); });
                nodes.forEach((el) => apply(el, rule.action));
            } catch (e) { }
        }
    };

    // Re-run as the page changes, at most every 100ms
    let queued = false;
    const schedule = () => { if (!queued) { queued = true; setTimeout(() => { queued = false; run(); }, 100); } };
    new MutationObserver(schedule).observe(document, { childList: true, subtree: true });
    if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', run); else run();
})();
