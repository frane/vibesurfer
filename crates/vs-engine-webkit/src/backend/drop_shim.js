// Run before a native drop (see drop_shim.rs). During the trusted drop
// event, the DataTransfer also lists and returns string items the OS
// pasteboard has no slot for, and the dropped files when the engine
// delivered none. Undone by window.__vsDropRestore().
(() => {
  const extra = __EXTRA__;
  const fileSpecs = __FILES__;
  const P = DataTransfer.prototype;
  const getData = P.getData;
  const types = Object.getOwnPropertyDescriptor(P, 'types');
  const files = Object.getOwnPropertyDescriptor(P, 'files');
  let dropping = false;
  let fallback = null;
  const fallbackFiles = () => {
    if (!fallback) {
      const dt = new DataTransfer();
      for (const f of fileSpecs) {
        const bytes = Uint8Array.from(atob(f.b64), (c) => c.charCodeAt(0));
        dt.items.add(new File([bytes], f.name, { type: f.type }));
      }
      fallback = dt.files;
    }
    return fallback;
  };
  const missingFiles = (dt) => dropping && fileSpecs.length > 0 && files.get.call(dt).length === 0;
  const on = (e) => { if (e.isTrusted) dropping = true; };
  const off = () => { dropping = false; };
  window.addEventListener('drop', on, true);
  window.addEventListener('drop', off, false);
  P.getData = function (t) {
    const k = String(t).toLowerCase();
    return dropping && Object.prototype.hasOwnProperty.call(extra, k) ? extra[k] : getData.call(this, t);
  };
  Object.defineProperty(P, 'types', { configurable: true, enumerable: true, get() {
    const v = [...types.get.call(this)];
    if (missingFiles(this) && !v.includes('Files')) v.unshift('Files');
    for (const k of Object.keys(extra)) if (!v.includes(k)) v.push(k);
    return Object.freeze(v);
  } });
  Object.defineProperty(P, 'files', { configurable: true, enumerable: true, get() {
    return missingFiles(this) ? fallbackFiles() : files.get.call(this);
  } });
  window.__vsDropRestore = () => {
    P.getData = getData;
    Object.defineProperty(P, 'types', types);
    Object.defineProperty(P, 'files', files);
    window.removeEventListener('drop', on, true);
    window.removeEventListener('drop', off, false);
    delete window.__vsDropRestore;
  };
  return 'ok';
})()
