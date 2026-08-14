globalThis.engramPair = {
  // Pair through the browser's own auth-flow window.
  //
  // The redirect target is a sink the browser intercepts and never loads, so
  // the token comes back to this extension and to nothing else. It arrives in
  // the fragment, which is never sent to a server and does not land in a proxy
  // log. No credential is ever written into a downloadable file.
  async pair(origin) {
    const redirect = engramShim.identity.getRedirectURL();
    const state = crypto.randomUUID();
    const url = origin + '/ui/pair'
      + '?redirect_uri=' + encodeURIComponent(redirect)
      + '&state=' + encodeURIComponent(state);

    const done = await engramShim.identity.launchWebAuthFlow({ url, interactive: true });
    const fragment = new URLSearchParams(done.split('#')[1] || '');

    // The nonce this flow started with, echoed back. A mismatch means the
    // response belongs to some other flow, and the token in it is not one we
    // asked for.
    if (fragment.get('state') !== state) throw new Error('Pairing was tampered with.');
    const token = fragment.get('token');
    if (!token) throw new Error('engram returned no token.');

    // The origin the deployment reported for itself, not the one typed: the
    // extension asks the browser for permission to reach exactly that host.
    const learned = fragment.get('origin') || origin;
    const granted = await engramShim.permissions.request({ origins: [learned + '/*'] });
    if (!granted) throw new Error('Permission for ' + learned + ' was declined.');

    await engramApi.save(learned, token);
    return learned;
  },

  // The fallback for when `launchWebAuthFlow` is unavailable: a token pasted
  // from Housekeeping → API tokens. Same end state, one more step.
  async pairManually(origin, token) {
    const granted = await engramShim.permissions.request({ origins: [origin + '/*'] });
    if (!granted) throw new Error('Permission for ' + origin + ' was declined.');
    await engramApi.save(origin, token);
    return origin;
  },
};
