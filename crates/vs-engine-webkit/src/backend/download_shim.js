// Download capture shim. Injected at document-start into *every*
// frame on every backend.
//
// A headless WKWebView / WebKitGTK / WebView2 has no download delegate,
// so anything the page tries to save to disk is dropped on the floor
// with no event an agent can observe. This shim intercepts the intent
// one layer *above* the engine's download machinery — at the click /
// `window.open` / `createObjectURL` level — reads the bytes with the
// page's own credentials, and parks them in a bounded buffer that
// `vs_download` drains.
//
// Two things make this work where a naive interceptor would not:
//
//   1. `URL.createObjectURL` is patched to keep a strong reference to
//      the Blob. Viewers (pdf.js among them) revoke the object URL a
//      tick after clicking their synthetic anchor, so by the time we
//      could fetch it the URL is dead. Holding the Blob sidesteps the
//      race entirely.
//   2. Sub-frames read their own bytes and `postMessage` them to the
//      top frame. A `blob:` URL minted inside a cross-origin iframe is
//      not fetchable from the top frame, and the top frame is the only
//      place engine-side `eval` can reach.
(function () {
  if (window.__vsDlInstalled) return;
  window.__vsDlInstalled = true;

  var TAG = 'vs-download-v1';
  // Bounded so a page that downloads in a loop can't grow the buffer
  // without limit. Entries are bytes-in-memory, hence the tight cap.
  var MAX_ENTRIES = 8;
  var MAX_BLOBS = 16;
  var MAX_BYTES = 64 * 1024 * 1024;

  var isTop = false;
  try { isTop = window.top === window; } catch (e) { isTop = false; }

  // Blob registry: object URL -> Blob, so a revoked URL is still
  // readable. Insertion-ordered Map, oldest evicted first.
  var blobs = new Map();

  function remember(url, blob) {
    try {
      blobs.set(url, blob);
      while (blobs.size > MAX_BLOBS) blobs.delete(blobs.keys().next().value);
    } catch (e) { /* Map unavailable: fall back to fetching the URL */ }
  }

  var origCreate = typeof URL !== 'undefined' && URL.createObjectURL;
  if (origCreate) {
    URL.createObjectURL = function (obj) {
      var url = origCreate.call(URL, obj);
      try {
        if (typeof Blob !== 'undefined' && obj instanceof Blob) remember(url, obj);
      } catch (e) { /* not a Blob (MediaSource, MediaStream): ignore */ }
      return url;
    };
  }

  // --- top-frame buffer -------------------------------------------------

  if (isTop && !window.__vsDl) {
    window.__vsDl = {
      list: [],
      seq: 0,
      push: function (entry) {
        this.seq += 1;
        entry.id = this.seq;
        this.list.push(entry);
        while (this.list.length > MAX_ENTRIES) this.list.shift();
        return entry.id;
      },
      get: function (id) {
        for (var i = 0; i < this.list.length; i += 1) {
          if (this.list[i].id === id) return this.list[i];
        }
        return null;
      },
      // Metadata only — never the bytes. Rust polls this until `done`,
      // then pulls the payload through `read` in chunks.
      meta: function (id) {
        var e = id ? this.get(id) : this.list[this.list.length - 1];
        if (!e) return 'null';
        return JSON.stringify({
          id: e.id,
          name: e.name || 'download',
          mime: e.mime || '',
          url: e.url || '',
          err: e.err || null,
          size: e.bytes ? e.bytes.length : 0,
          done: !!e.done,
          ts: e.ts || 0,
        });
      },
      index: function () {
        return JSON.stringify(this.list.map(function (e) {
          return {
            id: e.id,
            name: e.name || 'download',
            mime: e.mime || '',
            url: e.url || '',
            err: e.err || null,
            size: e.bytes ? e.bytes.length : 0,
            done: !!e.done,
            ts: e.ts || 0,
          };
        }));
      },
      // Base64 of `bytes[off .. off+len]`. The caller decodes each
      // chunk separately and concatenates the raw bytes, so `off` need
      // not land on a 3-byte boundary.
      read: function (id, off, len) {
        var e = this.get(id);
        if (!e || !e.bytes) return '';
        var slice = e.bytes.subarray(off, off + len);
        var s = '';
        var STEP = 0x8000; // apply() blows the arg limit past ~64k
        for (var i = 0; i < slice.length; i += STEP) {
          s += String.fromCharCode.apply(null, slice.subarray(i, i + STEP));
        }
        return btoa(s);
      },
      drop: function (id) {
        this.list = this.list.filter(function (e) { return e.id !== id; });
        return 'ok';
      },
      clear: function () { this.list = []; return 'ok'; },
    };
  }

  function record(entry) {
    entry.ts = Date.now();
    entry.done = true;
    if (isTop) {
      window.__vsDl.push(entry);
      return;
    }
    // Sub-frame: relay to the top frame. Bytes ride the structured
    // clone as a Uint8Array — no base64 inflation in the page.
    try {
      window.top.postMessage({
        __vs: TAG,
        name: entry.name,
        mime: entry.mime,
        url: entry.url,
        err: entry.err || null,
        bytes: entry.bytes || null,
      }, '*');
    } catch (e) { /* top frame gone */ }
  }

  // Trust boundary: any frame in this browsing context can post a
  // tagged message, so a hostile embedded frame can put an entry in
  // the buffer. That buys it nothing an ordinary download link on the
  // same page would not — the daemon sanitizes the filename to a
  // single component, and nothing is written until the agent asks for
  // it by calling vs_download.
  if (isTop) {
    window.addEventListener('message', function (ev) {
      var d = ev.data;
      if (!d || d.__vs !== TAG) return;
      window.__vsDl.push({
        name: d.name,
        mime: d.mime,
        url: d.url,
        err: d.err,
        bytes: d.bytes ? new Uint8Array(d.bytes) : null,
        ts: Date.now(),
        done: true,
      });
    }, true);
  }

  // --- fetching ---------------------------------------------------------

  function nameFromDisposition(cd) {
    if (!cd) return null;
    // RFC 5987 `filename*=UTF-8''...` wins over the plain form.
    var star = /filename\*\s*=\s*[^']*'[^']*'([^;]+)/i.exec(cd);
    if (star) {
      try { return decodeURIComponent(star[1].trim()); } catch (e) { /* fall through */ }
    }
    var plain = /filename\s*=\s*"([^"]*)"|filename\s*=\s*([^;]+)/i.exec(cd);
    if (plain) return (plain[1] || plain[2] || '').trim() || null;
    return null;
  }

  function nameFromUrl(url) {
    try {
      if (url.slice(0, 5) === 'blob:' || url.slice(0, 5) === 'data:') return null;
      var u = new URL(url, location.href);
      var last = u.pathname.split('/').filter(Boolean).pop();
      return last ? decodeURIComponent(last) : null;
    } catch (e) { return null; }
  }

  // Read `url` (Blob registry first, network second) and record the
  // result. Never throws — a failure is recorded as an `err` entry so
  // the agent sees *why* nothing arrived instead of a silent drop.
  function grab(url, hint, id) {
    var finish = function (entry) {
      entry.url = url;
      entry.name = entry.name || hint || nameFromUrl(url) || 'download';
      if (isTop && id) {
        var slot = window.__vsDl.get(id);
        if (slot) {
          slot.name = entry.name;
          slot.mime = entry.mime || '';
          slot.bytes = entry.bytes || null;
          slot.err = entry.err || null;
          slot.ts = Date.now();
          slot.done = true;
          return;
        }
      }
      record(entry);
    };

    var known = blobs.get(url);
    var source = known
      ? Promise.resolve({ blob: known, cd: null, ct: known.type })
      : fetch(url, { credentials: 'include' }).then(function (res) {
          if (!res.ok) throw new Error('HTTP ' + res.status + ' ' + res.statusText);
          var cd = null;
          var ct = null;
          try {
            cd = res.headers.get('content-disposition');
            ct = res.headers.get('content-type');
          } catch (e) { /* opaque response */ }
          return res.blob().then(function (b) { return { blob: b, cd: cd, ct: ct }; });
        });

    source.then(function (r) {
      if (r.blob.size > MAX_BYTES) {
        throw new Error('download exceeds ' + MAX_BYTES + ' bytes (' + r.blob.size + ')');
      }
      return r.blob.arrayBuffer().then(function (buf) {
        finish({
          name: nameFromDisposition(r.cd) || hint || nameFromUrl(url),
          mime: r.blob.type || r.ct || '',
          bytes: new Uint8Array(buf),
        });
      });
    }).catch(function (e) {
      finish({ err: String((e && e.message) || e) });
    });
  }

  // Engine-side entry point for `vs_download <page> <url>`: start a
  // credentialed read and return the buffer id to poll.
  if (isTop) {
    window.__vsDl.fetch = function (url, hint) {
      var id = this.push({ name: hint || nameFromUrl(url) || 'download', url: url, done: false });
      grab(url, hint, id);
      return String(id);
    };
  }

  // --- intent interception ----------------------------------------------

  var lastUrl = '';
  var lastAt = 0;

  function intercept(url, hint) {
    if (!url) return;
    var now = Date.now();
    // The DOM listener and the patched `.click()` both fire for the
    // same anchor; collapse the duplicate.
    if (url === lastUrl && now - lastAt < 500) return;
    lastUrl = url;
    lastAt = now;
    grab(url, hint, 0);
  }

  function anchorIntent(a) {
    if (!a || !a.getAttribute) return;
    var href = a.getAttribute('href');
    if (!href) return;
    var abs = a.href || href;
    var isBlob = abs.slice(0, 5) === 'blob:' || abs.slice(0, 5) === 'data:';
    // `download` is the explicit signal; a blob/data href is the
    // implicit one (nothing else navigates to those in practice).
    if (!a.hasAttribute('download') && !isBlob) return;
    intercept(abs, a.getAttribute('download') || null);
  }

  document.addEventListener('click', function (ev) {
    var t = ev.target;
    while (t && t.nodeType === 1 && t.tagName !== 'A') t = t.parentNode;
    if (t && t.nodeType === 1) anchorIntent(t);
  }, true);

  // A synthetic `a.click()` on an anchor never attached to the
  // document does not bubble anywhere the listener above can see it —
  // and that is exactly the shape every "save this blob" helper uses.
  try {
    var protoClick = HTMLAnchorElement.prototype.click;
    HTMLAnchorElement.prototype.click = function () {
      try { anchorIntent(this); } catch (e) { /* never block the click */ }
      return protoClick.apply(this, arguments);
    };
  } catch (e) { /* prototype frozen */ }

  try {
    var origOpen = window.open;
    window.open = function (url) {
      try {
        if (url && (String(url).slice(0, 5) === 'blob:' || String(url).slice(0, 5) === 'data:')) {
          intercept(String(url), null);
        }
      } catch (e) { /* never block the open */ }
      return origOpen.apply(window, arguments);
    };
  } catch (e) { /* window.open non-writable */ }
})()
