(function() {
    // Dump every IndexedDB database on this origin.
    //
    // IndexedDB is callback-driven and none of the three engines can await
    // a promise inside one `evaluateJavaScript`, so this script is both
    // the kickoff and the poll: the first call starts the dump and parks
    // it on `window.__vsIdbSave`, and every later call reports where it
    // got to. The host calls it until `state` leaves `pending`.
    //
    // Values that cannot survive JSON (Blob, File, ArrayBuffer, typed
    // arrays, Map, Set, and anything else structured clone carries but
    // JSON does not) are counted in `skipped` rather than written out
    // wrong. A blob that silently restored `{}` where a File used to be
    // would look like a working login until the page tried to use it.
    var S = window.__vsIdbSave;
    if (!S) {
        S = window.__vsIdbSave = { state: 'pending', error: '', dbs: [], skipped: 0 };
        try {
            if (!window.indexedDB || !indexedDB.databases) {
                // Safari < 14 and any engine without enumeration: we
                // cannot discover database names, so report done and
                // empty rather than hanging the caller.
                S.state = 'done';
            } else {
                start(S);
            }
        } catch (e) {
            S.state = 'error';
            S.error = String(e);
        }
    }
    return JSON.stringify({ state: S.state, error: S.error, dbs: S.dbs, skipped: S.skipped });

    function jsonSafe(v) {
        if (v === undefined) return false;
        if (v === null) return true;
        if (typeof v === 'object') {
            if (v instanceof Date) return true;
            if (typeof Blob !== 'undefined' && v instanceof Blob) return false;
            if (v instanceof ArrayBuffer || ArrayBuffer.isView(v)) return false;
            if (v instanceof Map || v instanceof Set) return false;
        }
        try {
            return JSON.stringify(v) !== undefined;
        } catch (e) {
            return false;
        }
    }

    function start(S) {
        indexedDB.databases().then(function(list) {
            var names = [];
            for (var i = 0; i < list.length; i++) {
                if (list[i] && list[i].name) names.push(list[i].name);
            }
            var left = names.length;
            if (!left) { S.state = 'done'; return; }
            names.forEach(function(name) { dumpDb(S, name, done); });
            function done() { if (--left === 0) S.state = 'done'; }
        }).catch(function(e) {
            S.state = 'error';
            S.error = String(e);
        });
    }

    function dumpDb(S, name, done) {
        var req;
        try {
            req = indexedDB.open(name);
        } catch (e) {
            done();
            return;
        }
        req.onerror = done;
        req.onblocked = done;
        req.onsuccess = function() {
            var db = req.result;
            var storeNames = Array.prototype.slice.call(db.objectStoreNames);
            var out = { name: name, version: db.version, stores: [] };
            if (!storeNames.length) {
                db.close();
                S.dbs.push(out);
                done();
                return;
            }
            var tx;
            try {
                tx = db.transaction(storeNames, 'readonly');
            } catch (e) {
                db.close();
                done();
                return;
            }
            var pending = storeNames.length;
            storeNames.forEach(function(storeName) {
                var os = tx.objectStore(storeName);
                var store = {
                    name: storeName,
                    keyPath: os.keyPath === undefined ? null : os.keyPath,
                    autoIncrement: !!os.autoIncrement,
                    records: [],
                };
                var values = null, keys = null;
                var gv = os.getAll(), gk = os.getAllKeys();
                gv.onsuccess = function() { values = gv.result || []; join(); };
                gv.onerror = function() { values = []; join(); };
                gk.onsuccess = function() { keys = gk.result || []; join(); };
                gk.onerror = function() { keys = []; join(); };

                function join() {
                    if (values === null || keys === null) return;
                    for (var i = 0; i < values.length; i++) {
                        var key = i < keys.length ? keys[i] : undefined;
                        if (!jsonSafe(values[i]) || !jsonSafe(key)) { S.skipped++; continue; }
                        store.records.push({ key: key, value: values[i] });
                    }
                    out.stores.push(store);
                    if (--pending === 0) {
                        S.dbs.push(out);
                        db.close();
                        done();
                    }
                }
            });
        };
    }
})()
