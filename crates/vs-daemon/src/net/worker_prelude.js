;(function () {
  // vibesurfer: evaluate code for the test (worker.evaluate) on request
  // of the page that started this worker.
  try {
    // Console messages go to the page too, for page.on('console').
    ['log', 'debug', 'info', 'warn', 'error'].forEach(function (k) {
      var orig = console[k];
      console[k] = function () {
        try {
          var text = Array.prototype.map.call(arguments, function (a) {
            if (typeof a === 'string') return a;
            try { return JSON.stringify(a); } catch (e) { return String(a); }
          }).join(' ');
          self.postMessage({ __tsConsole: { type: k === 'warn' ? 'warning' : k, text: text } });
        } catch (e) {}
        return orig.apply(console, arguments);
      };
    });
    self.addEventListener('message', function (e) {
      var d = e.data;
      if (!d || typeof d !== 'object' || d.__tsEval === undefined) return;
      e.stopImmediatePropagation();
      var id = d.__tsEval;
      Promise.resolve()
        .then(function () { return (0, eval)('(' + d.fn + ')')(d.arg); })
        .then(function (v) { self.postMessage({ __tsEvalResult: id, v: v }); },
              function (err) { self.postMessage({ __tsEvalResult: id, e: String((err && err.message) || err) }); });
    });
  } catch (e) {}
})();
