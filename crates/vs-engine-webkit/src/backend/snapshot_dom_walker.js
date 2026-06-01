(function() {
    const ROLES = {
        'A': 'lnk', 'BUTTON': 'btn',
        'TEXTAREA': 'ta', 'SELECT': 'sel',
        'H1': 'hd', 'H2': 'hd', 'H3': 'hd', 'H4': 'hd', 'H5': 'hd', 'H6': 'hd',
        'NAV': 'nav', 'MAIN': 'mn', 'HEADER': 'hdr', 'FOOTER': 'sec',
        'IMG': 'img', 'UL': 'lst', 'OL': 'lst', 'LI': 'li',
        'TABLE': 'tbl', 'TR': 'row', 'TD': 'cell', 'TH': 'cell',
        'FORM': 'frm', 'P': 'p', 'ARTICLE': 'art', 'SECTION': 'sec',
    };
    // Roles whose label is *only* the leaf text the user reads, not the
    // entire subtree text. Containers (nav, main, hdr, sec, art, tbl,
    // row, frm, lst) get an empty label — their identity is the
    // structure they wrap, not the bag of text inside them.
    const LEAF_LABEL_ROLES = new Set([
        'lnk', 'btn', 'hd', 'p', 'li', 'cell', 'lbl', 'tf', 'ta', 'sel',
        'chk', 'rad', 'img', 'itm',
    ]);
    // ARIA role → vs short-form mapping. Modern React UIs (Radix,
    // Headless UI, Reach UI, custom) render most actionable elements
    // as `<div role="...">` rather than semantic HTML, so the HTML-tag
    // table above misses them entirely. v0.1.8 consults `role="..."`
    // before falling back to the tag map.
    const ARIA_ROLES = {
        'button': 'btn', 'link': 'lnk',
        'checkbox': 'chk', 'switch': 'chk', 'radio': 'rad',
        'textbox': 'tf', 'searchbox': 'tf', 'combobox': 'sel',
        'listbox': 'lst', 'menu': 'lst', 'tree': 'lst',
        'option': 'itm', 'menuitem': 'itm', 'menuitemcheckbox': 'chk',
        'menuitemradio': 'rad', 'treeitem': 'itm', 'listitem': 'itm',
        'tab': 'itm', 'tabpanel': 'sec',
        'dialog': 'dlg', 'alertdialog': 'dlg',
        'navigation': 'nav', 'main': 'mn', 'banner': 'hdr', 'contentinfo': 'sec',
        'heading': 'hd', 'img': 'img', 'figure': 'img', 'region': 'sec',
        'article': 'art', 'form': 'frm', 'search': 'frm',
        'table': 'tbl', 'grid': 'tbl', 'row': 'row', 'gridcell': 'cell',
        'cell': 'cell', 'columnheader': 'hdr', 'rowheader': 'hdr',
    };
    function inputRole(el) {
        const t = (el.getAttribute('type') || 'text').toLowerCase();
        if (t === 'submit' || t === 'button') return 'btn';
        if (t === 'checkbox') return 'chk';
        if (t === 'radio') return 'rad';
        return 'tf';
    }
    function roleFor(el) {
        if (el.tagName === 'INPUT') return inputRole(el);
        const aria = el.getAttribute('role');
        if (aria) {
            const m = ARIA_ROLES[aria];
            if (m) return m;
        }
        // Heuristic: any element with an explicit click handler or a
        // tabindex that makes it focusable should expose `btn` so the
        // agent can act on it. Catches the Radix/Headless pattern of
        // `<div role="combobox" tabindex="0">` triggers.
        if (el.tagName !== 'DIV' && el.tagName !== 'SPAN') {
            return ROLES[el.tagName] || null;
        }
        const ti = el.getAttribute('tabindex');
        if (ti !== null && ti !== '-1') return 'btn';
        return ROLES[el.tagName] || null;
    }
    /// Direct (non-element-child) text under `el`, joined and trimmed.
    /// Used for container-ish elements whose subtree we don't want to
    /// scrape entirely.
    function directText(el) {
        let out = '';
        for (const c of el.childNodes) {
            if (c.nodeType === Node.TEXT_NODE) out += c.nodeValue;
        }
        return out.replace(/\s+/g, ' ').trim();
    }
    function labelFor(el, role) {
        const aria = el.getAttribute('aria-label');
        if (aria) return aria.trim();
        if (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA') {
            // Mask sensitive inputs so the agent never sees what the
            // user typed via `vs prompt-input --secret` (or via the
            // page directly). The HTML spec already marks `password`
            // / `hidden` inputs as not-for-display; the `autocomplete`
            // hints catch credit-card fields and similar.
            const type = (el.getAttribute('type') || 'text').toLowerCase();
            const ac = (el.getAttribute('autocomplete') || '').toLowerCase();
            const sensitive = (
                type === 'password' || type === 'hidden' ||
                ac === 'current-password' || ac === 'new-password' ||
                ac === 'one-time-code' ||
                ac === 'cc-number' || ac === 'cc-csc'
            );
            if (sensitive) {
                if (el.value) return '***';
                return (el.placeholder || '').trim();
            }
            return (el.value || el.placeholder || '').trim();
        }
        if (el.tagName === 'IMG') return (el.alt || '').trim();
        if (LEAF_LABEL_ROLES.has(role)) {
            // Use innerText (capped) for leaf-ish nodes the user reads.
            return (el.innerText || el.textContent || '')
                .replace(/\s+/g, ' ').trim().slice(0, 200);
        }
        // Container roles: only the direct text (typically empty).
        return directText(el).slice(0, 200);
    }
    let counter = (window.__vsRefCounter || 0);
    function refFor(el) {
        let r = el.getAttribute('data-vs-ref');
        if (r) return parseInt(r, 10);
        counter += 1;
        el.setAttribute('data-vs-ref', String(counter));
        return counter;
    }
    function visit(el) {
        const role = roleFor(el);
        const children = [];
        for (const c of el.children) {
            const node = visit(c);
            if (node) children.push(node);
        }
        if (!role && children.length === 0) return null;
        if (!role && children.length === 1) return children[0];
        return {
            r: role ? refFor(el) : 0,
            role: role || 'el',
            label: labelFor(el, role || 'el'),
            children,
        };
    }
    const docRef = refFor(document.documentElement);
    const root = {
        r: docRef,
        role: 'doc',
        label: document.title || '',
        children: [],
    };
    const body = document.body;
    if (body) {
        for (const c of body.children) {
            const n = visit(c);
            if (n) root.children.push(n);
        }
    }
    window.__vsRefCounter = counter;
    return JSON.stringify(root);
})()
