// ── nexus-admin API client ────────────────────────────────────────────────────
//
// Typed wrappers for every admin endpoint. Auth is a simple module-level
// singleton stored in localStorage — this app is served only to INSTANCE_ADMIN
// users and never rendered publicly.

// ── Types ─────────────────────────────────────────────────────────────────────

export interface Session {
  accessToken: string;
  username: string;
  userId: string;
  serverUrl: string;
}

export interface HealthCheck {
  status: string;
  version: string;
  uptime_secs: number;
  checks: Record<string, { status: string; latency_ms?: number; error?: string }>;
}

export interface UserStats {
  total: number;
  suspended: number;
  disabled: number;
  bots: number;
  new_24h: number;
  new_7d: number;
}

export interface AdminUser {
  id: string;
  username: string;
  display_name: string | null;
  flags: number;
  is_suspended: boolean;
  is_disabled: boolean;
  is_bot: boolean;
  is_staff: boolean;
  created_at: string;
}

export interface UserList {
  users: AdminUser[];
  total: number;
  limit: number;
  offset: number;
}

export interface AdminServer {
  id: string;
  name: string;
  description: string | null;
  owner_id: string;
  member_count: number;
  created_at: string;
  is_public: boolean;
}

export interface FederationStatus {
  server_name: string;
  software_version: string;
  peer_count: number;
  healthy_peer_count: number;
  pending_inbound_requests: number;
  pending_outbound_requests: number;
  uptime_seconds: number;
}

export interface FederationPeer {
  domain: string;
  trust_level: string;
  is_blocked: boolean;
  is_healthy: boolean;
  last_seen_at: string | null;
  added_at: string;
  software?: string | null;
}

export interface FederationAuditEntry {
  id: string;
  admin_id: string;
  action: string;
  target_domain: string;
  details: Record<string, unknown>;
  created_at: string;
}

export interface InstanceAuditEntry {
  id: string;
  actor_id: string;
  action: string;
  target_type: string | null;
  target_id: string | null;
  changes: Record<string, unknown>;
  reason: string | null;
  created_at: string;
}

export interface ModerationReport {
  id: string;
  reporter_id: string;
  reported_user_id: string | null;
  message_id: string | null;
  reason: string;
  status: string;
  created_at: string;
}

export interface InstanceOverview {
  total_users: number;
  active_users_24h: number;
  suspended_users: number;
  total_servers: number;
  total_messages: number;
  voice_connections: number;
  uptime_secs: number;
  version: string;
  gateway_online: boolean;
}

// ── Flag constants (mirrors nexus-common::user_flags) ─────────────────────────
export const USER_FLAGS = {
  STAFF:             1 << 0,
  BOT:               1 << 1,
  EMAIL_VERIFIED:    1 << 2,
  EARLY_SUPPORTER:   1 << 3,
  PREMIUM_SUPPORTER: 1 << 4,
  DISABLED:          1 << 5,
  SUSPENDED:         1 << 6,
  INSTANCE_ADMIN:    1 << 7,
} as const;

// ── Session singleton ─────────────────────────────────────────────────────────

// Starts empty and is filled by whoami() on load. Nothing is persisted: a
// session stored here would be a second, staler answer to a question the
// server can answer authoritatively on every request.
let _session: Session | null = null;

const _listeners = new Set<() => void>();

export function getSession(): Session | null { return _session; }

export function setSession(s: Session | null): void {
  _session = s;
  // Deliberately not persisted; see the declaration above. Clear anything an
  // older build of this console left behind, so it cannot be read by accident.
  try { localStorage.removeItem("nexus_admin_session"); } catch { /* private mode */ }
  _listeners.forEach((fn) => fn());
}

/**
 * Who the server says we are, and whether they may use this console.
 *
 * There is no login form: the ecosystem already authenticated whoever is
 * looking. The only question left is authorisation, and /admin/overview is the
 * authority on it — 403 means signed in but not an instance admin, which is a
 * different answer from "signed out" and deserves different words.
 */
export async function whoami(): Promise<
  { state: "admin"; session: Session } | { state: "forbidden" } | { state: "anonymous" }
> {
  try {
    const res = await fetch(`${apiOrigin()}/api/v1/users/@me`, { credentials: "include" });
    if (res.status === 401) return { state: "anonymous" };
    if (!res.ok) return { state: "anonymous" };
    const body = await res.json();
    const user = body.user ?? body;

    const gate = await fetch(`${apiOrigin()}/api/v1/admin/overview`, { credentials: "include" });
    if (gate.status === 403) return { state: "forbidden" };
    if (gate.status === 401) return { state: "anonymous" };

    return {
      state: "admin",
      session: {
        accessToken: "",
        username: user.username,
        userId: user.id,
        serverUrl: apiOrigin(),
      },
    };
  } catch {
    return { state: "anonymous" };
  }
}

export function subscribeSession(fn: () => void): () => void {
  _listeners.add(fn);
  return () => _listeners.delete(fn);
}

/**
 * The server this console talks to.
 *
 * Always the origin that served it. This app is a browser client behind the
 * ecosystem proxy, which injects a verified identity header for *this*
 * hostname — a request sent anywhere else arrives unauthenticated. The dev
 * override exists because a Vite server on :5173 really is a different origin
 * from the API.
 */
/** Vite's build-time env, read without requiring the vite/client types. */
function viteEnv(): Record<string, string | undefined> {
  return (import.meta as unknown as { env?: Record<string, string | undefined> }).env ?? {};
}

export function apiOrigin(): string {
  const env = viteEnv();
  const override = env.DEV ? env.VITE_NEXUS_API_ORIGIN : undefined;
  return (override ?? window.location.origin).replace(/\/$/, "");
}

// ── HTTP client ───────────────────────────────────────────────────────────────

export class ApiError extends Error {
  constructor(public status: number, message: string) {
    super(message);
    this.name = "ApiError";
  }
}

export async function apiFetch<T>(path: string, init: RequestInit = {}): Promise<T> {
  // No Authorization header. The server stopped accepting locally-minted JWTs
  // when its login was deleted; identity arrives as a signed header the proxy
  // adds, which the browser can neither see nor forge.
  const res = await fetch(`${apiOrigin()}/api/v1${path}`, {
    ...init,
    credentials: "include",
    headers: {
      "Content-Type": "application/json",
      ...(init.headers ?? {}),
    },
  });
  if (!res.ok) {
    const text = await res.text().catch(() => `HTTP ${res.status}`);
    throw new ApiError(res.status, text || `HTTP ${res.status}`);
  }
  return res.json() as Promise<T>;
}

// ── API namespace ─────────────────────────────────────────────────────────────

export const api = {
  // ── Instance health ────────────────────────────────────────────────────────
  overview: () => apiFetch<InstanceOverview>("/admin/overview"),
  health: () => apiFetch<HealthCheck>("/health"),

  // ── User management ────────────────────────────────────────────────────────
  userStats: () => apiFetch<UserStats>("/admin/users/stats"),
  users: (params: { limit?: number; offset?: number; q?: string }) => {
    const qs = new URLSearchParams(
      Object.entries(params)
        .filter(([, v]) => v !== undefined && v !== "")
        .map(([k, v]) => [k, String(v)])
    ).toString();
    return apiFetch<UserList>(`/admin/users${qs ? `?${qs}` : ""}`);
  },
  getUser: (id: string) => apiFetch<AdminUser>(`/admin/users/${id}`),
  suspendUser: (id: string) =>
    apiFetch<{ suspended: boolean }>(`/admin/users/${id}/suspend`, { method: "POST" }),
  unsuspendUser: (id: string) =>
    apiFetch<{ suspended: boolean }>(`/admin/users/${id}/unsuspend`, { method: "POST" }),
  disableUser: (id: string) =>
    apiFetch<{ disabled: boolean }>(`/admin/users/${id}/disable`, { method: "POST" }),

  // ── Server management ──────────────────────────────────────────────────────
  servers: (params?: { limit?: number; offset?: number }) => {
    const qs = params ? new URLSearchParams(
      Object.entries(params).filter(([, v]) => v !== undefined).map(([k, v]) => [k, String(v)])
    ).toString() : "";
    return apiFetch<{ servers: AdminServer[]; total: number }>(`/admin/servers${qs ? `?${qs}` : ""}`);
  },
  deleteServer: (id: string) =>
    apiFetch<{ deleted: boolean }>(`/admin/servers/${id}`, { method: "DELETE" }),

  // ── Federation ─────────────────────────────────────────────────────────────
  federationStatus: () => apiFetch<FederationStatus>("/admin/federation/status"),
  federationPeers: () => apiFetch<{ peers: FederationPeer[] }>("/admin/federation/peers"),
  federationAudit: () => apiFetch<{ entries: FederationAuditEntry[] }>("/admin/federation/audit"),
  instanceAudit: (params?: { action?: string; limit?: number }) => {
    const qs = params ? new URLSearchParams(
      Object.entries(params).filter(([, v]) => v !== undefined).map(([k, v]) => [k, String(v)])
    ).toString() : "";
    return apiFetch<{ entries: InstanceAuditEntry[]; total: number }>(`/admin/instance-audit${qs ? `?${qs}` : ""}`);
  },
  blockPeer: (domain: string) =>
    apiFetch(`/admin/federation/peers/${domain}/block`, { method: "POST" }),
  unblockPeer: (domain: string) =>
    apiFetch(`/admin/federation/peers/${domain}/unblock`, { method: "POST" }),

  // ── Moderation ─────────────────────────────────────────────────────────────
  // Reports are per-server — admin can pick a server to review
  serverReports: (serverId: string) =>
    apiFetch<{ reports: ModerationReport[] }>(`/servers/${serverId}/reports`),
  resolveReport: (serverId: string, reportId: string) =>
    apiFetch(`/servers/${serverId}/reports/${reportId}/resolve`, { method: "POST", body: "{}" }),
  dismissReport: (serverId: string, reportId: string) =>
    apiFetch(`/servers/${serverId}/reports/${reportId}/dismiss`, { method: "POST", body: "{}" }),

  // ── Prometheus metrics (raw text) ──────────────────────────────────────────
  metrics: () =>
    fetch(`${apiOrigin()}/api/v1/metrics`, { credentials: "include" }).then((r) => r.text()),
};
