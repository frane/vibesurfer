// Body of an IIFE — caller defines `blob` from JSON.parse(<payload>)
// before invoking this snippet.
if (blob.cookies) {
    for (const c of String(blob.cookies).split(';')) {
        const t = c.trim();
        if (t) document.cookie = t;
    }
}
if (blob.localStorage) {
    for (const k of Object.keys(blob.localStorage)) {
        try { localStorage.setItem(k, blob.localStorage[k]); } catch (e) {}
    }
}
if (blob.sessionStorage) {
    for (const k of Object.keys(blob.sessionStorage)) {
        try { sessionStorage.setItem(k, blob.sessionStorage[k]); } catch (e) {}
    }
}
return 'ok';
