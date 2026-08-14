globalThis.engramPair = {
  // Pair through the browser's own auth-flow window.
  //
  // The redirect target is a sink the browser intercepts and never loads, so
  // the token comes back to this extension and to nothing else. It arrives in
  // the fragment, which is never sent to a server and does not land in a proxy
  // log. No credential is ever written into a downloadable file.
  //
  // Host permission is deliberately *not* requested here. `permissions.request`
  // is only honoured while the browser still counts a click as a user gesture,
  // and by the time this returns that gesture is long spent: the auth flow is a
  // whole trip out through a browser window and back. The panel asks for
  // permission first, from inside the click, and calls this afterwards.
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

    // The address configured here, not the one the deployment reports for
    // itself. A deployment behind a proxy knows an internal host name, or a
    // scheme its proxy did not forward, while the browser reaches it at
    // another; what was typed is the address that resolves, and it is the one
    // host permission was just granted for.
    await engramApi.save(origin, token);
    return origin;
  },
};
