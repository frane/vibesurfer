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
    function inputRole(el) {
        const t = (el.getAttribute('type') || 'text').toLowerCase();
        if (t === 'submit' || t === 'button') return 'btn';
        if (t === 'checkbox') return 'chk';
        if (t === 'radio') return 'rad';
        return 'tf';
    }
    function roleFor(el) {
        if (el.tagName === 'INPUT') return inputRole(el);
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
