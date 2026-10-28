/**
 * store.ts - Zustand-compatible React Native store with AsyncStorage persistence
 */
import { useSyncExternalStore } from "react";
import AsyncStorage from "@react-native-async-storage/async-storage";
import { api, getDefaultServerOrigin, normalizeApiBaseUrl } from "./api";
import type { User, Server, Channel, Message, DmChannel, Relationship, ServerMember } from "./api";

export { api };

const STORAGE_KEY_SESSION = "nexus_session";
const STORAGE_KEY_SETTINGS = "nexus_settings";
const STORAGE_KEY_SERVER_URL = "nexus_server_url";

export interface Session {
  user: User;
  /** Ecosystem session token (`nxs_…`). One account, every app. */
  token: string;
}

type Listener = () => void;

function initialServerOrigin(): string {
  return getDefaultServerOrigin() || "";
}

function sortMessages(messages: Message[]): Message[] {
  return [...messages].sort((a, b) => new Date(a.createdAt).getTime() - new Date(b.createdAt).getTime());
}

function updateReactionState(
  messages: Message[],
  messageId: string,
  emoji: string,
  delta: number,
  mine: boolean,
): Message[] {
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
        updated[idx] = {
          ...updated[idx],
          count: nextCount,
          me: delta > 0 ? mine || updated[idx].me : (mine ? false : updated[idx].me),
        };
      }
      return { ...m, reactions: updated };
    }
    if (delta > 0) {
      return { ...m, reactions: [...reactions, { emoji, count: 1, me: mine }] };
    }
    return m;
  });
}

function mergeMessageUpdate(messages: Message[], update: Partial<Message> & { id: string; channelId: string }): Message[] {
  const idx = messages.findIndex((m) => m.id === update.id);
  if (idx < 0) return messages;
  const current = messages[idx];
  const next: Message = {
    ...current,
    ...update,
    reactions: update.reactions ?? current.reactions,
    embeds: update.embeds ?? current.embeds,
    attachments: update.attachments ?? current.attachments,
  };
  const nextMessages = [...messages];
  nextMessages[idx] = next;
  return nextMessages;
}

function patchSessionUser(session: Session | null, patch: Partial<User>): Session | null {
  if (!session) return session;
  return { ...session, user: { ...session.user, ...patch } };
}

class Store {
  session: Session | null = null;
  serverUrl = initialServerOrigin();
  servers: Server[] = [];
  activeServerId: string | null = null;
  channels: Channel[] = [];
  activeChannelId: string | null = null;
  messages: Record<string, Message[]> = {};
  dmChannels: DmChannel[] = [];
  relationships: Relationship[] = [];
  members: Record<string, ServerMember[]> = {};
  typingUsers: Record<string, string[]> = {};
  onlineUsers: Record<string, User["presence"]> = {};
  unreadChannels: Record<string, boolean> = {};
  voiceJoinedChannelId: string | null = null;
  voiceMuted = false;
  voiceDeafened = false;
  experienceMode: "full" | "messaging" = "full";
  settings = {
    theme: "dark",
    fontSize: 16,
    compactMode: false,
    reducedMotion: false,
    notificationsEnabled: true,
    mentionsOnly: false,
    soundsEnabled: true,
    vibrationEnabled: true,
    readReceipts: true,
    sharePresence: true,
    dmPermission: "everyone" as "everyone" | "friends" | "nobody",
  };
  updateSettings = (patch: Partial<Store["settings"]>) => {
    this.settings = { ...this.settings, ...patch };
    this._persistSettings();
    this.emit();
  };
  loading = false;
  error: string | null = null;

  get api() { return api; }

  private _rev = 0;
  private listeners = new Set<Listener>();
  subscribe = (fn: Listener) => { this.listeners.add(fn); return () => { this.listeners.delete(fn); }; };
  addListener = (fn: Listener) => { this.listeners.add(fn); return () => { this.listeners.delete(fn); }; };
  private emit() { this._rev++; this.listeners.forEach(fn => fn()); }
  notify() { this.emit(); }
  getSnapshot = () => this._rev;

  get hasSession() { return !!this.session; }
  get isAuthenticated() { return !!this.session; }

  // ── Persistence ──────────────────────────────────────────────────────────────

  /** Load session + settings from AsyncStorage. Call once at app startup. */
  async hydrate(): Promise<void> {
    try {
      const [sessionJson, settingsJson, serverUrlJson] = await Promise.all([
        AsyncStorage.getItem(STORAGE_KEY_SESSION),
        AsyncStorage.getItem(STORAGE_KEY_SETTINGS),
        AsyncStorage.getItem(STORAGE_KEY_SERVER_URL),
      ]);
      if (serverUrlJson) {
        const savedServerUrl = JSON.parse(serverUrlJson);
        if (typeof savedServerUrl === "string" && savedServerUrl.trim()) {
          this.serverUrl = savedServerUrl.trim().replace(/\/$/, "");
        }
      }
      api.setBaseUrl(this.serverUrl);
      if (sessionJson) {
        const saved = JSON.parse(sessionJson) as Partial<Session> & {
          accessToken?: string;
        };
        // A session stored by a build from before the SSO cutover holds an
        // accessToken this ecosystem no longer issues or accepts. Discard it
        // rather than restoring a login that cannot work — the user signs in
        // once more and is then on the new scheme for good.
        if (saved.token && saved.user) {
          api.setSessionToken(saved.token);
          this.session = { user: saved.user, token: saved.token };
        } else {
          await AsyncStorage.removeItem(STORAGE_KEY_SESSION);
        }
      }
      if (settingsJson) {
        const saved = JSON.parse(settingsJson);
        this.settings = { ...this.settings, ...saved };
      }
    } catch (e) {
      console.error("store hydrate error", e);
    }
  }

  async setServerUrl(url: string) {
    const normalized = normalizeApiBaseUrl(url).replace(/\/api\/v1$/, "");
    if (!normalized) {
      throw new Error("Server URL is required");
    }
    this.serverUrl = normalized;
    api.setBaseUrl(this.serverUrl);
    await AsyncStorage.setItem(STORAGE_KEY_SERVER_URL, JSON.stringify(this.serverUrl)).catch(
      e => console.error("persistServerUrl error", e),
    );
    this.emit();
  }

  private async _persistSession(): Promise<void> {
    if (this.session) {
      await AsyncStorage.setItem(STORAGE_KEY_SESSION, JSON.stringify(this.session)).catch(
        e => console.error("persistSession error", e),
      );
    } else {
      await AsyncStorage.removeItem(STORAGE_KEY_SESSION).catch(
        e => console.error("clearSession error", e),
      );
    }
  }

  private _persistSettings(): void {
    AsyncStorage.setItem(STORAGE_KEY_SETTINGS, JSON.stringify(this.settings)).catch(
      e => console.error("persistSettings error", e),
    );
  }

  // ── Auth ─────────────────────────────────────────────────────────────────────

  async login(username: string, password: string) {
    this.loading = true; this.error = null; this.emit();
    try {
      const r = await api.login(username, password);
      this.session = { user: r.user, token: r.token };
      await this._persistSession();
      await this.loadInitialData();
    } catch (e: unknown) {
      this.error = e instanceof Error ? e.message : "Login failed";
      throw e;
    } finally {
      this.loading = false; this.emit();
    }
  }

  /**
   * Registration is not in this app.
   *
   * Access is invite-only: a request has to be approved by an operator before
   * an account exists, and that flow already exists on the web. Reimplementing
   * half of it here would mean a second place to keep correct.
   */
  requestAccessUrl(): string {
    return api.requestAccessUrl();
  }

  logout() {
    api.logout();
    this.session = null;
    this.servers = []; this.activeServerId = null;
    this.channels = []; this.activeChannelId = null;
    this.messages = {}; this.dmChannels = [];
    this.relationships = []; this.members = {};
    this.typingUsers = {};
    this.unreadChannels = {};
    this.voiceJoinedChannelId = null;
    this.voiceMuted = false;
    this.voiceDeafened = false;
    this._persistSession();
    this.emit();
  }

  async loadInitialData() {
    try {
      const [servers, dms, relationships] = await Promise.all([
        api.getServers(), api.getDMs(), api.getRelationships(),
      ]);
      this.servers = servers; this.dmChannels = dms; this.relationships = relationships;
      this.emit();
    } catch (e) { console.error("loadInitialData error", e); }
  }

  async sendTyping(channelId: string) {
    try {
      await api.sendTyping(channelId);
    } catch (e) {
      console.error("sendTyping error", e);
    }
  }

  async selectServer(serverId: string) {
    this.activeServerId = serverId; this.channels = []; this.activeChannelId = null;
    this.emit();
    try {
      const channels = await api.getChannels(serverId);
      this.channels = channels;
      const first = channels.find(c => c.kind === "text" || c.kind === "announcement" || c.kind === "forum");
      if (first) await this.selectChannel(first.id);
      this.emit();
    } catch (e) { console.error("selectServer error", e); }
  }

  async createServer(name: string) {
    const server = await api.createServer(name);
    this.servers = [...this.servers, server]; this.emit();
    return server;
  }

  async loadMembers(serverId: string) {
    try {
      const members = await api.getMembers(serverId);
      this.members = { ...this.members, [serverId]: members }; this.emit();
    } catch (e) { console.error("loadMembers error", e); }
  }

  async selectChannel(channelId: string) {
    this.activeChannelId = channelId;
    const unread = { ...this.unreadChannels };
    delete unread[channelId];
    this.unreadChannels = unread;
    this.emit();
    await this.loadMessages(channelId);
  }

  async loadMessages(channelId: string, before?: string) {
    try {
      const msgs = await api.getMessages(channelId, before);
      const existing = this.messages[channelId] ?? [];
      const merged = before ? [...msgs, ...existing] : msgs;
      this.messages = { ...this.messages, [channelId]: sortMessages(merged) };
      this.emit();
    } catch (e) { console.error("loadMessages error", e); }
  }

  updateSessionUser(patch: Partial<User>) {
    this.session = patchSessionUser(this.session, patch);
    void this._persistSession();
    this.emit();
  }

  updatePresence(presence: User["presence"]) {
    this.updateSessionUser({ presence });
  }

  onMessageUpdate(update: Message) {
    const existing = this.messages[update.channelId] ?? [];
    const next = existing.some(m => m.id === update.id)
      ? mergeMessageUpdate(existing, update)
      : sortMessages([...existing, update]);
    this.messages = { ...this.messages, [update.channelId]: next };
    this.emit();
  }

  onPresenceUpdate(userId: string, presence: User["presence"]) {
    this.onlineUsers = { ...this.onlineUsers, [userId]: presence };
    this.emit();
  }

  async loadOlderMessages() {
    if (!this.activeChannelId) return;
    const msgs = this.messages[this.activeChannelId];
    if (!msgs?.length) return;
    await this.loadMessages(this.activeChannelId, msgs[0].id);
  }

  async sendMessage(content: string, replyTo?: string) {
    if (!this.activeChannelId || !this.session) return;
    const optimistic: Message = {
      id: "temp-" + Date.now(),
      channelId: this.activeChannelId,
      authorId: this.session.user.id,
      authorUsername: this.session.user.username,
      authorAvatar: this.session.user.avatar,
      content,
      createdAt: new Date().toISOString(),
      replyTo,
    };
    this.onMessageCreate(optimistic);
    try {
      const sent = await api.sendMessage(this.activeChannelId, content, replyTo);
      const current = this.messages[this.activeChannelId] ?? [];
      const updated = current.map(m => m.id === optimistic.id ? sent : m);
      this.messages = { ...this.messages, [this.activeChannelId]: sortMessages(updated) };
      this.emit();
    } catch {
      const current = this.messages[this.activeChannelId] ?? [];
      this.messages = {
        ...this.messages,
        [this.activeChannelId]: current.filter(m => m.id !== optimistic.id),
      };
      this.emit();
    }
  }

  onMessageCreate(msg: Message) {
    const existing = this.messages[msg.channelId] ?? [];
    if (existing.some(m => m.id === msg.id)) return;
    const unread = { ...this.unreadChannels };
    if (this.activeChannelId !== msg.channelId) unread[msg.channelId] = true;
    this.unreadChannels = unread;
    this.messages = { ...this.messages, [msg.channelId]: sortMessages([...existing, msg]) };
    this.emit();
  }

  onMessageDelete(channelId: string, messageId: string) {
    const existing = this.messages[channelId] ?? [];
    this.messages = { ...this.messages, [channelId]: existing.filter(m => m.id !== messageId) };
    this.emit();
  }

  onTypingStart(channelId: string, username: string) {
    const current = this.typingUsers[channelId] ?? [];
    if (!current.includes(username)) {
      this.typingUsers = { ...this.typingUsers, [channelId]: [...current, username] };
      this.emit();
    }
    setTimeout(() => {
      const users = this.typingUsers[channelId] ?? [];
      if (users.includes(username)) {
        this.typingUsers = { ...this.typingUsers, [channelId]: users.filter(u => u !== username) };
        this.emit();
      }
    }, 5000);
  }

  onReactionUpdate(channelId: string, messageId: string, emoji: string, delta: number, mine: boolean) {
    const existing = this.messages[channelId] ?? [];
    this.messages = {
      ...this.messages,
      [channelId]: updateReactionState(existing, messageId, emoji, delta, mine),
    };
    this.emit();
  }

  appendMessage(msg: Message) {
    this.onMessageCreate(msg);
  }

  prependOlderMessages(channelId: string, msgs: Message[]) {
    const existing = this.messages[channelId] ?? [];
    this.messages = { ...this.messages, [channelId]: sortMessages([...msgs, ...existing]) };
    this.emit();
  }

  async addReaction(channelId: string, messageId: string, emoji: string) {
    await api.addReaction(channelId, messageId, emoji);
    this.onReactionUpdate(channelId, messageId, emoji, 1, true);
  }

  async removeReaction(channelId: string, messageId: string, emoji: string) {
    await api.removeReaction(channelId, messageId, emoji);
    this.onReactionUpdate(channelId, messageId, emoji, -1, true);
  }

  async createChannel(serverId: string, name: string, kind: Channel["kind"] = "text") {
    const channel = await api.createChannel(serverId, { name, kind });
    this.channels = [...this.channels, channel]; this.emit();
    return channel;
  }

  async selectDM(channelId: string) {
    this.activeChannelId = channelId;
    this.activeServerId = null;
    const unread = { ...this.unreadChannels };
    delete unread[channelId];
    this.unreadChannels = unread;
    this.emit();
    await this.loadMessages(channelId);
  }

  async createDM(recipientId: string) {
    const dm = await api.createDM(recipientId);
    this.dmChannels = [...this.dmChannels.filter(d => d.id !== dm.id), dm]; this.emit();
    return dm;
  }

  async joinVoice(channelId: string) {
    this.voiceJoinedChannelId = channelId;
    this.emit();
    try {
      await api.joinVoice(channelId);
    } catch (e) { console.error("joinVoice error", e); }
  }

  async leaveVoice() {
    if (!this.voiceJoinedChannelId) return;
    const channelId = this.voiceJoinedChannelId;
    this.voiceJoinedChannelId = null;
    this.voiceMuted = false;
    this.voiceDeafened = false;
    this.emit();
    try {
      await api.leaveVoice(channelId);
    } catch (e) { console.error("leaveVoice error", e); }
  }

  async toggleMute() {
    this.voiceMuted = !this.voiceMuted;
    this.emit();
    try {
      await api.toggleMute();
    } catch (e) { console.error("toggleMute error", e); }
  }

  async toggleDeafen() {
    this.voiceDeafened = !this.voiceDeafened;
    this.emit();
    try {
      await api.toggleDeafen();
    } catch (e) { console.error("toggleDeafen error", e); }
  }
}

export const store = new Store();

/**
 * useStore() — subscribe a React component to the store.
 * Call once per component; returns the store instance and re-renders
 * whenever store.emit() fires.
 */
export function useStore(): Store {
  useSyncExternalStore(store.subscribe, store.getSnapshot, store.getSnapshot);
  return store;
}
