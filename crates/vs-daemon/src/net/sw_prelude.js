;(function () {
  // vibesurfer: report the responses this service worker builds or
  // serves from its cache, which never reach the network.
  try {
    var seen = new WeakSet();
    var mark = function (p) {
      return p.then(function (r) {
        if (r) seen.add(r);
        return r;
      });
    };
    if (self.Cache) {
      var cm = Cache.prototype.match;
      Cache.prototype.match = function () { return mark(cm.apply(this, arguments)); };
    }
    if (self.CacheStorage) {
      var sm = CacheStorage.prototype.match;
      CacheStorage.prototype.match = function () { return mark(sm.apply(this, arguments)); };
    }
    var respond = FetchEvent.prototype.respondWith;
    FetchEvent.prototype.respondWith = function (answer) {
      var q = this.request;
      return respond.call(this, Promise.resolve(answer).then(function (r) {
        try {
          if (r && (!r.url || seen.has(r))) {
            var qh = [], rh = [];
            q.headers.forEach(function (v, k) { qh.push([k, v]); });
            r.headers.forEach(function (v, k) { rh.push([k, v]); });
            r.clone().arrayBuffer().then(function (b) {
              var u = new Uint8Array(b.byteLength > 1048576 ? 0 : b), s = '';
              for (var i = 0; i < u.length; i++) s += String.fromCharCode(u[i]);
              return fetch(self.location.origin + '/__vs_sw_report__', {
                method: 'POST',
                headers: { 'x-vs-sw-report': '1' },
                body: JSON.stringify({ url: q.url, method: q.method, headers: qh, status: r.status, statusText: r.statusText, responseHeaders: rh, body: btoa(s), cached: seen.has(r) })
              });
            }).catch(function () {});
          }
        } catch (e) {}
        return r;
      }));
    };
  } catch (e) {}
})();
