/**
 * api.ts - REST API client for Nexus Mobile
 */

export function normalizeBaseOrigin(url: string): string {
  const trimmed = url.trim().replace(/\/+$/, "");
  if (!trimmed) {
    throw new Error("Server URL is required");
  }
  return trimmed.endsWith("/api/v1") ? trimmed.replace(/\/api\/v1$/, "") : trimmed;
}

function defaultApiOrigin(): string {
  const env = process.env?.EXPO_PUBLIC_NEXUS_API_BASE_URL?.trim();
  if (env) {
    return normalizeBaseOrigin(env);
  }
  return "";
}

export function getDefaultServerOrigin(): string {
  return defaultApiOrigin();
}

export function getServerUrlPlaceholder(): string {
  return "https://your-nexus-server.com";
}

export function normalizeApiBaseUrl(url: string): string {
  const origin = normalizeBaseOrigin(url);
  return `${origin}/api/v1`;
}

export function apiBaseToOrigin(baseUrl: string): string {
  return baseUrl.replace(/\/api\/v1$/, "");
}

export const DEFAULT_API_BASE = (() => {
  const origin = defaultApiOrigin();
  return origin ? normalizeApiBaseUrl(origin) : "";
})();

// ── Types ─────────────────────────────────────────────────────────────────────

export interface AuthTokens {
  accessToken: string;
  refreshToken: string;
  expiresIn: number;
}

export interface User {
  id: string;
  username: string;
  displayName?: string;
  avatar?: string;
  email?: string;
  presence?: string;
  status?: string;
  bio?: string;
  createdAt: string;
  totpEnabled?: boolean;
}

export interface Server {
  id: string;
  name: string;
  icon?: string;
  ownerId: string;
  memberCount?: number;
  description?: string;
  isPublic?: boolean;
}

export interface Channel {
  id: string;
  serverId?: string;
  name: string;
  kind: "text" | "voice" | "announcement" | "forum" | "stage" | "dm" | "group_dm";
  topic?: string;
  isE2ee?: boolean;
  disappearAfterSeconds?: number;
  isStream?: boolean;
}

export interface Message {
  id: string;
  channelId: string;
  authorId: string;
  authorUsername: string;
  authorAvatar?: string;
  content: string;
  createdAt: string;
  editedAt?: string;
  replyTo?: string;
  threadId?: string;
  attachments?: Attachment[];
  reactions?: Reaction[];
  embeds?: Embed[];
  stickerIds?: string[];
}

export interface Attachment {
  id: string;
  filename: string;
  url: string;
  size: number;
  contentType?: string;
}

export interface Reaction {
  emoji: string;
  count: number;
  me: boolean;
}

export interface Embed {
  type?: string;
  title?: string;
  description?: string;
  url?: string;
  thumbnail?: { url: string };
  image?: { url: string };
  color?: number;
}

export interface Relationship {
  id: string;
  direction: "incoming" | "outgoing";
  status: "pending" | "accepted" | "blocked" | "denied";
  user: { id: string; username: string; displayName?: string; avatar?: string };
}

export interface DmChannel {
  id: string;
  channelType: "dm" | "group_dm";
  name?: string;
  recipients: { id: string; username: string; displayName?: string; avatar?: string }[];
  lastMessageId?: string;
}

export interface ServerMember {
  userId: string;
  username: string;
  displayName?: string;
  avatar?: string;
  nickname?: string;
  presence: "online" | "idle" | "do_not_disturb" | "invisible" | "offline";
  roles: string[];
}

// ── API client ────────────────────────────────────────────────────────────────

/**
 * Where accounts live. Not the chat server — the ecosystem identity service.
 *
 * A native app is not behind the ecosystem proxy and has no browser cookie
 * jar, so it does what a browser does, by hand: sign in to Auth, hold the
 * session token, and present it as the `nexus_session` cookie on every request
 * to an app. The proxy exchanges that cookie for a short-lived signed identity
 * per host, exactly as it does for a browser, and the app never sees the
 * session at all.
 */
export const AUTH_BASE =
  process.env.EXPO_PUBLIC_NEXUS_AUTH_URL ?? "https://auth.tnhc.dev";

class NexusApi {
  private baseUrl: string;
  /** Ecosystem session token (`nxs_…`), presented as a cookie. */
  private _sessionToken: string | null = null;

  constructor(baseUrl: string = DEFAULT_API_BASE) {
    this.baseUrl = baseUrl;
  }

  // ── Config ──────────────────────────────────────────────────────────────────

  getBaseUrl(): string { return this.baseUrl; }
  setBaseUrl(url: string): void { this.baseUrl = normalizeApiBaseUrl(url); }

  /** Adopt an ecosystem session token, e.g. one restored from storage. */
  setSessionToken(token: string | null) {
    this._sessionToken = token;
  }

  getSessionToken(): string | null { return this._sessionToken; }

  clearTokens() {
    this._sessionToken = null;
  }

  get hasToken(): boolean { return !!this._sessionToken; }

  // ── Core ────────────────────────────────────────────────────────────────────

  private headers(): Record<string, string> {
    const h: Record<string, string> = { "Content-Type": "application/json" };
    // A cookie, not an Authorization header. The app servers stopped accepting
    // locally-minted tokens when their logins were deleted; what they trust is
    // the identity header the ecosystem proxy adds, and the proxy mints that
    // from this cookie.
    if (this._sessionToken) h["Cookie"] = `nexus_session=${this._sessionToken}`;
    return h;
  }

  async request<T>(method: string, path: string, body?: unknown): Promise<T> {
    const baseUrl = this.baseUrl.trim();
    if (!baseUrl) {
      throw new Error("Server URL is not configured");
    }
    const opts: RequestInit = { method, headers: this.headers() };
    if (body !== undefined) opts.body = JSON.stringify(body);
    const res = await fetch(baseUrl + path, opts);
    if (res.status === 401) {
      // Nothing to refresh: ecosystem sessions are not renewed by this client,
      // they are re-established by signing in again. Dropping the token here
      // is what makes the UI fall back to the sign-in screen.
      this.clearTokens();
      throw new Error("Unauthorized");
    }
    if (!res.ok) {
      const txt = await res.text();
      throw new Error(res.status + ": " + txt);
    }
    if (res.status === 204) return undefined as unknown as T;
    return res.json() as Promise<T>;
  }

  // ── Auth ────────────────────────────────────────────────────────────────────

  /**
   * Sign in to the ecosystem.
   *
   * Goes to Auth, not to the app server: there is one account for every app and
   * only Auth holds it. Returns the session token so the caller can persist it.
   */
  async login(username: string, password: string): Promise<{ token: string; user: User }> {
    const res = await fetch(`${AUTH_BASE}/api/v1/auth/login`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ username, password }),
    });
    if (!res.ok) {
      throw new Error(res.status === 401 ? "Incorrect username or password." : `HTTP ${res.status}`);
    }
    const body = (await res.json()) as { token?: string; user?: User };
    if (!body.token) throw new Error("Sign-in did not return a session.");
    this.setSessionToken(body.token);
    return { token: body.token, user: body.user as User };
  }

  /**
   * Where to send someone who has no account.
   *
   * Registration is invite-only and deliberately not in this app: it needs an
   * operator to approve the request, so it lives on the web where that flow
   * already exists rather than being half-reimplemented here.
   */
  requestAccessUrl(): string {
    return "https://app.tnhc.dev/request";
  }

  async logout() {
    try { await this.request("POST", "/auth/logout"); } catch { /* ignore */ }
    this.clearTokens();
  }

  // ── Users ───────────────────────────────────────────────────────────────────

  async getMe(): Promise<User> { return this.request("GET", "/users/@me"); }
  async getUser(userId: string): Promise<User> { return this.request("GET", "/users/" + userId); }
  async updateMe(data: Partial<User>): Promise<User> { return this.request("PATCH", "/users/@me", data); }
  async searchUsers(query: string): Promise<User[]> {
    return this.request("GET", "/users/search?q=" + encodeURIComponent(query));
  }

  async changePassword(currentPassword: string, newPassword: string): Promise<void> {
    await this.request("POST", "/users/@me/change-password", {
      current_password: currentPassword,
      new_password: newPassword,
    });
  }

  async deleteAccount(password: string): Promise<void> {
    await this.request("DELETE", "/users/@me", { password });
  }

  async cancelAccountDeletion(): Promise<void> {
    await this.request("POST", "/users/@me/cancel-deletion");
  }

  // ── Sessions ─────────────────────────────────────────────────────────────────

  async getSessions(): Promise<any[]> {
    return this.request("GET", "/auth/sessions");
  }
  async revokeSession(sessionId: string): Promise<void> {
    return this.request("DELETE", "/auth/sessions/" + sessionId);
  }

  // ── E2EE ─────────────────────────────────────────────────────────────────────

  async listDevices(): Promise<any[]> {
    return this.request("GET", "/devices");
  }
  async deleteDevice(deviceId: string): Promise<void> {
    return this.request("DELETE", "/devices/" + deviceId);
  }

  // ── Voice ────────────────────────────────────────────────────────────────────

  async joinVoice(channelId: string): Promise<any> {
    return this.request("POST", "/voice/channels/" + channelId + "/join");
  }
  async leaveVoice(channelId?: string): Promise<void> {
    if (channelId) {
      await this.request("POST", "/voice/channels/" + channelId + "/leave");
    }
  }
  async toggleMute(): Promise<void> {
    await this.request("PATCH", "/voice/state", { muted: true });
  }
  async toggleDeafen(): Promise<void> {
    await this.request("PATCH", "/voice/state", { deafened: true });
  }
  async getVoiceState(channelId: string): Promise<any> {
    return this.request("GET", "/voice/channels/" + channelId);
  }

  // ── Search ────────────────────────────────────────────────────────────────────

  async searchMessages(q: string): Promise<{ messages: Message[] }> {
    return this.request("GET", "/search?q=" + encodeURIComponent(q) + "&type=messages");
  }
  async searchUsersRemote(q: string): Promise<User[]> {
    return this.request("GET", "/search?q=" + encodeURIComponent(q) + "&type=users");
  }

  // ── Directory ─────────────────────────────────────────────────────────────────

  async searchServers(query: string): Promise<Server[]> {
    return this.request("GET", "/directory/servers?q=" + encodeURIComponent(query));
  }

  // ── Threads ───────────────────────────────────────────────────────────────────

  async getThreads(channelId: string): Promise<any[]> {
    return this.request("GET", "/channels/" + channelId + "/threads");
  }
  async getThread(threadId: string): Promise<any> {
    return this.request("GET", "/threads/" + threadId);
  }

  // ── Servers ─────────────────────────────────────────────────────────────────

  async getServers(): Promise<Server[]> { return this.request("GET", "/users/@me/servers"); }
  async getServer(serverId: string): Promise<Server> { return this.request("GET", "/servers/" + serverId); }
  async createServer(name: string, isPublic = false): Promise<Server> {
    return this.request("POST", "/servers", { name, is_public: isPublic });
  }
  async deleteServer(serverId: string) { return this.request("DELETE", "/servers/" + serverId); }
  async leaveServer(serverId: string) { return this.request("DELETE", "/servers/" + serverId + "/members/@me"); }
  async joinServer(inviteCode: string): Promise<Server> {
    return this.request("POST", "/invites/" + inviteCode + "/join");
  }

  // ── Channels ────────────────────────────────────────────────────────────────

  async getChannels(serverId: string): Promise<Channel[]> {
    return this.request("GET", "/servers/" + serverId + "/channels");
  }
  async getChannel(channelId: string): Promise<Channel> { return this.request("GET", "/channels/" + channelId); }
  async createChannel(serverId: string, data: { name: string; kind?: Channel["kind"] }): Promise<Channel> {
    const { kind, ...rest } = data;
    return this.request("POST", "/servers/" + serverId + "/channels", {
      ...rest,
      channel_type: kind ?? "text",
    });
  }
  async updateChannel(channelId: string, data: Partial<Channel>): Promise<Channel> {
    return this.request("PATCH", "/channels/" + channelId, data);
  }
  async deleteChannel(channelId: string) { return this.request("DELETE", "/channels/" + channelId); }

  async sendTyping(channelId: string): Promise<void> {
    this.request("POST", "/channels/" + channelId + "/typing").catch(() => {});
  }

  // ── Messages ─────────────────────────────────────────────────────────────────

  async getMessages(channelId: string, before?: string, limit = 50): Promise<Message[]> {
    let path = "/channels/" + channelId + "/messages?limit=" + limit;
    if (before) path += "&before=" + before;
    return this.request("GET", path);
  }
  async sendMessage(channelId: string, content: string, replyTo?: string, threadId?: string): Promise<Message> {
    return this.request("POST", "/channels/" + channelId + "/messages", {
      content,
      ...(replyTo ? { reply_to: replyTo } : {}),
      ...(threadId ? { thread_id: threadId } : {}),
    });
  }
  async editMessage(messageId: string, content: string): Promise<Message> {
    return this.request("PATCH", "/messages/" + messageId, { content });
  }
  async deleteMessage(messageId: string) { return this.request("DELETE", "/messages/" + messageId); }
  async addReaction(channelId: string, messageId: string, emoji: string) {
    return this.request("PUT",
      "/channels/" + channelId + "/messages/" + messageId + "/reactions/" + encodeURIComponent(emoji));
  }
  async removeReaction(channelId: string, messageId: string, emoji: string) {
    return this.request("DELETE",
      "/channels/" + channelId + "/messages/" + messageId + "/reactions/" + encodeURIComponent(emoji));
  }

  // ── DMs ──────────────────────────────────────────────────────────────────────

  async getDMs(): Promise<DmChannel[]> { return this.request("GET", "/users/@me/channels"); }
  async createDM(recipientId: string): Promise<DmChannel> {
    return this.request("POST", "/users/@me/channels", { recipient_id: recipientId });
  }

  // ── Relationships ─────────────────────────────────────────────────────────────

  async getRelationships(): Promise<Relationship[]> { return this.request("GET", "/users/@me/relationships"); }
  async addFriend(userId: string) { return this.request("POST", "/users/@me/relationships", { user_id: userId, type: 1 }); }
  async blockUser(userId: string) { return this.request("POST", "/users/@me/relationships", { user_id: userId, type: 2 }); }
  async removeRelationship(userId: string) { return this.request("DELETE", "/users/@me/relationships/" + userId); }

  // ── Members ───────────────────────────────────────────────────────────────────

  async getMembers(serverId: string, limit = 100): Promise<ServerMember[]> {
    return this.request("GET", "/servers/" + serverId + "/members?limit=" + limit);
  }

  // ── Invites ───────────────────────────────────────────────────────────────────

  async getInvites(serverId: string) {
    return this.request("GET", "/servers/" + serverId + "/invites");
  }
  async createInvite(serverId: string, maxAge?: number, maxUses?: number) {
    return this.request("POST", "/servers/" + serverId + "/invites", {
      ...(maxAge ? { max_age: maxAge } : {}),
      ...(maxUses ? { max_uses: maxUses } : {}),
    });
  }

  // ── Gateway ───────────────────────────────────────────────────────────────────

  async getGatewayUrl(): Promise<string> {
    const r = await this.request<{ url: string }>("GET", "/gateway");
    return r.url;
  }
}

export const api = new NexusApi();
export { NexusApi };
