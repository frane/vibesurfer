// Run before a native drop (see drop_shim.rs). The next trusted drop's
// DataTransfer also lists and returns string items the OS pasteboard
// has no slot for, and the dropped files when the engine delivered
// none. They stay on that DataTransfer, so a handler that reads it after
// an await sees them too. window.__vsDropRestore() forgets a drop that
// never happened.
(() => {
  const pending = { extra: __EXTRA__, files: __FILES__ };
  if (!window.__vsDrop) {
    const P = DataTransfer.prototype;
    const getData = P.getData;
    const types = Object.getOwnPropertyDescriptor(P, 'types');
    const files = Object.getOwnPropertyDescriptor(P, 'files');
    const state = { pending: null, drops: new WeakMap() };
    const toFileList = (specs) => {
      const dt = new DataTransfer();
      for (const f of specs) {
        const bytes = Uint8Array.from(atob(f.b64), (c) => c.charCodeAt(0));
        dt.items.add(new File([bytes], f.name, { type: f.type }));
      }
      return dt.files;
    };
    window.addEventListener('drop', (e) => {
      if (!e.isTrusted || !state.pending || !e.dataTransfer) return;
      const p = state.pending;
      state.pending = null;
      const native = files.get.call(e.dataTransfer);
      state.drops.set(e.dataTransfer, {
        extra: p.extra,
        files: native.length === 0 && p.files.length > 0 ? toFileList(p.files) : null,
      });
    }, true);
    const has = Object.prototype.hasOwnProperty;
    P.getData = function (t) {
      const d = state.drops.get(this);
      const k = String(t).toLowerCase();
      return d && has.call(d.extra, k) ? d.extra[k] : getData.call(this, t);
    };
    Object.defineProperty(P, 'types', { configurable: true, enumerable: true, get() {
      const v = [...types.get.call(this)];
      const d = state.drops.get(this);
      if (d) {
        if (d.files && !v.includes('Files')) v.unshift('Files');
        for (const k of Object.keys(d.extra)) if (!v.includes(k)) v.push(k);
      }
      return Object.freeze(v);
    } });
    Object.defineProperty(P, 'files', { configurable: true, enumerable: true, get() {
      const d = state.drops.get(this);
      return d && d.files ? d.files : files.get.call(this);
    } });
    window.__vsDrop = state;
    window.__vsDropRestore = () => { state.pending = null; };
  }
  window.__vsDrop.pending = pending;
  return 'ok';
})()
