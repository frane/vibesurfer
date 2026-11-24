;(function () {
  // vibesurfer: evaluate code for the test (worker.evaluate) on request
  // of the page that started this worker.
  try {
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
