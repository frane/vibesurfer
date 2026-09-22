(function() {
    // Restore the IndexedDB databases captured by `idb_save.js`.
    //
    // Kickoff-and-poll for the same reason as the save side: the host
    // calls this until `state` leaves `pending`. The payload is spliced in
    // by the caller as `var payload = {...};` ahead of this body.
    //
    // Each database is opened at the version it was captured at, missing
    // stores are created in the upgrade transaction (the only place a
    // store can be created), and records are written with `put` so a
    // restore over an existing database overwrites rather than throws.
    var S = window.__vsIdbLoad;
    if (S && S.token === payload.token) {
        return JSON.stringify({
            state: S.state,
            error: S.error || S.waiting,
            trace: S.trace.join(' > '),
        });
    }
    S = window.__vsIdbLoad = {
        token: payload.token,
        state: 'pending',
        error: '',
        // Failures, collected so one database's success cannot erase
        // another's failure. `waiting` is not a failure: it says a
        // connection is still holding a database open.
        failures: [],
        waiting: '',
        // Breadcrumbs for the host's error message. A restore that
        // reports success while the database stays empty is the one
        // failure this path must never hide, so it says where it went.
        trace: [],
    };
    var dbs = payload.dbs || [];
    var left = dbs.length;
    if (!left || !window.indexedDB) {
        S.state = 'done';
        return JSON.stringify({ state: S.state, error: S.error });
    }
    dbs.forEach(function(spec) { restore(spec, done); });
    return JSON.stringify({ state: S.state, error: S.error });

    // A database that could not be written must not land as success.
    // Reporting `done` on a blocked or failed open is how a restore
    // claims a session it never wrote, which is the failure this whole
    // path exists to remove. `onblocked` deliberately does not report
    // at all: another connection is holding the database open and may
    // yet close, so the request stays live and the host's deadline
    // decides, with `S.error` naming what it was waiting on.
    function done(why) {
        if (why) { S.failures.push(why); }
        if (--left === 0) {
            S.error = S.failures.join('; ');
            S.state = S.failures.length ? 'error' : 'done';
        }
    }

    function restore(spec, done) {
        S.trace.push('open ' + spec.name + '@v' + (spec.version || 1));
        open(spec, spec.version || 1, done, true);
    }

    // `version` decides everything here: object stores can only be
    // created inside an upgrade transaction, and that only runs when
    // the requested version is higher than the one on disk. A database
    // that already exists at the captured version — an empty one the
    // page created on open, or a partial restore — would otherwise
    // report success with none of its stores. When stores are missing
    // after a successful open, `retry` reopens one version up, which
    // is the only way to add them.
    function open(spec, version, done, retry) {
        var req;
        try {
            req = indexedDB.open(spec.name, version);
        } catch (e) {
            done('open threw: ' + spec.name);
            return;
        }
        req.onerror = function() { done('open failed: ' + spec.name); };
        req.onblocked = function() { S.waiting = 'blocked by another connection: ' + spec.name; };
        req.onupgradeneeded = function() {
            var db = req.result;
            (spec.stores || []).forEach(function(store) {
                if (db.objectStoreNames.contains(store.name)) return;
                try {
                    S.trace.push('create ' + store.name);
                    db.createObjectStore(store.name, {
                        keyPath: store.keyPath === null ? undefined : store.keyPath,
                        autoIncrement: !!store.autoIncrement,
                    });
                } catch (e) {}
            });
        };
        req.onsuccess = function() {
            var db = req.result;
            var wanted = (spec.stores || []).map(function(s) { return s.name; });
            var names = wanted.filter(function(n) { return db.objectStoreNames.contains(n); });
            S.trace.push('opened v' + db.version + ' stores=' + names.join(','));
            if (names.length < wanted.length && retry) {
                var next = db.version + 1;
                db.close();
                S.trace.push('reopen@v' + next);
                open(spec, next, done, false);
                return;
            }
            if (!names.length) { db.close(); done('no stores restored: ' + spec.name); return; }
            var tx;
            try {
                tx = db.transaction(names, 'readwrite');
            } catch (e) {
                db.close();
                done('transaction failed: ' + spec.name);
                return;
            }
            (spec.stores || []).forEach(function(store) {
                if (names.indexOf(store.name) < 0) return;
                var os = tx.objectStore(store.name);
                (store.records || []).forEach(function(rec) {
                    try {
                        // An in-line-key store derives the key from the
                        // value and rejects an explicit one.
                        if (os.keyPath === null || os.keyPath === undefined) {
                            os.put(rec.value, rec.key);
                        } else {
                            os.put(rec.value);
                        }
                    } catch (e) {}
                });
            });
            tx.oncomplete = function() { S.trace.push('wrote ' + names.join(',')); db.close(); done(); };
            tx.onerror = function() { db.close(); done('write failed: ' + spec.name); };
            tx.onabort = function() { db.close(); done('write aborted: ' + spec.name); };
        };
    }
})()
