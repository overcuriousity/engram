globalThis.engramApi = {
  async config() {
    const { origin, token } = await engramShim.storage.local.get(['origin', 'token']);
    return origin && token ? { origin, token } : null;
  },

  async save(origin, token) {
    await engramShim.storage.local.set({ origin, token });
  },

  async forget() {
    await engramShim.storage.local.remove(['origin', 'token']);
  },

  // Every request goes through here so there is one place that knows about the
  // bearer header, one place that decides what an unreachable deployment
  // means, and one place that handles a token the server no longer accepts.
  // Both doors below build on it: `call` reads a JSON body, `stream` reads a
  // body that is still arriving.
  async send(path, init = {}) {
    const cfg = await this.config();
    if (!cfg) throw new Error('Not paired yet.');

    let res;
    try {
      res = await fetch(cfg.origin + path, {
        ...init,
        headers: {
          ...(init.headers || {}),
          authorization: 'Bearer ' + cfg.token,
        },
      });
    } catch (e) {
      // An abort is the panel's own doing — the operator moved on — and the
      // caller that aborted knows what it did. Reporting it as a deployment
      // that cannot be reached would be a lie about the deployment.
      if (e.name === 'AbortError') throw e;
      // Unreachable is its own case, and it is not queued: the capture is
      // lost and the operator is told, rather than silently held in a queue
      // that may never drain.
      throw new Error('engram is unreachable.');
    }

    if (res.status === 401) {
      // Cleared here, not merely reported. Every caller treats `config()`
      // returning something as "paired", so a revoked token left in storage
      // is not a failed call — it is a panel that can never be used again
      // short of reinstalling the extension.
      await this.forget();
      throw new Error('That token no longer works — pair again.');
    }
    return res;
  },

  // The server's own text is kept: "that file is `application/pdf`" is useful
  // and "request failed" is not.
  async call(path, init = {}) {
    const res = await this.send(path, init);
    const body = await res.json().catch(() => ({}));
    if (!res.ok) throw new Error(body.error || 'engram answered ' + res.status);
    return body;
  },

  // Server-sent events read by hand, because `EventSource` cannot carry the
  // bearer header and this deployment authenticates every route. That is the
  // whole reason `POST /api/v1/ask/stream` exists as a POST that streams its
  // own response rather than as the web UI's park-then-GET pair.
  //
  // `onFrame(name, data)` is called once per event, in order. `data` is the
  // parsed JSON payload — every frame this server sends carries one.
  //
  // `signal` ends the request where it stands. An ask the operator has walked
  // away from is not merely unread: it goes on retrieving and prompting on the
  // deployment, holding the lane the next ask needs, for as long as
  // `infer.ask.timeout_secs` allows. Aborting is how the reader says so.
  async stream(path, body, signal, onFrame) {
    const res = await this.send(path, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(body),
      signal,
    });
    if (!res.ok) {
      const err = await res.json().catch(() => ({}));
      throw new Error(err.error || 'engram answered ' + res.status);
    }

    const reader = res.body.getReader();
    const decode = new TextDecoder();
    // Frames are separated by a blank line and arrive split across whatever
    // chunk boundaries the network chose, so the tail of a chunk is held back
    // until the separator that completes it turns up.
    let buffer = '';
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      buffer += decode.decode(value, { stream: true });

      let cut;
      while ((cut = buffer.indexOf('\n\n')) !== -1) {
        const frame = buffer.slice(0, cut);
        buffer = buffer.slice(cut + 2);

        let name = 'message';
        let data = '';
        for (const line of frame.split('\n')) {
          if (line.startsWith('event:')) name = line.slice(6).trim();
          // Multiple `data:` lines are one payload joined by newlines. This
          // server sends one line per frame, but a keep-alive comment and a
          // multi-line payload are both legal SSE and neither may be read as
          // the end of anything.
          else if (line.startsWith('data:')) data += (data ? '\n' : '') + line.slice(5).trim();
        }
        // A keep-alive is a comment line and produces no event. Nothing to
        // report, and nothing to hand on.
        if (!data) continue;
        let parsed;
        try {
          parsed = JSON.parse(data);
        } catch (e) {
          continue;
        }
        onFrame(name, parsed);
      }
    }
  },
};
