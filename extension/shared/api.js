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

  // Every call goes through here so there is one place that knows about the
  // bearer header, and one place that turns a failure into a message worth
  // showing. The server's own text is kept: "that file is `application/pdf`"
  // is useful and "request failed" is not.
  async call(path, init = {}) {
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
    const body = await res.json().catch(() => ({}));
    if (!res.ok) throw new Error(body.error || 'engram answered ' + res.status);
    return body;
  },
};
