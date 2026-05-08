(function() {
    const cookies = document.cookie || '';
    const ls = {};
    for (let i = 0; i < localStorage.length; i++) {
        const k = localStorage.key(i);
        ls[k] = localStorage.getItem(k);
    }
    const ss = {};
    for (let i = 0; i < sessionStorage.length; i++) {
        const k = sessionStorage.key(i);
        ss[k] = sessionStorage.getItem(k);
    }
    return JSON.stringify({
        version: 1,
        url: location.href,
        origin: location.origin,
        cookies,
        localStorage: ls,
        sessionStorage: ss,
    });
})()
