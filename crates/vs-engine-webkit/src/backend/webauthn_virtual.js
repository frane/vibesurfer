// Virtual WebAuthn authenticator — a pure-JS software authenticator that
// overrides navigator.credentials.create / .get so passkey registration
// and login work in the headless engine with no platform authenticator
// and no automation protocol attached. It is a plain injected script,
// the same surface as the snapshot walker and the rAF shim: there is no
// CDP, no WebDriver, navigator.webdriver stays undefined.
//
// It implements a CTAP-style software authenticator: ES256 (P-256)
// credentials via WebCrypto, "none" attestation, DER-encoded assertion
// signatures over authenticatorData || SHA-256(clientDataJSON). Registered
// credentials live on window.__vsWebAuthn so a create() in one step and a
// get() in a later step on the same document share the keystore.
(function () {
  if (window.__vsWebAuthn && window.__vsWebAuthn.installed) return;
  const store = (window.__vsWebAuthn = window.__vsWebAuthn || { creds: [], installed: false });

  const subtle = crypto.subtle;

  function u8(buf) { return buf instanceof Uint8Array ? buf : new Uint8Array(buf); }
  function concat(arrays) {
    let len = 0;
    for (const a of arrays) len += a.length;
    const out = new Uint8Array(len);
    let o = 0;
    for (const a of arrays) { out.set(a, o); o += a.length; }
    return out;
  }
  function toBufferSource(x) {
    if (x == null) return new Uint8Array(0);
    if (x instanceof ArrayBuffer) return new Uint8Array(x);
    if (ArrayBuffer.isView(x)) return new Uint8Array(x.buffer, x.byteOffset, x.byteLength);
    return new Uint8Array(x);
  }
  function b64url(bytes) {
    let s = '';
    const b = u8(bytes);
    for (let i = 0; i < b.length; i++) s += String.fromCharCode(b[i]);
    return btoa(s).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
  }
  async function sha256(bytes) { return u8(await subtle.digest('SHA-256', u8(bytes))); }

  // --- Minimal CBOR encoder (the subset WebAuthn needs) ---------------
  function cborHead(major, len) {
    if (len < 24) return new Uint8Array([(major << 5) | len]);
    if (len < 256) return new Uint8Array([(major << 5) | 24, len]);
    if (len < 65536) return new Uint8Array([(major << 5) | 25, len >> 8, len & 0xff]);
    return new Uint8Array([(major << 5) | 26, (len >>> 24) & 0xff, (len >>> 16) & 0xff, (len >>> 8) & 0xff, len & 0xff]);
  }
  function cborUint(n) { return cborHead(0, n); }
  function cborNegint(n) { return cborHead(1, -n - 1); } // n is negative
  function cborBytes(bytes) { const b = u8(bytes); return concat([cborHead(2, b.length), b]); }
  function cborText(str) { const b = new TextEncoder().encode(str); return concat([cborHead(3, b.length), b]); }
  function cborInt(n) { return n < 0 ? cborNegint(n) : cborUint(n); }
  // entries: array of [keyBytes, valueBytes]; caller pre-encodes keys/values.
  function cborMap(entries) {
    const parts = [cborHead(5, entries.length)];
    for (const [k, v] of entries) { parts.push(k, v); }
    return concat(parts);
  }

  // COSE_Key for an ES256 P-256 public key from raw x/y (32 bytes each).
  function coseKey(x, y) {
    return cborMap([
      [cborInt(1), cborInt(2)],    // kty: EC2
      [cborInt(3), cborInt(-7)],   // alg: ES256
      [cborInt(-1), cborInt(1)],   // crv: P-256
      [cborInt(-2), cborBytes(x)], // x
      [cborInt(-3), cborBytes(y)], // y
    ]);
  }

  // Raw (r||s, 64 bytes) ECDSA signature -> ASN.1 DER, as WebAuthn wants.
  function rawSigToDer(raw) {
    const r = raw.slice(0, 32), s = raw.slice(32, 64);
    function trim(intBytes) {
      let i = 0;
      while (i < intBytes.length - 1 && intBytes[i] === 0) i++;
      let v = intBytes.slice(i);
      if (v[0] & 0x80) v = concat([new Uint8Array([0]), v]); // keep positive
      return v;
    }
    const rt = trim(r), st = trim(s);
    const body = concat([
      new Uint8Array([0x02, rt.length]), rt,
      new Uint8Array([0x02, st.length]), st,
    ]);
    return concat([new Uint8Array([0x30, body.length]), body]);
  }

  function randomBytes(n) { const b = new Uint8Array(n); crypto.getRandomValues(b); return b; }

  function authData(rpIdHash, flags, signCount, attested) {
    const sc = new Uint8Array([(signCount >>> 24) & 0xff, (signCount >>> 16) & 0xff, (signCount >>> 8) & 0xff, signCount & 0xff]);
    const head = concat([u8(rpIdHash), new Uint8Array([flags]), sc]);
    return attested ? concat([head, attested]) : head;
  }

  async function create(options) {
    const pk = options.publicKey;
    const rpId = (pk.rp && pk.rp.id) || location.hostname;
    const params = pk.pubKeyCredParams || [{ type: 'public-key', alg: -7 }];
    if (!params.some((p) => p.alg === -7)) {
      throw new DOMException('virtual authenticator only supports ES256 (-7)', 'NotSupportedError');
    }
    const keyPair = await subtle.generateKey({ name: 'ECDSA', namedCurve: 'P-256' }, true, ['sign', 'verify']);
    const jwk = await subtle.exportKey('jwk', keyPair.publicKey);
    const x = toBufferSource(b64urlDecode(jwk.x));
    const y = toBufferSource(b64urlDecode(jwk.y));

    const credId = randomBytes(32);
    const rpIdHash = await sha256(new TextEncoder().encode(rpId));
    const aaguid = new Uint8Array(16); // all-zero AAGUID
    const credIdLen = new Uint8Array([(credId.length >> 8) & 0xff, credId.length & 0xff]);
    const attestedCredData = concat([aaguid, credIdLen, credId, coseKey(x, y)]);
    // flags: UP(0x01) | UV(0x04) | AT(0x40) = 0x45
    const ad = authData(rpIdHash, 0x45, 0, attestedCredData);
    const attestationObject = cborMap([
      [cborText('fmt'), cborText('none')],
      [cborText('attStmt'), cborMap([])],
      [cborText('authData'), cborBytes(ad)],
    ]);
    const clientData = new TextEncoder().encode(JSON.stringify({
      type: 'webauthn.create',
      challenge: b64url(toBufferSource(pk.challenge)),
      origin: location.origin,
      crossOrigin: false,
    }));

    store.creds.push({
      id: credId, rpId, privateKey: keyPair.privateKey, publicKey: keyPair.publicKey,
      userHandle: pk.user ? toBufferSource(pk.user.id) : new Uint8Array(0), signCount: 0,
    });

    return makeCredential(credId, {
      clientDataJSON: clientData.buffer,
      attestationObject: attestationObject.buffer,
      getAuthenticatorData: () => ad.buffer,
      getPublicKey: () => null,
      getPublicKeyAlgorithm: () => -7,
      getTransports: () => ['internal'],
    });
  }

  async function get(options) {
    const pk = options.publicKey;
    const rpId = pk.rpId || location.hostname;
    let cred = null;
    if (pk.allowCredentials && pk.allowCredentials.length) {
      const wanted = pk.allowCredentials.map((c) => b64url(toBufferSource(c.id)));
      cred = store.creds.find((c) => c.rpId === rpId && wanted.includes(b64url(c.id)));
    } else {
      cred = store.creds.find((c) => c.rpId === rpId);
    }
    if (!cred) throw new DOMException('no virtual credential for this rp', 'NotAllowedError');

    cred.signCount++;
    const rpIdHash = await sha256(new TextEncoder().encode(rpId));
    const ad = authData(rpIdHash, 0x05, cred.signCount, null); // UP|UV, no AT
    const clientData = new TextEncoder().encode(JSON.stringify({
      type: 'webauthn.get',
      challenge: b64url(toBufferSource(pk.challenge)),
      origin: location.origin,
      crossOrigin: false,
    }));
    const clientDataHash = await sha256(clientData);
    const signed = concat([ad, clientDataHash]);
    const raw = u8(await subtle.sign({ name: 'ECDSA', hash: 'SHA-256' }, cred.privateKey, signed));
    const der = rawSigToDer(raw);

    return makeCredential(cred.id, {
      clientDataJSON: clientData.buffer,
      authenticatorData: ad.buffer,
      signature: der.buffer,
      userHandle: cred.userHandle.length ? cred.userHandle.buffer : null,
    });
  }

  function makeCredential(rawId, response) {
    const raw = u8(rawId);
    return {
      id: b64url(raw),
      rawId: raw.buffer,
      type: 'public-key',
      authenticatorAttachment: 'platform',
      response,
      getClientExtensionResults: () => ({}),
    };
  }

  function b64urlDecode(s) {
    s = s.replace(/-/g, '+').replace(/_/g, '/');
    const pad = '='.repeat((4 - (s.length % 4)) % 4);
    const bin = atob(s + pad);
    const b = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i++) b[i] = bin.charCodeAt(i);
    return b;
  }

  // Install the overrides. Keep PublicKeyCredential defined so feature
  // detection (`window.PublicKeyCredential`) still passes.
  if (!window.PublicKeyCredential) {
    window.PublicKeyCredential = function PublicKeyCredential() {};
  }
  window.PublicKeyCredential.isUserVerifyingPlatformAuthenticatorAvailable = () => Promise.resolve(true);
  window.PublicKeyCredential.isConditionalMediationAvailable = () => Promise.resolve(false);

  if (!navigator.credentials) navigator.credentials = {};
  navigator.credentials.create = function (options) {
    if (options && options.publicKey) return create(options);
    return Promise.reject(new DOMException('unsupported credential type', 'NotSupportedError'));
  };
  navigator.credentials.get = function (options) {
    if (options && options.publicKey) return get(options);
    return Promise.reject(new DOMException('unsupported credential type', 'NotSupportedError'));
  };

  store.installed = true;
  return 'installed';
})();
