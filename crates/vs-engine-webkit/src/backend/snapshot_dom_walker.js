(function() {
    // Snapshot caching: a MutationObserver marks the page dirty on any
    // DOM change. When the page is clean and we have a cached result,
    // return it without re-walking the tree (the expensive part). The
    // observer and cache die with the document on navigation, so a
    // fresh page always walks once.
    if (!window.__vsDirtyObserver) {
        window.__vsDirty = true;
        try {
            window.__vsDirtyObserver = new MutationObserver(function() {
                window.__vsDirty = true;
            });
            window.__vsDirtyObserver.observe(document.documentElement, {
                subtree: true, childList: true, attributes: true, characterData: true,
            });
            // MutationObserver misses input `.value` changes (a property,
            // not an attribute). Fills and real typing dispatch input and
            // change, so mark dirty on those too, in the capture phase.
            var __vsMark = function() { window.__vsDirty = true; };
            document.addEventListener('input', __vsMark, true);
            document.addEventListener('change', __vsMark, true);

        } catch (e) { window.__vsDirty = true; }
    }
    if (window.__vsDirty === false && typeof window.__vsLastResult === 'string') {
        return window.__vsLastResult;
    }
    const ROLES = {

        'A': 'lnk', 'BUTTON': 'btn',
        'TEXTAREA': 'ta', 'SELECT': 'sel',
        'H1': 'hd', 'H2': 'hd', 'H3': 'hd', 'H4': 'hd', 'H5': 'hd', 'H6': 'hd',
        'NAV': 'nav', 'MAIN': 'mn', 'HEADER': 'hdr', 'FOOTER': 'sec',
        'IMG': 'img', 'UL': 'lst', 'OL': 'lst', 'LI': 'li',
        'TABLE': 'tbl', 'TR': 'row', 'TD': 'cell', 'TH': 'cell',
        'FORM': 'frm', 'P': 'p', 'ARTICLE': 'art', 'SECTION': 'sec',
        // Embedded documents. The walker cannot cross the frame
        // boundary, so the node carries the only thing an agent can
        // act on: the resolved src. Without it an embedded viewer
        // (pdf.js and friends) is a hole in the tree.
        'IFRAME': 'ifr', 'FRAME': 'ifr', 'EMBED': 'ifr', 'OBJECT': 'ifr',
    };
    // Roles whose label is *only* the leaf text the user reads, not the
    // entire subtree text. Containers (nav, main, hdr, sec, art, tbl,
    // row, frm, lst) get an empty label — their identity is the
    // structure they wrap, not the bag of text inside them.
    const LEAF_LABEL_ROLES = new Set([
        'lnk', 'btn', 'hd', 'p', 'li', 'cell', 'lbl', 'tf', 'ta', 'sel',
        'chk', 'rad', 'img', 'itm',
    ]);
    // Interactive roles get a hid=1 attr when the element cannot be
    // seen or hit: display:none / visibility:hidden (checkVisibility)
    // or a zero-size box. Sites keep hidden duplicates of buttons in
    // the DOM (x.com login); without the marker an agent cannot tell
    // the dead ref from the live one.
    const INTERACTIVE_ROLES = new Set([
        'btn', 'lnk', 'tf', 'ta', 'sel', 'chk', 'rad', 'itm',
    ]);
    function hiddenFor(el, role) {
        if (!INTERACTIVE_ROLES.has(role)) return false;
        try {
            if (el.checkVisibility && !el.checkVisibility()) return true;
            const b = el.getBoundingClientRect();
            return b.width < 0.5 || b.height < 0.5;
        } catch (e) { return false; }
    }
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
    // Bot-challenge detection.
    //
    // A challenge (Turnstile, hCaptcha, reCAPTCHA) is a hole in the
    // tree: its container is a bare <div> the role table ignores, and
    // when the widget fails to render there is no iframe either, so
    // the only trace left is a hidden response input that reads as an
    // anonymous `tf ... hid=1`. An agent sees a form, submits it, gets
    // rejected by the server, and concludes the form is broken — the
    // page never said it was gated.
    //
    // The response input is the reliable marker: every provider
    // creates it, under both implicit (`.cf-turnstile` div) and
    // explicit (`turnstile.render(...)`) rendering.
    const CHALLENGE_INPUTS = {
        'cf-turnstile-response': 'turnstile',
        'h-captcha-response': 'hcaptcha',
        'g-recaptcha-response': 'recaptcha',
    };
    const CHALLENGE_CLASSES = [
        ['cf-turnstile', 'turnstile'],
        ['h-captcha', 'hcaptcha'],
        ['g-recaptcha', 'recaptcha'],
    ];
    /// `{provider, state}` if `el` is a challenge widget or its
    /// response field, else null. `state` is what an agent needs to
    /// decide what to do: `solved` (token present, submit away),
    /// `pending` (widget is up, a human can complete it), or
    /// `unrendered` (the script loaded but produced no widget at all —
    /// nothing for anyone to interact with).
    function challengeFor(el) {
        var provider = null;
        var tokenEl = null;
        var name = el.getAttribute && (el.getAttribute('name') || '');
        if (name && CHALLENGE_INPUTS[name]) {
            provider = CHALLENGE_INPUTS[name];
            tokenEl = el;
        } else {
            var cls = '';
            try { cls = String(el.className && el.className.baseVal !== undefined ? el.className.baseVal : (el.className || '')); } catch (e) { cls = ''; }
            for (var i = 0; i < CHALLENGE_CLASSES.length; i += 1) {
                if (cls.indexOf(CHALLENGE_CLASSES[i][0]) >= 0) {
                    provider = CHALLENGE_CLASSES[i][1];
                    break;
                }
            }
            if (!provider) return null;
            try {
                tokenEl = el.querySelector('[name="cf-turnstile-response"],[name="h-captcha-response"],[name="g-recaptcha-response"]');
            } catch (e) { tokenEl = null; }
        }
        var token = tokenEl && tokenEl.value ? tokenEl.value : '';
        // Look for a rendered widget: the provider draws into an
        // iframe. Search the container, or the response field's
        // enclosing widget wrapper.
        var scope = tokenEl && tokenEl.parentNode && tokenEl.parentNode.parentNode
            ? tokenEl.parentNode.parentNode
            : el;
        var rendered = false;
        try {
            rendered = !!(scope.querySelector && scope.querySelector('iframe'));
        } catch (e) { rendered = false; }
        var state = token ? 'solved' : (rendered ? 'pending' : 'unrendered');
        return { provider: provider, state: state };
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
        if (role === 'ifr') {
            // `.src` / `.data` are already resolved against the base
            // URL; the attribute is often relative and useless to an
            // agent that wants to fetch it.
            var frameSrc = String(el.src || el.data || el.getAttribute('src') || '');
            if (!frameSrc && el.tagName === 'IFRAME' && el.getAttribute('srcdoc') !== null) {
                return 'srcdoc:';
            }
            // A data: URL is the whole document inline — megabytes of
            // it, sometimes. Keep the mime prefix, drop the payload.
            if (frameSrc.slice(0, 5) === 'data:') {
                return 'data:' + frameSrc.slice(5).split(';')[0].split(',')[0];
            }
            return frameSrc.slice(0, 500);
        }
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
        const chal = challengeFor(el);
        const children = [];
        for (const c of el.children) {
            const node = visit(c);
            if (node) children.push(node);
        }
        // Pierce open shadow roots. Cookie consent layers (OneTrust,
        // Cookiebot, Sourcepoint) and most web-component-based UIs put
        // their actual buttons inside a shadow root, which the walker
        // above would otherwise miss entirely.
        if (el.shadowRoot) {
            for (const c of el.shadowRoot.children) {
                const node = visit(c);
                if (node) children.push(node);
            }
        }
        // A challenge node is always kept, even when it is a roleless
        // childless div — dropping it is exactly the bug.
        if (!chal && !role && children.length === 0) return null;
        if (!chal && !role && children.length === 1) return children[0];
        const node = {
            // Always allocate a real (non-zero) ref, even for roleless
            // structural wrappers kept only to preserve tree shape.
            // Ref 0 is the delta grammar's ROOT sentinel, so emitting it
            // for a real node makes +/@ parent references ambiguous.
            r: refFor(el),
            role: role || 'el',
            label: labelFor(el, role || 'el'),
            children,
        };
        if (chal) {
            node.chal = chal.provider + ':' + chal.state;
            // Implicit rendering matches twice — once on the
            // `.cf-turnstile` container, once on the response input
            // inside it. Keep the outermost and clear the rest, so a
            // page reports one challenge rather than two.
            (function clearInner(kids) {
                for (var i = 0; i < kids.length; i += 1) {
                    if (kids[i].chal) delete kids[i].chal;
                    if (kids[i].children) clearInner(kids[i].children);
                }
            })(children);
            // Override the label: the response field would otherwise
            // read as an anonymous masked input, which is what made
            // this invisible in the first place.
            node.label = chal.provider + ' challenge (' + chal.state + ')';
            // Never mark a challenge hidden. It is legitimately
            // zero-size when unrendered, and `hid=1` reads as "dead
            // element, ignore it" — the opposite of what we mean.
            return node;
        }
        if (role && hiddenFor(el, role)) node.hid = 1;
        return node;
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
    // Shadow-piercing ref lookup used by act / wait / inspect helpers.
    // Reinstalled on every walk so the closure picks up new shadow
    // roots that appeared between snapshots.
    window.__vsFindRef = function(r) {
        const sel = '[data-vs-ref="' + String(r) + '"]';
        function walk(root) {
            const direct = root.querySelector(sel);
            if (direct) return direct;
            for (const el of root.querySelectorAll('*')) {
                if (el.shadowRoot) {
                    const found = walk(el.shadowRoot);
                    if (found) return found;
                }
            }
            return null;
        }
        return walk(document);
    };
    var __vsResult = JSON.stringify(root);
    window.__vsLastResult = __vsResult;
    window.__vsDirty = false;
    return __vsResult;
})()

