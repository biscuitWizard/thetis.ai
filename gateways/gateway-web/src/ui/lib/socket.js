/* The websocket, with reconnect and expired-login recovery.
 *
 * Frames are handed to whoever registers for their `type`, so adding a message
 * kind means registering a handler rather than editing a switch.
 */

export class Connection {
  constructor({ onStatus }) {
    this.socket = null;
    this.handlers = new Map();
    this.onStatus = onStatus;
    this.retryDelay = 400;
    this.openHooks = [];
    this.failures = 0;
    this.everOpened = false;
  }

  on(type, handler) {
    this.handlers.set(type, handler);
    return this;
  }

  onOpen(hook) {
    this.openHooks.push(hook);
    return this;
  }

  connect() {
    const scheme = location.protocol === "https:" ? "wss" : "ws";
    this.socket = new WebSocket(`${scheme}://${location.host}/ws`);

    this.socket.onopen = () => {
      this.retryDelay = 400;
      this.failures = 0;
      this.everOpened = true;
      this.onStatus("online", "connected");
      this.openHooks.forEach((hook) => hook());
    };

    this.socket.onclose = () => {
      this.onStatus("offline", "reconnecting…");
      this.failures += 1;
      if (this.failures >= 3) {
        fetch("/api/me", { credentials: "same-origin" })
          .then((response) => {
            if (response.status === 401) {
              const next = encodeURIComponent(location.pathname + location.search);
              location.assign(`/login?next=${next}`);
            }
          })
          .catch(() => {});
      }
      setTimeout(() => this.connect(), this.retryDelay);
      // Back off, but stay responsive enough that a restart feels instant.
      this.retryDelay = Math.min(this.retryDelay * 2, 8000);
    };

    this.socket.onerror = () => this.onStatus("offline", "connection error");

    this.socket.onmessage = (event) => {
      let frame;
      try {
        frame = JSON.parse(event.data);
      } catch {
        return;
      }
      this.handlers.get(frame.type)?.(frame);
    };
  }

  send(frame) {
    if (this.socket?.readyState === WebSocket.OPEN) {
      this.socket.send(JSON.stringify(frame));
      return true;
    }
    return false;
  }
}
