// Body of an IIFE — caller defines `payload` from JSON.parse(<...>)
// before invoking this snippet. Restores localStorage + sessionStorage
// only; cookies are restored by the host-side cookie store API, which
// (unlike document.cookie) sees HttpOnly entries.
if (payload.localStorage) {
    for (const k of Object.keys(payload.localStorage)) {
        try { localStorage.setItem(k, payload.localStorage[k]); } catch (e) {}
    }
}
if (payload.sessionStorage) {
    for (const k of Object.keys(payload.sessionStorage)) {
        try { sessionStorage.setItem(k, payload.sessionStorage[k]); } catch (e) {}
    }
}
return 'ok';
