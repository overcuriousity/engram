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

  // The one place that adds the bearer header and handles unreachable servers and rejected tokens.
  async send(path, init = {}) {
    const cfg = await this.config();
    if (!cfg) throw new Error('Not paired yet.');

    let res;
    try {
      res = await fetch(cfg.origin + path, {
        ...init,
        // A cookie would put the request back under the server's origin check.
        credentials: 'omit',
        headers: {
          ...(init.headers || {}),
          authorization: 'Bearer ' + cfg.token,
        },
      });
    } catch (e) {
      // An abort is the panel's own doing, not an unreachable deployment.
      if (e.name === 'AbortError') throw e;
      // Not queued: the operator is told, rather than the capture held in a queue that may never drain.
      throw new Error('Server unreachable.');
    }

    if (res.status === 401) {
      // Cleared, since every caller reads a stored token as "paired".
      await this.forget();
      throw new Error('Token no longer works; pair again.');
    }
    return res;
  },

  // The server's own text is kept.
  async call(path, init = {}) {
    const res = await this.send(path, init);
    const body = await res.json().catch(() => ({}));
    if (!res.ok) throw new Error(body.error || 'engram answered ' + res.status);
    return body;
  },

  // SSE read by hand: `EventSource` cannot carry the bearer header. `signal` aborts an abandoned ask,
  // which would otherwise hold the server's lane until `infer.ask.timeout_secs`.
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
    // The tail of a chunk is held until the blank line that completes its frame.
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
          // Multiple `data:` lines join with newlines, as SSE allows.
          else if (line.startsWith('data:')) data += (data ? '\n' : '') + line.slice(5).trim();
        }
        // A keep-alive comment produces no event.
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
