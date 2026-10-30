/**
 * Nexus Widget — embeddable customer chat for Nexus-Hosting sites.
 *
 * Usage: add to any page:
 *   <script src="https://your-nexus-node.example/widget/nexus-widget.js"
 *           data-widget-id="abc123xyz"
 *           data-api-base="https://your-nexus-node.example/api/v1"
 *           async></script>
 *
 * The script auto-initialises on load. One widget per page.
 */

interface WidgetConfig {
  widget_id: string;
  name: string;
  greeting: string | null;
  active: boolean;
}

interface WidgetMessage {
  id: string;
  widget_id: string;
  visitor_id: string;
  visitor_name: string | null;
  content: string;
  reply_message_id: string | null;
  created_at: string;
}

const VISITOR_KEY = "nexus_widget_visitor_id";
const NAME_KEY = "nexus_widget_visitor_name";
const POLL_INTERVAL_MS = 8000;

function getOrCreateVisitorId(): string {
  try {
    let id = localStorage.getItem(VISITOR_KEY);
    if (!id) {
      id = crypto.randomUUID();
      localStorage.setItem(VISITOR_KEY, id);
    }
    return id;
  } catch {
    return crypto.randomUUID();
  }
}

function getStoredName(): string {
  try {
    return localStorage.getItem(NAME_KEY) ?? "";
  } catch {
    return "";
  }
}

function storeName(name: string) {
  try {
    localStorage.setItem(NAME_KEY, name);
  } catch {}
}

class NexusWidgetUI {
  private widgetId: string;
  private apiBase: string;
  private visitorId: string;
  private visitorName: string;
  private config: WidgetConfig | null = null;
  private messages: WidgetMessage[] = [];
  private open = false;
  private container: HTMLElement;
  private bubble: HTMLElement;
  private panel: HTMLElement;
  private messageList: HTMLElement;
  private input: HTMLInputElement;
  private sendBtn: HTMLButtonElement;
  private pollTimer: ReturnType<typeof setInterval> | null = null;

  constructor(widgetId: string, apiBase: string) {
    this.widgetId = widgetId;
    this.apiBase = apiBase.replace(/\/$/, "");
    this.visitorId = getOrCreateVisitorId();
    this.visitorName = getStoredName();

    this.container = document.createElement("div");
    this.container.id = "nexus-widget-root";
    this.container.style.cssText =
      "position:fixed;bottom:20px;right:20px;z-index:2147483647;font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',sans-serif;";

    this.bubble = this.createBubble();
    this.panel = this.createPanel();
    this.messageList = this.panel.querySelector<HTMLElement>(".nw-messages")!;
    this.input = this.panel.querySelector<HTMLInputElement>(".nw-input")!;
    this.sendBtn = this.panel.querySelector<HTMLButtonElement>(".nw-send")!;

    this.container.appendChild(this.panel);
    this.container.appendChild(this.bubble);
    document.body.appendChild(this.container);

    this.bubble.addEventListener("click", () => this.toggleOpen());
    this.sendBtn.addEventListener("click", () => this.handleSend());
    this.input.addEventListener("keydown", (e) => {
      if (e.key === "Enter" && !e.shiftKey) {
        e.preventDefault();
        this.handleSend();
      }
    });

    this.init();
  }

  private createBubble(): HTMLElement {
    const el = document.createElement("button");
    el.setAttribute("aria-label", "Open chat");
    el.style.cssText = [
      "width:56px;height:56px;border-radius:50%;border:none;cursor:pointer",
      "background:#5865f2;color:#fff;font-size:24px;line-height:1",
      "box-shadow:0 4px 16px rgba(0,0,0,.24);transition:transform .15s",
    ].join(";");
    el.innerHTML = `<svg width="24" height="24" viewBox="0 0 24 24" fill="none" xmlns="http://www.w3.org/2000/svg"><path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z" fill="currentColor"/></svg>`;
    el.addEventListener("mouseenter", () => (el.style.transform = "scale(1.08)"));
    el.addEventListener("mouseleave", () => (el.style.transform = "scale(1)"));
    return el;
  }

  private createPanel(): HTMLElement {
    const el = document.createElement("div");
    el.style.cssText = [
      "display:none;flex-direction:column;width:340px;height:460px",
      "border-radius:16px;overflow:hidden;box-shadow:0 8px 32px rgba(0,0,0,.28)",
      "background:#fff;margin-bottom:12px",
    ].join(";");

    el.innerHTML = `
      <div class="nw-header" style="background:#5865f2;color:#fff;padding:14px 16px;display:flex;align-items:center;gap:10px;">
        <div style="width:36px;height:36px;border-radius:50%;background:rgba(255,255,255,.2);display:flex;align-items:center;justify-content:center;">
          <svg width="18" height="18" viewBox="0 0 24 24" fill="none"><path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z" fill="currentColor"/></svg>
        </div>
        <div>
          <div class="nw-title" style="font-weight:600;font-size:15px;line-height:1.2;">Chat</div>
          <div class="nw-subtitle" style="font-size:12px;opacity:.8;">We usually reply quickly</div>
        </div>
        <button class="nw-close" aria-label="Close" style="margin-left:auto;background:none;border:none;color:#fff;cursor:pointer;padding:4px;opacity:.8;line-height:1;">✕</button>
      </div>
      <div class="nw-name-bar" style="padding:12px 16px;background:#f9fafb;border-bottom:1px solid #e5e7eb;display:none;">
        <label style="font-size:12px;color:#6b7280;display:block;margin-bottom:4px;">Your name (optional)</label>
        <input class="nw-name-input" type="text" maxlength="50" placeholder="Enter your name…"
          style="width:100%;box-sizing:border-box;padding:6px 10px;border:1px solid #d1d5db;border-radius:8px;font-size:14px;outline:none;" />
      </div>
      <div class="nw-messages" style="flex:1;overflow-y:auto;padding:16px;display:flex;flex-direction:column;gap:10px;background:#f9fafb;"></div>
      <div style="padding:12px 16px;background:#fff;border-top:1px solid #e5e7eb;display:flex;gap:8px;align-items:flex-end;">
        <input class="nw-input" type="text" maxlength="2000" placeholder="Type a message…"
          style="flex:1;padding:9px 12px;border:1px solid #d1d5db;border-radius:10px;font-size:14px;outline:none;resize:none;" />
        <button class="nw-send" style="padding:9px 14px;background:#5865f2;color:#fff;border:none;border-radius:10px;cursor:pointer;font-size:14px;font-weight:600;white-space:nowrap;">Send</button>
      </div>
    `;

    el.querySelector(".nw-close")!.addEventListener("click", () => this.toggleOpen());

    const nameInput = el.querySelector<HTMLInputElement>(".nw-name-input")!;
    nameInput.value = this.visitorName;
    nameInput.addEventListener("change", () => {
      this.visitorName = nameInput.value.trim();
      storeName(this.visitorName);
    });

    return el;
  }

  private async init() {
    try {
      const resp = await fetch(
        `${this.apiBase}/widget/${this.widgetId}/config`
      );
      if (!resp.ok) return;
      this.config = (await resp.json()) as WidgetConfig;

      if (!this.config.active) return;

      const titleEl = this.panel.querySelector<HTMLElement>(".nw-title");
      if (titleEl) titleEl.textContent = this.config.name;

      if (this.config.greeting) {
        this.appendSystemMessage(this.config.greeting);
      }

      this.bubble.style.display = "flex";
      this.bubble.style.alignItems = "center";
      this.bubble.style.justifyContent = "center";

      // Show name bar if visitor hasn't set a name yet
      if (!this.visitorName) {
        (
          this.panel.querySelector<HTMLElement>(".nw-name-bar")!
        ).style.display = "block";
      }

      await this.loadMessages();
    } catch {
      // silently fail if widget can't connect
    }
  }

  private async loadMessages() {
    try {
      const resp = await fetch(
        `${this.apiBase}/widget/${this.widgetId}/messages?visitor_id=${encodeURIComponent(this.visitorId)}&limit=50`
      );
      if (!resp.ok) return;
      const msgs = (await resp.json()) as WidgetMessage[];
      // API returns newest-first; reverse for display
      msgs.reverse();
      this.messages = msgs;
      this.renderMessages();
    } catch {}
  }

  private renderMessages() {
    this.messageList.innerHTML = "";
    if (this.messages.length === 0 && this.config?.greeting) {
      this.appendSystemMessage(this.config.greeting);
      return;
    }
    for (const msg of this.messages) {
      this.appendMessageBubble(msg);
    }
    this.scrollToBottom();
  }

  private appendSystemMessage(text: string) {
    const el = document.createElement("div");
    el.style.cssText =
      "text-align:center;font-size:12px;color:#9ca3af;padding:4px 0;";
    el.textContent = text;
    this.messageList.appendChild(el);
  }

  private appendMessageBubble(msg: WidgetMessage) {
    const isVisitor = msg.visitor_id === this.visitorId && !msg.reply_message_id;
    const isReply = !!msg.reply_message_id;

    const wrapper = document.createElement("div");
    wrapper.style.cssText = `display:flex;flex-direction:column;align-items:${isVisitor && !isReply ? "flex-end" : "flex-start"};`;

    const bubble = document.createElement("div");
    const bgColor = isReply ? "#f3f4f6" : "#5865f2";
    const textColor = isReply ? "#111827" : "#fff";
    bubble.style.cssText = [
      `background:${bgColor};color:${textColor}`,
      "max-width:80%;padding:8px 12px;border-radius:14px;font-size:14px;line-height:1.4",
      isReply ? "border-radius:4px 14px 14px 14px" : "border-radius:14px 14px 4px 14px",
    ].join(";");
    bubble.textContent = msg.content;

    const meta = document.createElement("div");
    meta.style.cssText = "font-size:11px;color:#9ca3af;margin-top:2px;padding:0 4px;";
    const label = isReply
      ? "Support"
      : msg.visitor_name || "You";
    const time = new Date(msg.created_at).toLocaleTimeString([], {
      hour: "2-digit",
      minute: "2-digit",
    });
    meta.textContent = `${label} · ${time}`;

    wrapper.appendChild(bubble);
    wrapper.appendChild(meta);
    this.messageList.appendChild(wrapper);
  }

  private scrollToBottom() {
    this.messageList.scrollTop = this.messageList.scrollHeight;
  }

  private async handleSend() {
    const content = this.input.value.trim();
    if (!content) return;

    this.input.value = "";
    this.sendBtn.disabled = true;

    try {
      const resp = await fetch(
        `${this.apiBase}/widget/${this.widgetId}/messages`,
        {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({
            visitor_id: this.visitorId,
            visitor_name: this.visitorName || undefined,
            content,
          }),
        }
      );

      if (resp.ok) {
        const msg = (await resp.json()) as WidgetMessage;
        this.messages.push(msg);
        this.appendMessageBubble(msg);
        this.scrollToBottom();
      }
    } catch {
      // best-effort
    } finally {
      this.sendBtn.disabled = false;
      this.input.focus();
    }
  }

  private startPolling() {
    if (this.pollTimer) return;
    this.pollTimer = setInterval(async () => {
      await this.loadMessages();
    }, POLL_INTERVAL_MS);
  }

  private stopPolling() {
    if (this.pollTimer) {
      clearInterval(this.pollTimer);
      this.pollTimer = null;
    }
  }

  toggleOpen() {
    this.open = !this.open;
    this.panel.style.display = this.open ? "flex" : "none";

    if (this.open) {
      this.startPolling();
      this.scrollToBottom();
      this.input.focus();
      this.bubble.setAttribute("aria-label", "Close chat");
      this.bubble.innerHTML = `<svg width="22" height="22" viewBox="0 0 24 24" fill="none"><path d="M18 6L6 18M6 6l12 12" stroke="currentColor" stroke-width="2.5" stroke-linecap="round"/></svg>`;
    } else {
      this.stopPolling();
      this.bubble.setAttribute("aria-label", "Open chat");
      this.bubble.innerHTML = `<svg width="24" height="24" viewBox="0 0 24 24" fill="none"><path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z" fill="currentColor"/></svg>`;
    }
  }
}

// ── Auto-init from <script data-widget-id="..."> ─────────────────────────────

function bootstrap() {
  const scriptEl =
    document.currentScript as HTMLScriptElement | null ??
    document.querySelector<HTMLScriptElement>(
      'script[data-widget-id]'
    );

  if (!scriptEl) return;

  const widgetId = scriptEl.getAttribute("data-widget-id");
  const apiBase =
    scriptEl.getAttribute("data-api-base") ??
    new URL(scriptEl.src).origin + "/api/v1";

  if (!widgetId) {
    console.warn("[NexusWidget] missing data-widget-id attribute");
    return;
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", () => {
      new NexusWidgetUI(widgetId, apiBase);
    });
  } else {
    new NexusWidgetUI(widgetId, apiBase);
  }
}

bootstrap();
