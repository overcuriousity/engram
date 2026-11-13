globalThis.engramPair = {
  // The token returns in the fragment of a redirect the browser intercepts, so no server or log sees it.
  // Host permission is asked for by the panel inside the click, since the gesture is spent by now.
  async pair(origin) {
    const redirect = engramShim.identity.getRedirectURL();
    const state = crypto.randomUUID();
    const url = origin + '/ui/pair'
      + '?redirect_uri=' + encodeURIComponent(redirect)
      + '&state=' + encodeURIComponent(state);

    const done = await engramShim.identity.launchWebAuthFlow({ url, interactive: true });
    const fragment = new URLSearchParams(done.split('#')[1] || '');

    // A mismatched nonce means a token we did not ask for.
    if (fragment.get('state') !== state) throw new Error('Pairing was tampered with.');
    const token = fragment.get('token');
    if (!token) throw new Error('engram returned no token.');

    // The configured address, which resolves and holds the permission; a proxied deployment may report another.
    await engramApi.save(origin, token);
    return origin;
  },
};
