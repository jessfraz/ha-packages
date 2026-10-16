// The startup attempt uses the same requests as the dashboard. Retrying never
// creates a credential or changes pairing; Home Assistant owns authentication.
export async function waitForConnection(connect, { homeAssistant, signal, onWaiting }) {
  let delay = 1_000;
  for (;;) {
    signal.throwIfAborted();
    const attempt = new AbortController();
    const cancel = () => attempt.abort(signal.reason);
    signal.addEventListener("abort", cancel, { once: true });
    const timeout = setTimeout(() => attempt.abort(new DOMException("Connection timed out", "TimeoutError")), 15_000);
    try {
      const result = await connect(attempt.signal);
      signal.throwIfAborted();
      return result;
    } catch (error) {
      signal.throwIfAborted();
      if (!homeAssistant) throw error;
      onWaiting(error);
    } finally {
      clearTimeout(timeout);
      signal.removeEventListener("abort", cancel);
      // Cancel the other handshake read if Promise.all rejected early.
      attempt.abort();
    }
    await new Promise((resolve, reject) => {
      const cancelWait = () => {
        clearTimeout(timer);
        reject(signal.reason);
      };
      const timer = setTimeout(() => {
        signal.removeEventListener("abort", cancelWait);
        resolve();
      }, delay);
      signal.addEventListener("abort", cancelWait, { once: true });
      if (signal.aborted) cancelWait();
    });
    delay = Math.min(delay * 2, 15_000);
  }
}
