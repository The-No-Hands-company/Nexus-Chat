import { create } from "zustand";
import { persist } from "zustand/middleware";

const DEFAULT_SERVER_URL = import.meta.env.DEV ? "http://localhost:8080" : "";

function normalizeServerUrl(url: string): string {
  return url.trim().replace(/\/$/, "").replace(/\/api\/v1$/, "");
}

/**
 * The server this client talks to.
 *
 * Always the origin that served the page. Under ecosystem SSO that is not a
 * preference — the proxy injects the identity header for *this* hostname, so a
 * request sent anywhere else arrives unauthenticated however signed in you are.
 *
 * This used to read a `nexus_server_url` left in localStorage by the old
 * "type your server" login. Anyone carrying that value from before the cutover
 * had their bootstrap sent to the wrong origin and was told "Not signed in" on
 * a host they were perfectly signed in to.
 *
 * The dev override stays: a local Vite server on :5173 genuinely is a
 * different origin from the API on :8080.
 */
function currentServerOrigin(): string {
  if (import.meta.env.DEV) {
    return normalizeServerUrl(
      localStorage.getItem("nexus_server_url") ?? DEFAULT_SERVER_URL,
    );
  }
  return window.location.origin;
}

export function getServerUrlPlaceholder(): string {
  return import.meta.env.DEV ? "http://localhost:8080" : "https://your-nexus-server.com";
}

function orderMessages(messages: NxMessage[]): NxMessage[] {
  return [...messages].sort((a, b) => new Date(a.created_at).getTime() - new Date(b.created_at).getTime());
}

/**
 * Settle an optimistic message against the server's version of it.
 *
 * The gateway broadcasts MESSAGE_CREATE to everyone including the sender, and
 * that echo frequently arrives before the POST response. When it does, the
 * real message is already in the list, so swapping the placeholder for `sent`
 * leaves the same message twice — which is exactly why every message showed up
 * in pairs. onMessageCreate dedupes by id, but it has no way to know the
 * placeholder's temp id refers to the same message.
 *
 * Exported so the ordering race can be tested directly, without a DOM or a
 * live socket.
 */
export function reconcileSent(
  list: NxMessage[],
  optimisticId: string,
  sent: NxMessage,
): NxMessage[] {
  const alreadyArrived = list.some((m) => m.id === sent.id);
  return alreadyArrived
    ? list.filter((m) => m.id !== optimisticId)
    : list.map((m) => (m.id === optimisticId ? sent : m));
}

function updateReactionState(
  messages: NxMessage[],
  messageId: string,
  emoji: string,
  delta: number,
): NxMessage[] {
  return messages.map((m) => {
    if (m.id !== messageId) return m;
    const reactions = [...(m.reactions ?? [])];
    const idx = reactions.findIndex((r) => r.emoji === emoji);
    if (idx >= 0) {
      const updated = [...reactions];
      const nextCount = updated[idx].count + delta;
      if (nextCount <= 0) {
        updated.splice(idx, 1);
      } else {
        updated[idx] = { ...updated[idx], count: nextCount, me: delta > 0 ? true : false };
      }
      return { ...m, reactions: updated };
    }
    if (delta > 0) {
      return { ...m, reactions: [...reactions, { emoji, count: 1, me: true }] };
    }
    return m;
  });
}

// ── Types ─────────────────────────────────────────────────────────────────────

export interface Session {
  /**
   * Empty under ecosystem SSO. This client holds no credential at all: the
   * proxy authenticates the browser and injects a signed identity header the
   * server verifies. Kept on the type so the many call sites that read it
   * still compile; nothing is sent.
   */
  accessToken: string;
  refreshToken: string;
  username: string;
  userId: string;
  avatar: string | null;
  serverUrl: string;
  displayName?: string | null;
  bio?: string | null;
  status?: string | null;
}

export interface NxServer {
  id: string;
  name: string;
  icon: string | null;
  description: string | null;
  owner_id: string;
}

export interface NxChannel {
  id: string;
  name: string | null;
  /**
   * `channel_type` on the wire, like every other field here. It was declared
   * as `kind`, which the API has never sent or accepted: reads left it
   * undefined so ChannelList's `kind === "text"` filter matched nothing and
   * the channel list rendered empty, and writes sent `{name, kind}` which the
   * API rejected with a 422 the UI swallowed into console.error.
   */
  channel_type: string;
  server_id: string | null;
  topic: string | null;
  is_e2ee: boolean;
  recipients?: { id: string; username: string; avatar: string | null }[];
}

export interface NxMessage {
  id: string;
  content: string;
  author_id: string;
  author_username: string | null;
  author_avatar: string | null;
  channel_id: string;
  created_at: string;
  edited_at: string | null;
  attachments: Attachment[];
  reactions: Reaction[];
  reply_to: string | null;
}

export interface Attachment {
  id: string;
  filename: string;
  content_type: string;
  size: number;
  url: string | null;
}

export interface Reaction {
  emoji: string;
  count: number;
  me: boolean;
}

export interface NxUser {
  id: string;
  username: string;
  display_name: string | null;
  avatar: string | null;
  presence: "online" | "idle" | "dnd" | "invisible" | "offline";
  status: string | null;
}

export type GatewayStatus = "offline" | "connecting" | "online";

// ── API helpers ───────────────────────────────────────────────────────────────

export function apiBase(_session: Session | null): string {
  // Deliberately ignores session.serverUrl. A session persisted before the SSO
  // cutover can carry an origin that is no longer right, and honouring it would
  // send authenticated calls somewhere that cannot authenticate them.
  return `${currentServerOrigin()}/api/v1`;
}

export function authHeaders(_session: Session | null): HeadersInit {
  // No Authorization header. The server stopped accepting locally-minted JWTs
  // when its login was deleted; identity now arrives as a signed header the
  // ecosystem proxy adds, which the browser cannot see or forge.
  return { "Content-Type": "application/json" };
}

/**
 * Ask the server who we are.
 *
 * Replaces the login form. The request carries no credential — if the proxy
 * let it through, it already attached a verified identity, so a 200 here *is*
 * the sign-in. A 401 means the gate will redirect on the next navigation.
 */
/**
 * Why the last bootstrap did not produce a session.
 *
 * Kept because "Not signed in" with no explanation is undiagnosable from the
 * outside: the server can be returning 200 to curl while the browser sees
 * something else entirely, and without this there is no way to tell which.
 */
export let lastBootstrapReason: string | null = null;

export async function bootstrapSession(): Promise<Session | null> {
  const origin = currentServerOrigin();
  lastBootstrapReason = null;
  try {
    const res = await fetch(`${origin}/api/v1/users/@me`, {
      headers: { "Content-Type": "application/json" },
      credentials: "include",
    });
    if (!res.ok) {
      lastBootstrapReason = `${origin}/api/v1/users/@me returned ${res.status}`;
      return null;
    }
    const body = await res.json();
    const user = body.user ?? body;
    return {
      accessToken: "",
      refreshToken: "",
      username: user.username,
      userId: user.id,
      avatar: user.avatar ?? null,
      serverUrl: origin,
      displayName: user.display_name ?? null,
      bio: user.bio ?? null,
      status: user.status ?? null,
    };
  } catch (err) {
    // A redirect to the sign-in host counts as a network error here, because
    // the page's own CSP (connect-src 'self') refuses to follow it.
    lastBootstrapReason = `request to ${origin} failed: ${
      err instanceof Error ? err.message : String(err)
    }`;
    return null;
  }
}

async function apiFetch<T>(
  session: Session,
  path: string,
  init: RequestInit = {}
): Promise<T> {
  const res = await fetch(`${apiBase(session)}${path}`, {
    ...init,
    headers: { ...authHeaders(session), ...(init.headers ?? {}) },
  });
  if (!res.ok) {
    const text = await res.text().catch(() => String(res.status));
    throw new Error(text || `HTTP ${res.status}`);
  }
  return res.json() as Promise<T>;
}

// ── Store ─────────────────────────────────────────────────────────────────────

interface Store {
  // Auth
  session: Session | null;
  setSession: (s: Session | null) => void;

  // Server
  serverUrl: string;
  setServerUrl: (url: string) => void;

  // Navigation
  activeServerId: string | null;
  activeChannelId: string | null;
  setActiveServer: (id: string | null) => void;
  setActiveChannel: (id: string | null) => void;

  // Data
  servers: NxServer[];
  channels: NxChannel[];
  dmChannels: NxChannel[];
  messages: Record<string, NxMessage[]>;
  unread: Record<string, boolean>;
  typing: Record<string, string[]>;
  onlineUsers: Record<string, NxUser["presence"]>;
  gatewayStatus: GatewayStatus;
  setGatewayStatus: (s: GatewayStatus) => void;

  // Loaders
  loadServers: () => Promise<void>;
  loadChannels: (serverId: string) => Promise<void>;
  loadDms: () => Promise<void>;
  loadMessages: (channelId: string) => Promise<void>;
  loadMoreMessages: (channelId: string) => Promise<void>;

  // Actions
  sendMessage: (channelId: string, content: string, replyTo?: string) => Promise<void>;
  addReaction: (channelId: string, messageId: string, emoji: string) => Promise<void>;
  deleteMessage: (channelId: string, messageId: string) => Promise<void>;
  createServer: (name: string) => Promise<NxServer>;
  createChannel: (serverId: string, name: string, kind: string) => Promise<NxChannel>;

  // Gateway events (called by the gateway hook)
  onMessageCreate: (msg: NxMessage) => void;
  onMessageDelete: (channelId: string, messageId: string) => void;
  onPresenceUpdate: (userId: string, presence: NxUser["presence"]) => void;
  onTypingStart: (channelId: string, username: string) => void;
  markRead: (channelId: string) => void;

  // Profile / settings actions
  updateProfile: (fields: { display_name?: string; bio?: string; status?: string; avatar?: string }) => Promise<void>;
  changePassword: (currentPw: string, newPw: string) => Promise<void>;
  deleteAccount: (password: string) => Promise<void>;
  cancelAccountDeletion: () => Promise<void>;
  joinServer: (inviteCode: string) => Promise<NxServer>;
  createDm: (userId: string) => Promise<NxChannel>;
  sendTyping: (channelId: string) => Promise<void>;
}

export const useStore = create<Store>()(
  persist(
    (set, get) => ({
      session: null,
      setSession: (session) => set({ session }),
      serverUrl: DEFAULT_SERVER_URL,
      setServerUrl: (serverUrl) => {
        const normalized = normalizeServerUrl(serverUrl);
        if (!normalized) {
          throw new Error("Server URL is required");
        }
        localStorage.setItem("nexus_server_url", normalized);
        set({ serverUrl: normalized });
      },

      activeServerId: null,
      activeChannelId: null,
      setActiveServer: (id) => set({ activeServerId: id, activeChannelId: null }),
      setActiveChannel: (id) => {
        set({ activeChannelId: id });
        if (id) get().markRead(id);
      },

      servers: [],
      channels: [],
      dmChannels: [],
      messages: {},
      unread: {},
      typing: {},
      onlineUsers: {},
      gatewayStatus: "offline",
      setGatewayStatus: (gatewayStatus) => set({ gatewayStatus }),

      // ── Loaders ──────────────────────────────────────────────────────────

      loadServers: async () => {
        const s = get().session;
        if (!s) return;
        try {
          const data = await apiFetch<NxServer[]>(s, "/servers");
          set({ servers: Array.isArray(data) ? data : [] });
        } catch (e) { console.error("loadServers", e); }
      },

      loadChannels: async (serverId) => {
        const s = get().session;
        if (!s) return;
        try {
          const data = await apiFetch<NxChannel[]>(s, `/servers/${serverId}/channels`);
          set({ channels: Array.isArray(data) ? data : [] });
        } catch (e) { console.error("loadChannels", e); }
      },

      loadDms: async () => {
        const s = get().session;
        if (!s) return;
        try {
          const data = await apiFetch<NxChannel[]>(s, "/users/@me/channels");
          set({ dmChannels: Array.isArray(data) ? data : [] });
        } catch (e) { console.error("loadDms", e); }
      },

      loadMessages: async (channelId) => {
        const s = get().session;
        if (!s) return;
        try {
          const data = await apiFetch<NxMessage[]>(s, `/channels/${channelId}/messages?limit=50`);
          set((st) => ({
            messages: { ...st.messages, [channelId]: orderMessages(Array.isArray(data) ? data : []) },
          }));
        } catch (e) { console.error("loadMessages", e); }
      },

      loadMoreMessages: async (channelId) => {
        const s = get().session;
        if (!s) return;
        const existing = get().messages[channelId] ?? [];
        const oldest = existing[0];
        if (!oldest) return;
        try {
          const data = await apiFetch<NxMessage[]>(s, `/channels/${channelId}/messages?limit=50&before=${oldest.id}`);
          set((st) => ({
            messages: { ...st.messages, [channelId]: orderMessages([...(Array.isArray(data) ? data : []), ...existing]) },
          }));
        } catch (e) { console.error("loadMoreMessages", e); }
      },

      // ── Actions ────────────────────────────────────────────────────────

      sendMessage: async (channelId, content, replyTo) => {
        const s = get().session;
        if (!s) return;
        const optimistic: NxMessage = {
          id: `temp-${Date.now()}`,
          content,
          author_id: s.userId,
          author_username: s.username,
          author_avatar: s.avatar,
          channel_id: channelId,
          created_at: new Date().toISOString(),
          edited_at: null,
          attachments: [],
          reactions: [],
          reply_to: replyTo ?? null,
        };
        set((st) => ({
          messages: { ...st.messages, [channelId]: orderMessages([...(st.messages[channelId] ?? []), optimistic]) },
        }));
        try {
          const sent = await apiFetch<NxMessage>(s, `/channels/${channelId}/messages`, {
            method: "POST",
            body: JSON.stringify({ content, reference_message_id: replyTo ?? undefined }),
          });
          set((st) => {
            const next = reconcileSent(st.messages[channelId] ?? [], optimistic.id, sent);
            return { messages: { ...st.messages, [channelId]: orderMessages(next) } };
          });
        } catch (e) {
          set((st) => ({
            messages: {
              ...st.messages,
              [channelId]: (st.messages[channelId] ?? []).filter((m) => m.id !== optimistic.id),
            },
          }));
          throw e;
        }
      },

      addReaction: async (channelId, messageId, emoji) => {
        const s = get().session;
        if (!s) return;
        await apiFetch(s, `/channels/${channelId}/messages/${messageId}/reactions/${encodeURIComponent(emoji)}/@me`, {
          method: "PUT",
        });
        set((st) => ({
          messages: {
            ...st.messages,
            [channelId]: updateReactionState(st.messages[channelId] ?? [], messageId, emoji, +1),
          },
        }));
      },

      deleteMessage: async (channelId, messageId) => {
        const s = get().session;
        if (!s) return;
        await apiFetch(s, `/channels/${channelId}/messages/${messageId}`, { method: "DELETE" });
        set((st) => ({
          messages: {
            ...st.messages,
            [channelId]: (st.messages[channelId] ?? []).filter((m) => m.id !== messageId),
          },
        }));
      },

      createServer: async (name) => {
        const s = get().session;
        if (!s) throw new Error("Not authenticated");
        const srv = await apiFetch<NxServer>(s, "/servers", {
          method: "POST",
          body: JSON.stringify({ name }),
        });
        set((st) => ({ servers: [srv, ...st.servers] }));
        return srv;
      },

      createChannel: async (serverId, name, kind = "text") => {
        const s = get().session;
        if (!s) throw new Error("Not authenticated");
        const ch = await apiFetch<NxChannel>(s, `/servers/${serverId}/channels`, {
          method: "POST",
          // `channel_type`, not `kind` — the API answers 422 "missing field
          // `channel_type`" to the latter, which is what made the new-channel
          // button appear to do nothing at all.
          body: JSON.stringify({ name, channel_type: kind }),
        });
        set((st) => ({ channels: [ch, ...st.channels] }));
        return ch;
      },

      // ── Profile / settings actions ────────────────────────────────────────

      updateProfile: async (fields) => {
        const { session } = get();
        if (!session) return;
        await apiFetch(session, "/users/@me", { method: "PATCH", body: JSON.stringify(fields) });
      },

      changePassword: async (currentPw, newPw) => {
        const { session } = get();
        if (!session) return;
        await apiFetch(session, "/users/@me/change-password", {
          method: "POST",
          body: JSON.stringify({ current_password: currentPw, new_password: newPw }),
        });
      },

      deleteAccount: async (password) => {
        const { session } = get();
        if (!session) return;
        await apiFetch(session, "/users/@me", {
          method: "DELETE",
          body: JSON.stringify({ password }),
        });
      },

      cancelAccountDeletion: async () => {
        const { session } = get();
        if (!session) return;
        await apiFetch(session, "/users/@me/cancel-deletion", {
          method: "POST",
        });
      },

      joinServer: async (inviteCode) => {
        const { session } = get();
        if (!session) throw new Error('Not authenticated');
        const res = await apiFetch<{ server: NxServer }>(session, `/invites/${inviteCode}/join`, { method: 'POST' });
        const srv = res.server ?? (res as unknown as NxServer);
        set((st) => ({ servers: st.servers.some(s => s.id === srv.id) ? st.servers : [...st.servers, srv] }));
        return srv;
      },

      createDm: async (userId) => {
        const { session } = get();
        if (!session) throw new Error('Not authenticated');
        const dm = await apiFetch<NxChannel>(session, '/channels/@me/dms', {
          method: 'POST',
          body: JSON.stringify({ recipient_id: userId }),
        });
        set((st) => ({ dmChannels: st.dmChannels.some(d => d.id === dm.id) ? st.dmChannels : [dm, ...st.dmChannels] }));
        return dm;
      },

      sendTyping: async (channelId) => {
        const { session } = get();
        if (!session) return;
        await apiFetch(session, `/channels/${channelId}/typing`, { method: 'POST' }).catch(() => {});
      },

      // ── Gateway event handlers ─────────────────────────────────────────────

      onMessageCreate: (msg) => {
        set((st) => {
          const prev = st.messages[msg.channel_id] ?? [];
          if (prev.some((m) => m.id === msg.id)) return {};
          const updated = orderMessages([...prev, msg]);
          const isActive = st.activeChannelId === msg.channel_id;
          return {
            messages: { ...st.messages, [msg.channel_id]: updated },
            unread: isActive ? st.unread : { ...st.unread, [msg.channel_id]: true },
          };
        });
      },

      onMessageDelete: (channelId, messageId) => {
        set((st) => ({
          messages: {
            ...st.messages,
            [channelId]: (st.messages[channelId] ?? []).filter((m) => m.id !== messageId),
          },
        }));
      },

      onPresenceUpdate: (userId, presence) => {
        set((st) => ({ onlineUsers: { ...st.onlineUsers, [userId]: presence } }));
      },

      onTypingStart: (channelId, username) => {
        set((st) => {
          const cur = st.typing[channelId] ?? [];
          if (cur.includes(username)) return {};
          const next = [...cur, username];
          setTimeout(() => {
            set((s2) => ({
              typing: {
                ...s2.typing,
                [channelId]: (s2.typing[channelId] ?? []).filter((u) => u !== username),
              },
            }));
          }, 5000);
          return { typing: { ...st.typing, [channelId]: next } };
        });
      },

      markRead: (channelId) => {
        set((st) => ({ unread: { ...st.unread, [channelId]: false } }));
      },
    }),
    {
      name: "nexus-web-session",
      partialize: (s) => ({ session: s.session, serverUrl: s.serverUrl }),
    }
  )
);
