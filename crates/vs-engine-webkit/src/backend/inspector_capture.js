// Inspector capture payload, injected at document-start on every page.
//
// Patches console + fetch + XHR + global error events to forward
// JSON-encoded events to the host via two message-handler channels:
//   webkit.messageHandlers.vsConsole.postMessage(json_string)
//   webkit.messageHandlers.vsNetwork.postMessage(json_string)
//
// Wire format is identical on macOS (WKScriptMessageHandler) and Linux
// (WebKitGTK UserContentManager script-message-received signal). The
// host parses the body string as JSON and pushes into a per-page
// RingBuffer<ConsoleEntry> / RingBuffer<NetworkEntry>.
//
// Dictionary on the wire:
//   console:  {kind: "console", level, message, stack?, ts_ms}
//   network:  {kind: "network", phase: "start"|"end",
//              seq, method, url, status?, size?, ts_ms,
//              req_headers?, res_headers?, req_body?, res_body?}
//
// Idempotent — installs a one-shot guard so a second injection (e.g.
// after a same-page navigation rebinding) is a no-op.

(function() {
  if (window.__vsInspectorInstalled) return;
  window.__vsInspectorInstalled = true;

  function send(channel, obj) {
    try {
      window.webkit.messageHandlers[channel].postMessage(JSON.stringify(obj));
    } catch (e) {
      // Host bridge missing (engine doesn't capture). Fail open.
    }
  }

  function nowMs() { return Date.now(); }
  function fmtArg(a) {
    if (a === null) return 'null';
    if (a === undefined) return 'undefined';
    if (typeof a === 'string') return a;
    try { return JSON.stringify(a); }
    catch (_) { return String(a); }
  }
  function joinArgs(args) {
    var parts = new Array(args.length);
    for (var i = 0; i < args.length; i++) parts[i] = fmtArg(args[i]);
    return parts.join(' ');
  }

  // ---- console ----
  var levels = ['log', 'info', 'warn', 'error', 'debug'];
  for (var i = 0; i < levels.length; i++) {
    (function(level) {
      var orig = console[level];
      console[level] = function() {
        var stack = (level === 'error' || level === 'warn')
          ? (new Error()).stack || null
          : null;
        send('vsConsole', {
          kind: 'console',
          level: level,
          message: joinArgs(arguments),
          stack: stack,
          ts_ms: nowMs(),
        });
        if (orig) try { orig.apply(console, arguments); } catch (_) {}
      };
    })(levels[i]);
  }

  // Uncaught errors and unhandled rejections — surface as console.error.
  window.addEventListener('error', function(ev) {
    var msg = ev.message || (ev.error && ev.error.message) || 'uncaught error';
    send('vsConsole', {
      kind: 'console',
      level: 'error',
      message: msg,
      stack: ev.error && ev.error.stack ? ev.error.stack : null,
      ts_ms: nowMs(),
    });
  });
  window.addEventListener('unhandledrejection', function(ev) {
    var reason = ev.reason;
    var msg = (reason && reason.message) ? reason.message : String(reason);
    send('vsConsole', {
      kind: 'console',
      level: 'error',
      message: 'unhandled promise rejection: ' + msg,
      stack: reason && reason.stack ? reason.stack : null,
      ts_ms: nowMs(),
    });
  });

  // ---- network ----
  var seqCounter = 0;
  function nextSeq() { seqCounter += 1; return seqCounter; }

  // Body / header capture caps. Bigger than the default daemon truncate
  // (4096 bytes) so the host can decide to clip; the host is the
  // authoritative truncator.
  var MAX_BODY = 32768;
  function clip(s) {
    if (typeof s !== 'string') return null;
    return s.length > MAX_BODY ? s.slice(0, MAX_BODY) : s;
  }
  function headersToList(h) {
    var out = [];
    if (!h) return out;
    if (typeof h.forEach === 'function') {
      h.forEach(function(value, name) { out.push([name, value]); });
      return out;
    }
    if (Array.isArray(h)) {
      for (var i = 0; i < h.length; i++) out.push([h[i][0], h[i][1]]);
      return out;
    }
    if (typeof h === 'object') {
      for (var k in h) if (Object.prototype.hasOwnProperty.call(h, k)) {
        out.push([k, String(h[k])]);
      }
    }
    return out;
  }

  // fetch
  var origFetch = window.fetch;
  if (origFetch) {
    window.fetch = function(input, init) {
      var seq = nextSeq();
      var startMs = nowMs();
      var method = (init && init.method) || (input && input.method) || 'GET';
      var url = (typeof input === 'string') ? input : (input && input.url) || '';
      var reqHeaders = headersToList(init && init.headers);
      var reqBody = init && init.body && typeof init.body === 'string'
        ? clip(init.body) : null;

      send('vsNetwork', {
        kind: 'network', phase: 'start',
        seq: seq, method: method, url: url, ts_ms: startMs,
        req_headers: reqHeaders, req_body: reqBody,
      });

      return origFetch.apply(this, arguments).then(function(res) {
        var endMs = nowMs();
        var resHeaders = headersToList(res.headers);
        // Clone so the page can still consume the body.
        var bodyPromise;
        try { bodyPromise = res.clone().text(); }
        catch (_) { bodyPromise = Promise.resolve(null); }
        bodyPromise.then(function(text) {
          send('vsNetwork', {
            kind: 'network', phase: 'end',
            seq: seq, method: method, url: url,
            status: res.status, ts_ms: endMs,
            size: text ? text.length : 0,
            res_headers: resHeaders,
            res_body: clip(text),
          });
        });
        return res;
      }).catch(function(err) {
        send('vsNetwork', {
          kind: 'network', phase: 'end',
          seq: seq, method: method, url: url,
          status: 0, ts_ms: nowMs(),
          err: String(err && err.message || err),
        });
        throw err;
      });
    };
  }

  // XHR
  var XHR = window.XMLHttpRequest;
  if (XHR && XHR.prototype) {
    var origOpen = XHR.prototype.open;
    var origSend = XHR.prototype.send;
    var origSetReq = XHR.prototype.setRequestHeader;

    XHR.prototype.open = function(method, url) {
      this.__vs = {
        seq: nextSeq(), method: method, url: url,
        startMs: 0, headers: [],
      };
      return origOpen.apply(this, arguments);
    };
    XHR.prototype.setRequestHeader = function(name, value) {
      if (this.__vs) this.__vs.headers.push([name, value]);
      return origSetReq.apply(this, arguments);
    };
    XHR.prototype.send = function(body) {
      var meta = this.__vs;
      if (!meta) return origSend.apply(this, arguments);
      meta.startMs = nowMs();
      send('vsNetwork', {
        kind: 'network', phase: 'start',
        seq: meta.seq, method: meta.method, url: meta.url,
        ts_ms: meta.startMs,
        req_headers: meta.headers,
        req_body: typeof body === 'string' ? clip(body) : null,
      });
      var xhr = this;
      this.addEventListener('loadend', function() {
        var endMs = nowMs();
        var resHeaders = [];
        try {
          var raw = xhr.getAllResponseHeaders() || '';
          var lines = raw.split('\r\n');
          for (var i = 0; i < lines.length; i++) {
            var idx = lines[i].indexOf(':');
            if (idx > 0) {
              resHeaders.push([
                lines[i].slice(0, idx).trim(),
                lines[i].slice(idx + 1).trim(),
              ]);
            }
          }
        } catch (_) {}
        var resBody = null;
        try {
          if (xhr.responseType === '' || xhr.responseType === 'text') {
            resBody = clip(xhr.responseText || '');
          }
        } catch (_) {}
        send('vsNetwork', {
          kind: 'network', phase: 'end',
          seq: meta.seq, method: meta.method, url: meta.url,
          status: xhr.status, ts_ms: endMs,
          size: resBody ? resBody.length : 0,
          res_headers: resHeaders,
          res_body: resBody,
        });
      });
      return origSend.apply(this, arguments);
    };
  }

  // This script is injected after the download shim, so its global is
  // created after that shim's hide pass has run. Re-hide, or
  // `Object.keys(window)` still advertises the inspector.
  if (typeof window.__vsHideGlobals === 'function') {
    window.__vsHideGlobals(['__vsInspectorInstalled']);
  }
})();
