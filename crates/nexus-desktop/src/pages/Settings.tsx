import { useState, useEffect } from "react";
import { useStore } from "../store";
import { invoke } from "../invoke";
import ThemeSwitcher from "../themes/ThemeSwitcher";
import FederationPanel from "../components/FederationPanel";
import AccessibilitySettingsPanel from "../components/AccessibilitySettingsPanel";
import VoiceSettingsPanel from "../components/VoiceSettingsPanel";
import AiIntelligencePanel from "../components/AiIntelligencePanel";
import AdminAnalyticsPanel from "../components/AdminAnalyticsPanel";
import ImportWizard from "../components/ImportWizard";
import type { PluginManifest } from "../plugins/types";
import { formatDistanceToNow } from "date-fns";
import clsx from "clsx";
import {
  type FontSize,
  type Density,
  type ColorScheme,
  getFontSize,
  getDensity,
  getColorScheme,
  saveFontSize,
  saveDensity,
  saveColorScheme,
  isDarkScheme,
} from "../themes/appearance";

interface E2eeDevice {
  id: string;
  userId: string;
  name: string;
  deviceType: string;
  lastSeenAt?: string;
  verified: boolean;
  createdAt: string;
}

interface Session {
  id: string;
  device_info: string | null;
  created_at: string;
  last_seen_at: string;
}

export default function SettingsPage() {
  const {
    plugins,
    enabledPlugins,
    installPlugin,
    uninstallPlugin,
    togglePlugin,
    session,
    setSession,
    activeThemeId,
    setActiveTheme,
    experienceProfile,
    loadExperienceProfile,
    setExperienceMode,
    activeServerId,
  } = useStore();
  const [urlInput, setUrlInput] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);

  // Profile section state
  const [displayName, setDisplayName] = useState(session?.displayName ?? "");
  const [avatarUrl, setAvatarUrl] = useState(session?.avatar ?? "");
  const [profileSaving, setProfileSaving] = useState(false);
  const [profileMsg, setProfileMsg] = useState<string | null>(null);

  // Status section state
  const [statusPresence, setStatusPresence] = useState<"online"|"idle"|"dnd"|"invisible">("online");
  const [statusText, setStatusText] = useState("");
  const [statusExpiry, setStatusExpiry] = useState<"none"|"1h"|"4h"|"today"|"tomorrow"|"custom">("none");
  const [statusCustomExpiry, setStatusCustomExpiry] = useState("");
  const [statusSaving, setStatusSaving] = useState(false);
  const [statusMsg, setStatusMsg] = useState<string | null>(null);

  // ── Notification preferences (persisted to localStorage) ──────────────────
  const [notifEnabled, setNotifEnabled] = useState(
    () => localStorage.getItem("nexus:notif:enabled") !== "false"
  );
  const [notifMentionsOnly, setNotifMentionsOnly] = useState(
    () => localStorage.getItem("nexus:notif:mentionsOnly") === "true"
  );
  const [notifSound, setNotifSound] = useState(
    () => localStorage.getItem("nexus:notif:sound") !== "false"
  );
  const toggleNotif = (key: string, value: boolean) => {
    localStorage.setItem(`nexus:notif:${key}`, String(value));
    if (key === "enabled") setNotifEnabled(value);
    if (key === "mentionsOnly") setNotifMentionsOnly(value);
    if (key === "sound") setNotifSound(value);
  };

  // ── Privacy preferences ────────────────────────────────────────────────────
  // ── Appearance preferences ────────────────────────────────────────────────
  const [colorScheme, setColorScheme] = useState<ColorScheme>(() => getColorScheme());
  const [fontSize, setFontSize] = useState<FontSize>(() => getFontSize());
  const [density, setDensity] = useState<Density>(() => getDensity());

  const handleColorScheme = (scheme: ColorScheme) => {
    setColorScheme(scheme);
    saveColorScheme(scheme);
    const dark = isDarkScheme(scheme);
    if (!dark && activeThemeId !== "nexus-light") {
      setActiveTheme("nexus-light");
    } else if (dark && activeThemeId === "nexus-light") {
      setActiveTheme("nexus-dark");
    }
  };

  // Listen for OS color-scheme changes when the user is in "system" mode
  useEffect(() => {
    if (colorScheme !== "system") return;
    const mq = window.matchMedia("(prefers-color-scheme: dark)");
    const handler = (e: MediaQueryListEvent) => {
      if (e.matches) {
        if (activeThemeId === "nexus-light") setActiveTheme("nexus-dark");
      } else {
        if (activeThemeId !== "nexus-light") setActiveTheme("nexus-light");
      }
    };
    mq.addEventListener("change", handler);
    return () => mq.removeEventListener("change", handler);
  }, [colorScheme, activeThemeId, setActiveTheme]);
  const [privacyReadReceipts, setPrivacyReadReceipts] = useState(
    () => localStorage.getItem("nexus:privacy:readReceipts") !== "false"
  );
  const [privacyOnlineStatus, setPrivacyOnlineStatus] = useState(
    () => localStorage.getItem("nexus:privacy:onlineStatus") !== "false"
  );
  const togglePrivacy = (key: string, value: boolean) => {
    localStorage.setItem(`nexus:privacy:${key}`, String(value));
    if (key === "readReceipts") setPrivacyReadReceipts(value);
    if (key === "onlineStatus") setPrivacyOnlineStatus(value);
  };

  // ── E2EE Devices ───────────────────────────────────────────────────────────
  const [devices, setDevices] = useState<E2eeDevice[]>([]);
  const [devicesLoading, setDevicesLoading] = useState(false);
  const [devicesError, setDevicesError] = useState<string | null>(null);

  const loadDevices = () => {
    if (!session) return;
    setDevicesLoading(true);
    setDevicesError(null);
    invoke<E2eeDevice[]>("list_devices", {})
      .then(setDevices)
      .catch((e) => setDevicesError(String(e)))
      .finally(() => setDevicesLoading(false));
  };

  useEffect(() => {
    loadDevices();
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [session]);

  const revokeDevice = async (deviceId: string) => {
    try {
      await invoke("delete_device", { deviceId });
      setDevices((prev) => prev.filter((d) => d.id !== deviceId));
    } catch (e) {
      setDevicesError(`Failed to revoke device: ${String(e)}`);
    }
  };

  // ── Sessions ───────────────────────────────────────────────────────────────
  const [sessions, setSessions] = useState<Session[]>([]);
  const [sessionsLoading, setSessionsLoading] = useState(false);
  const [sessionsError, setSessionsError] = useState<string | null>(null);

  const loadSessions = () => {
    if (!session) return;
    setSessionsLoading(true);
    setSessionsError(null);
    invoke<{ sessions: Session[] }>("list_sessions", {})
      .then((res) => setSessions(res.sessions))
      .catch((e) => setSessionsError(String(e)))
      .finally(() => setSessionsLoading(false));
  };

  useEffect(() => {
    loadSessions();
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [session]);

  useEffect(() => {
    if (session) {
      loadExperienceProfile();
    }
  }, [session, loadExperienceProfile]);

  const revokeSession = async (sessionId: string) => {
    try {
      await invoke("revoke_session", { sessionId });
      setSessions((prev) => prev.filter((s) => s.id !== sessionId));
    } catch (e) {
      setSessionsError(`Failed to revoke session: ${String(e)}`);
    }
  };

  const revokeAllOtherSessions = async () => {
    try {
      await invoke("revoke_all_sessions", {});
      // Keep only the current session (the one with no current marker will remain)
      loadSessions();
    } catch (e) {
      setSessionsError(`Failed to revoke sessions: ${String(e)}`);
    }
  };

  const saveProfile = async () => {
    if (!session) return;
    setProfileSaving(true);
    setProfileMsg(null);
    try {
      await invoke("update_profile", {
        displayName: displayName.trim() || null,
        avatarUrl: avatarUrl.trim() || null,
      });
      setSession({
        ...session,
        displayName: displayName.trim() || undefined,
        avatar: avatarUrl.trim() || undefined,
      });
      setProfileMsg("Saved!");
      setTimeout(() => setProfileMsg(null), 2500);
    } catch (e) {
      setProfileMsg(`Error: ${String(e)}`);
    } finally {
      setProfileSaving(false);
    }
  };

  const saveStatus = async () => {
    setStatusSaving(true);
    setStatusMsg(null);
    try {
      let expiresAt: string | null = null;
      if (statusExpiry === "1h") {
        expiresAt = new Date(Date.now() + 3600_000).toISOString();
      } else if (statusExpiry === "4h") {
        expiresAt = new Date(Date.now() + 4 * 3600_000).toISOString();
      } else if (statusExpiry === "today") {
        const d = new Date(); d.setHours(23, 59, 59, 999);
        expiresAt = d.toISOString();
      } else if (statusExpiry === "tomorrow") {
        const d = new Date(); d.setDate(d.getDate() + 1); d.setHours(23, 59, 59, 999);
        expiresAt = d.toISOString();
      } else if (statusExpiry === "custom" && statusCustomExpiry) {
        expiresAt = new Date(statusCustomExpiry).toISOString();
      }
      await invoke("update_presence_status", {
        presence: statusPresence,
        customStatus: statusText.trim() || null,
        expiresAt,
      });
      setStatusMsg("Status updated!");
      setTimeout(() => setStatusMsg(null), 2500);
    } catch (e) {
      setStatusMsg(`Error: ${String(e)}`);
    } finally {
      setStatusSaving(false);
    }
  };

  const handleInstall = async () => {
    const url = urlInput.trim();
    if (!url) return;
    setError(null);
    setLoading(true);
    try {
      const res = await fetch(url);
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const manifest: PluginManifest = await res.json();
      if (!manifest.id || !manifest.name || !manifest.url) {
        throw new Error("Invalid plugin manifest (missing id, name, or url).");
      }
      installPlugin(manifest);
      setUrlInput("");
    } catch (e: unknown) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  };

  const [deletePassword, setDeletePassword] = useState("");
  const [deleteMsg, setDeleteMsg] = useState<string | null>(null);
  const [deleteLoading, setDeleteLoading] = useState(false);

  const cancelAccountDeletion = async () => {
    try {
      await invoke("cancel_account_deletion", {});
      setDeleteMsg("Deletion cancelled.");
    } catch (e) {
      setDeleteMsg(`Error: ${String(e)}`);
    }
  };

  const requestAccountDeletion = async () => {
    if (!deletePassword.trim()) {
      setDeleteMsg("Enter your password to confirm deletion.");
      return;
    }
    setDeleteLoading(true);
    setDeleteMsg(null);
    try {
      await invoke("delete_account", { password: deletePassword });
      setDeleteMsg("Deletion scheduled. You can cancel it below.");
      setDeletePassword("");
    } catch (e) {
      setDeleteMsg(`Error: ${String(e)}`);
    } finally {
      setDeleteLoading(false);
    }
  };

  return (
    <div className="flex-1 overflow-y-auto p-8 text-sm">
      <h1 className="text-2xl font-bold mb-6">Settings</h1>

      {/* ── Profile ──────────────────────────────────────────── */}
      <section className="mb-10">
        <h2 className="text-base font-semibold mb-4 border-b border-bg-600 pb-2">Profile</h2>
        <div className="flex gap-6 items-start max-w-lg">
          {/* Avatar preview */}
          <div className="shrink-0">
            {avatarUrl ? (
              <img
                src={avatarUrl}
                alt="avatar"
                className="w-16 h-16 rounded-full object-cover"
                onError={(e) => { (e.target as HTMLImageElement).style.display = 'none'; }}
              />
            ) : (
              <div className="w-16 h-16 rounded-full bg-accent-500 flex items-center justify-center text-2xl font-bold text-white select-none">
                {(session?.displayName ?? session?.username ?? '?')[0].toUpperCase()}
              </div>
            )}
          </div>

          <div className="flex flex-col gap-3 flex-1">
            <div>
              <label className="block text-xs font-semibold text-muted uppercase tracking-wide mb-1">Display Name</label>
              <input
                className="input w-full"
                placeholder={session?.username ?? ''}
                value={displayName}
                onChange={(e) => setDisplayName(e.target.value)}
              />
            </div>
            <div>
              <label className="block text-xs font-semibold text-muted uppercase tracking-wide mb-1">Avatar URL</label>
              <input
                className="input w-full"
                placeholder="https://example.com/avatar.png"
                value={avatarUrl}
                onChange={(e) => setAvatarUrl(e.target.value)}
              />
            </div>
            <div className="flex items-center gap-3">
              <button
                onClick={saveProfile}
                disabled={profileSaving}
                className="btn-primary disabled:opacity-50"
              >
                {profileSaving ? 'Saving…' : 'Save Changes'}
              </button>
              {profileMsg && (
                <span className={profileMsg.startsWith('Error') ? 'text-dnd text-xs' : 'text-green-400 text-xs'}>
                  {profileMsg}
                </span>
              )}
            </div>
          </div>
        </div>
      </section>

      {/* ── Status ───────────────────────────────────────────── */}
      <section className="mb-10">
        <h2 className="text-base font-semibold mb-4 border-b border-bg-600 pb-2">Status</h2>
        <div className="flex flex-col gap-4 max-w-md">
          {/* Presence selector */}
          <div>
            <p className="text-xs font-semibold text-muted uppercase tracking-wider mb-2">Presence</p>
            <div className="flex gap-2">
              {(["online", "idle", "dnd", "invisible"] as const).map((p) => (
                <button
                  key={p}
                  aria-pressed={statusPresence === p}
                  onClick={() => setStatusPresence(p)}
                  className={clsx(
                    "flex-1 rounded-lg border py-2 text-xs font-medium capitalize transition-colors",
                    statusPresence === p
                      ? "border-accent-600 bg-accent-600/15 text-accent-300"
                      : "border-bg-600/50 bg-bg-800 text-muted hover:text-fg"
                  )}
                >
                  {p === "dnd" ? "Do Not Disturb" : p.charAt(0).toUpperCase() + p.slice(1)}
                </button>
              ))}
            </div>
          </div>

          {/* Custom status text */}
          <div>
            <label className="block text-xs font-semibold text-muted uppercase tracking-wider mb-1">
              Custom Status
            </label>
            <input
              className="input w-full"
              placeholder="What are you up to?"
              value={statusText}
              maxLength={128}
              onChange={(e) => setStatusText(e.target.value)}
            />
          </div>

          {/* Expiry presets */}
          <div>
            <p className="text-xs font-semibold text-muted uppercase tracking-wider mb-2">Clear after</p>
            <div className="flex flex-wrap gap-2">
              {(["none", "1h", "4h", "today", "tomorrow", "custom"] as const).map((opt) => (
                <button
                  key={opt}
                  aria-pressed={statusExpiry === opt}
                  onClick={() => setStatusExpiry(opt)}
                  className={clsx(
                    "rounded-lg border px-3 py-1 text-xs font-medium transition-colors",
                    statusExpiry === opt
                      ? "border-accent-600 bg-accent-600/15 text-accent-300"
                      : "border-bg-600/50 bg-bg-800 text-muted hover:text-fg"
                  )}
                >
                  {opt === "none" ? "Never" : opt === "1h" ? "1 hour" : opt === "4h" ? "4 hours" : opt === "today" ? "Today" : opt === "tomorrow" ? "Tomorrow" : "Custom"}
                </button>
              ))}
            </div>
            {statusExpiry === "custom" && (
              <input
                type="datetime-local"
                value={statusCustomExpiry}
                min={new Date(Date.now() + 60_000).toISOString().slice(0, 16)}
                onChange={(e) => setStatusCustomExpiry(e.target.value)}
                className="mt-2 input w-full"
              />
            )}
          </div>

          <div className="flex items-center gap-3">
            <button
              onClick={saveStatus}
              disabled={statusSaving}
              className="btn-primary disabled:opacity-50"
            >
              {statusSaving ? "Saving…" : "Update Status"}
            </button>
            {statusMsg && (
              <span className={statusMsg.startsWith("Error") ? "text-dnd text-xs" : "text-green-400 text-xs"}>
                {statusMsg}
              </span>
            )}
          </div>
        </div>
      </section>

      {/* ── Experience Mode ───────────────────────────── */}
      <section className="mb-10">
        <h2 className="text-base font-semibold mb-4 border-b border-bg-600 pb-2">Experience Mode</h2>
        <p className="text-sm text-muted mb-3 max-w-2xl">
          Use Full mode for the complete Nexus platform, or Messaging mode for a focused chat-first experience.
        </p>
        <div className="flex gap-2 max-w-md">
          <button
            onClick={() => setExperienceMode("full")}
            className={clsx(
              "flex-1 rounded-lg border py-2 text-sm font-medium transition-colors",
              experienceProfile?.experienceMode === "full"
                ? "border-accent-600 bg-accent-600/15 text-accent-300"
                : "border-bg-600/50 bg-bg-800 text-muted hover:text-fg"
            )}
          >
            Full
          </button>
          <button
            onClick={() => setExperienceMode("messaging")}
            className={clsx(
              "flex-1 rounded-lg border py-2 text-sm font-medium transition-colors",
              experienceProfile?.experienceMode === "messaging"
                ? "border-accent-600 bg-accent-600/15 text-accent-300"
                : "border-bg-600/50 bg-bg-800 text-muted hover:text-fg"
            )}
          >
            Messaging
          </button>
        </div>
      </section>

      <section className="mb-10">
        <h2 className="text-base font-semibold mb-5 border-b border-bg-600 pb-2">Appearance</h2>

        {/* ── Color Scheme ──────────────────────────────── */}
        <div className="mb-6 max-w-md">
          <p className="text-xs font-semibold text-muted uppercase tracking-wider mb-2">Color Scheme</p>
          <div className="flex gap-2">
            {(["system", "dark", "light"] as const).map((scheme) => (
              <button
                key={scheme}
                aria-pressed={colorScheme === scheme}
                onClick={() => handleColorScheme(scheme)}
                className={clsx(
                  "flex-1 rounded-lg border py-2 text-sm font-medium transition-colors capitalize",
                  colorScheme === scheme
                    ? "border-accent-600 bg-accent-600/15 text-accent-300"
                    : "border-bg-600/50 bg-bg-800 text-muted hover:text-fg"
                )}
              >
                {scheme === "system" ? "🖥 System" : scheme === "dark" ? "🌙 Dark" : "☀ Light"}
              </button>
            ))}
          </div>
          <p className="text-xs text-muted mt-1.5">
            {colorScheme === "system"
              ? "Follows your OS dark/light mode setting automatically."
              : colorScheme === "dark"
              ? "Always use a dark theme."
              : "Always use the light theme."}
          </p>
        </div>

        {/* ── Theme ─────────────────────────────────────── */}
        <div className="mb-6 max-w-xs">
          <ThemeSwitcher />
        </div>

        {/* ── Font Size ─────────────────────────────────── */}
        <div className="mb-6 max-w-md">
          <p className="text-xs font-semibold text-muted uppercase tracking-wider mb-2">Font Size</p>
          <div className="flex gap-1.5 items-end">
            {([
              { id: "sm" as const, label: "Small", size: "text-xs" },
              { id: "md" as const, label: "Normal", size: "text-sm" },
              { id: "lg" as const, label: "Large", size: "text-base" },
              { id: "xl" as const, label: "Extra Large", size: "text-lg" },
            ]).map(({ id, label, size }) => (
              <button
                key={id}
                aria-pressed={fontSize === id}
                onClick={() => {
                  setFontSize(id);
                  saveFontSize(id);
                }}
                className={clsx(
                  "flex-1 rounded-lg border py-2 transition-colors text-center",
                  size,
                  fontSize === id
                    ? "border-accent-600 bg-accent-600/15 text-accent-300"
                    : "border-bg-600/50 bg-bg-800 text-muted hover:text-fg"
                )}
              >
                {label}
              </button>
            ))}
          </div>
          <div className="mt-2 rounded-lg bg-bg-800 border border-bg-600/30 px-3 py-2">
            <p className="text-muted text-[0.875em]">
              Preview: The quick brown fox jumps over the lazy dog.
            </p>
          </div>
        </div>

        {/* ── Message Density ───────────────────────────── */}
        <div className="mb-2 max-w-md">
          <p className="text-xs font-semibold text-muted uppercase tracking-wider mb-2">Message Density</p>
          <div className="flex gap-2">
            {([
              {
                id: "compact" as const,
                label: "Compact",
                desc: "Tightest spacing — maximise visible messages.",
              },
              {
                id: "cozy" as const,
                label: "Cozy",
                desc: "Balanced spacing. The default.",
              },
              {
                id: "roomy" as const,
                label: "Roomy",
                desc: "Extra breathing room between messages.",
              },
            ]).map(({ id, label, desc }) => (
              <button
                key={id}
                aria-pressed={density === id}
                onClick={() => {
                  setDensity(id);
                  saveDensity(id);
                }}
                className={clsx(
                  "flex-1 rounded-lg border p-3 text-left transition-colors",
                  density === id
                    ? "border-accent-600 bg-accent-600/15"
                    : "border-bg-600/50 bg-bg-800 hover:border-bg-500"
                )}
              >
                <p
                  className={clsx(
                    "text-sm font-medium",
                    density === id ? "text-accent-300" : "text-fg"
                  )}
                >
                  {label}
                </p>
                <p className="text-xs text-muted mt-0.5">{desc}</p>
              </button>
            ))}
          </div>
        </div>
      </section>

      {/* ── Accessibility ────────────────────────────── */}
      <section className="mb-10">
        <h2 className="text-base font-semibold mb-5 border-b border-bg-600 pb-2">Accessibility</h2>
        <AccessibilitySettingsPanel />
      </section>

      {/* ── Plugins ──────────────────────────────────── */}
      <section>
        <h2 className="text-base font-semibold mb-4 border-b border-bg-600 pb-2">Plugins</h2>

        {/* Install form */}
        <div className="mb-6">
          <p className="text-muted mb-2">
            Enter the URL of a plugin manifest JSON to install it.
          </p>
          <div className="flex gap-2 max-w-lg">
            <input
              type="url"
              placeholder="https://example.com/my-plugin/manifest.json"
              value={urlInput}
              onChange={(e) => setUrlInput(e.target.value)}
              onKeyDown={(e) => e.key === "Enter" && handleInstall()}
              className="input flex-1"
            />
            <button
              onClick={handleInstall}
              disabled={loading || !urlInput.trim()}
              className="btn-primary whitespace-nowrap disabled:opacity-50"
            >
              {loading ? "Installing…" : "Install"}
            </button>
          </div>
          {error && <p className="mt-2 text-dnd text-xs">{error}</p>}
        </div>

        {/* Installed plugins list */}
        {plugins.length === 0 ? (
          <p className="text-muted">No plugins installed.</p>
        ) : (
          <ul className="flex flex-col gap-3 max-w-2xl">
            {plugins.map((p) => {
              const enabled = enabledPlugins.includes(p.id);
              return (
                <li
                  key={p.id}
                  className="flex items-start gap-4 bg-bg-800 rounded-lg p-4"
                >
                  {p.iconUrl && (
                    <img
                      src={p.iconUrl}
                      alt=""
                      className="w-10 h-10 rounded-lg flex-shrink-0 object-cover"
                    />
                  )}
                  <div className="flex-1 min-w-0">
                    <div className="font-medium">{p.name}</div>
                    <div className="text-xs text-muted">{p.id} · v{p.version}</div>
                    {p.description && (
                      <div className="text-xs text-muted mt-1">{p.description}</div>
                    )}
                  </div>
                  <div className="flex gap-2 flex-shrink-0">
                    <button
                      onClick={() => togglePlugin(p.id)}
                      className={`px-3 py-1 rounded text-xs font-medium transition-colors ${
                        enabled
                          ? "bg-accent-600 hover:bg-accent-500 text-white"
                          : "bg-bg-600 hover:bg-bg-500 text-muted hover:text-white"
                      }`}
                    >
                      {enabled ? "Disable" : "Enable"}
                    </button>
                    <button
                      onClick={() => uninstallPlugin(p.id)}
                      className="px-3 py-1 rounded text-xs font-medium bg-bg-600 hover:bg-dnd text-muted hover:text-white transition-colors"
                    >
                      Uninstall
                    </button>
                  </div>
                </li>
              );
            })}
          </ul>
        )}
      </section>

      {/* ── Notifications ──────────────────────────────── */}
      <section className="mb-10">
        <h2 className="text-base font-semibold mb-4 border-b border-bg-600 pb-2">Notifications</h2>
        <div className="flex flex-col gap-4 max-w-md">
          <ToggleRow
            label="Enable notifications"
            description="Show desktop notifications for new messages"
            value={notifEnabled}
            onChange={(v) => toggleNotif("enabled", v)}
          />
          <ToggleRow
            label="Mentions only"
            description="Only notify for @mentions and direct messages"
            value={notifMentionsOnly}
            onChange={(v) => toggleNotif("mentionsOnly", v)}
          />
          <ToggleRow
            label="Notification sounds"
            description="Play a sound when a notification arrives"
            value={notifSound}
            onChange={(v) => toggleNotif("sound", v)}
          />
        </div>
      </section>

      {/* ── Privacy ─────────────────────────────────────── */}
      <section className="mb-10">
        <h2 className="text-base font-semibold mb-4 border-b border-bg-600 pb-2">Privacy</h2>
        <div className="flex flex-col gap-4 max-w-md">
          <ToggleRow
            label="Show online status"
            description="Let others see when you are online"
            value={privacyOnlineStatus}
            onChange={(v) => togglePrivacy("onlineStatus", v)}
          />
          <ToggleRow
            label="Read receipts"
            description="Show others when you have read their messages"
            value={privacyReadReceipts}
            onChange={(v) => togglePrivacy("readReceipts", v)}
          />
        </div>
      </section>

      {/* ── Devices ─────────────────────────────────────── */}
      <section className="mb-10">
        <h2 className="text-base font-semibold mb-4 border-b border-bg-600 pb-2">Devices</h2>
        {devicesError ? (
          <p className="text-sm text-red-400">Failed to load devices: {devicesError}</p>
        ) : devicesLoading ? (
          <p className="text-sm text-muted">Loading devices…</p>
        ) : devices.length === 0 ? (
          <p className="text-sm text-muted">No registered E2EE devices found.</p>
        ) : (
          <ul className="flex flex-col gap-2 max-w-lg">
            {devices.map((d) => (
              <li key={d.id} className="flex items-center gap-3 rounded-lg bg-bg-800 p-3">
                <div className="flex-1 min-w-0">
                  <p className="text-sm font-medium text-fg">{d.name}</p>
                  <p className="text-xs text-muted">
                    {d.deviceType} · {d.verified ? "✓ Verified" : "Unverified"}
                  </p>
                  {d.lastSeenAt && (
                    <p className="text-xs text-muted/60">
                      Last seen {formatDistanceToNow(new Date(d.lastSeenAt), { addSuffix: true })}
                    </p>
                  )}
                </div>
                <code className="text-[10px] text-muted/60 font-mono shrink-0">
                  {d.id.slice(0, 8)}…
                </code>
                <button
                  onClick={() => revokeDevice(d.id)}
                  className="text-[11px] text-red-400 hover:text-red-300 hover:bg-red-500/10 rounded px-2 py-0.5 transition-colors shrink-0"
                  title="Revoke this device"
                >
                  Revoke
                </button>
              </li>
            ))}
          </ul>
        )}
      </section>

      {/* ── Sessions ──────────────────────────────────────────────────────── */}
      <section className="mb-10">
        <div className="flex items-center justify-between mb-4 border-b border-bg-600 pb-2">
          <h2 className="text-base font-semibold">Active Sessions</h2>
          {sessions.length > 1 && (
            <button
              onClick={revokeAllOtherSessions}
              className="text-xs text-red-400 hover:text-red-300 transition-colors"
            >
              Revoke all other sessions
            </button>
          )}
        </div>
        {sessionsError && (
          <p className="text-sm text-red-400 mb-2">Error: {sessionsError}</p>
        )}
        {sessionsLoading ? (
          <p className="text-sm text-muted">Loading sessions…</p>
        ) : sessions.length === 0 ? (
          <p className="text-sm text-muted">No active sessions found.</p>
        ) : (
          <ul className="flex flex-col gap-2 max-w-lg">
            {sessions.map((s) => (
              <li key={s.id} className="flex items-start gap-3 rounded-lg bg-bg-800 p-3">
                <div className="flex-1 min-w-0">
                  <p className="text-sm font-medium text-fg truncate">
                    {s.device_info ?? "Unknown device"}
                  </p>
                  <p className="text-xs text-muted/60">
                    Last active {formatDistanceToNow(new Date(s.last_seen_at), { addSuffix: true })}
                    {" · "}
                    Created {formatDistanceToNow(new Date(s.created_at), { addSuffix: true })}
                  </p>
                </div>
                <button
                  onClick={() => revokeSession(s.id)}
                  className="text-[11px] text-red-400 hover:text-red-300 hover:bg-red-500/10 rounded px-2 py-0.5 transition-colors shrink-0 mt-0.5"
                  title="Revoke this session"
                >
                  Revoke
                </button>
              </li>
            ))}
          </ul>
        )}
      </section>

      {/* ── Connection ──────────────────────────────────── */}
      <section className="mb-10">
        <h2 className="text-base font-semibold mb-4 border-b border-bg-600 pb-2">Connection</h2>
        <div className="max-w-md">
          <label className="block text-xs font-semibold text-muted uppercase tracking-wide mb-1">
            Server URL
          </label>
          <div className="flex items-center gap-2 rounded-lg bg-bg-800 border border-bg-600/50 px-3 py-2">
            <code className="text-sm text-fg flex-1 truncate">
              {session?.serverUrl ?? "Not connected"}
            </code>
            <span className="text-xs text-green-400">●</span>
          </div>
          <p className="text-xs text-muted mt-1">
            To change the server, log out and connect to a different instance.
          </p>
        </div>
      </section>

      {/* ── Federation — instance-admin only (enforced server-side) ──── */}
      <section className="space-y-4" aria-labelledby="fed-heading">
        <div>
          <h2 id="fed-heading" className="text-sm font-semibold text-fg">
            Federation
          </h2>
          <p className="text-xs text-muted mt-0.5">
            Manage which remote Nexus instances this server federates with.
            Requires instance-admin privileges.
          </p>
        </div>
        <FederationPanel />
      </section>

      {/* ── Voice ─────────────────────────────────────────────────────── */}
      <section className="mb-10" aria-labelledby="voice-heading">
        <h2 id="voice-heading" className="text-base font-semibold mb-4 border-b border-bg-600 pb-2">
          Voice & Audio
        </h2>
        <VoiceSettingsPanel onClose={() => {}} />
      </section>

      {/* ── AI Intelligence ──────────────────────────────────────────── */}
      <section className="mb-10" aria-labelledby="ai-heading">
        <h2 id="ai-heading" className="text-base font-semibold mb-4 border-b border-bg-600 pb-2">
          AI Features
        </h2>
        <p className="text-xs text-muted mb-4">
          All AI features are opt-in, on-device or self-hosted only. No data leaves your instance.
        </p>
        <AiIntelligencePanel />
      </section>

      {/* ── Import & Migration ───────────────────────────────────────── */}
      {session && activeServerId && (
        <section className="mb-10" aria-labelledby="import-heading">
          <h2 id="import-heading" className="text-base font-semibold mb-4 border-b border-bg-600 pb-2">
            Import Data
          </h2>
          <p className="text-xs text-muted mb-4">
            Migrate messages and channels from Discord, Slack, or Matrix exports.
          </p>
          <ImportWizard serverId={activeServerId} />
        </section>
      )}

      {/* ── Admin Analytics — instance-admin only ────────────────────── */}
      {session && activeServerId && (
        <section className="mb-10" aria-labelledby="analytics-heading">
          <h2 id="analytics-heading" className="text-base font-semibold mb-4 border-b border-bg-600 pb-2">
            Server Analytics
          </h2>
          <AdminAnalyticsPanel serverId={activeServerId} />
        </section>
      )}

      <section className="mb-10">
        <h2 className="text-base font-semibold mb-4 border-b border-bg-600 pb-2">Account</h2>
        <div className="max-w-lg rounded-lg border border-bg-600/50 bg-bg-800 p-4 space-y-3">
          <p className="text-sm text-muted">
            Request account deletion. You’ll have a 30-day grace period and can cancel it.
          </p>
          <input
            type="password"
            className="input w-full"
            placeholder="Confirm password"
            value={deletePassword}
            onChange={(e) => setDeletePassword(e.target.value)}
          />
          <div className="flex gap-2">
            <button
              onClick={requestAccountDeletion}
              disabled={deleteLoading}
              className="btn-danger disabled:opacity-50"
            >
              {deleteLoading ? "Working…" : "Delete Account"}
            </button>
            <button
              onClick={cancelAccountDeletion}
              className="btn-secondary"
            >
              Cancel Deletion
            </button>
          </div>
          {deleteMsg && <p className="text-xs text-muted">{deleteMsg}</p>}
        </div>
      </section>
    </div>
  );
}

// ── ToggleRow helper ─────────────────────────────────────────────────────────
function ToggleRow({
  label,
  description,
  value,
  onChange,
}: {
  label: string;
  description?: string;
  value: boolean;
  onChange: (v: boolean) => void;
}) {
  return (
    <div className="flex items-center justify-between gap-4">
      <div className="flex-1">
        <p className="text-sm font-medium text-fg">{label}</p>
        {description && <p className="text-xs text-muted mt-0.5">{description}</p>}
      </div>
      <button
        type="button"
        onClick={() => onChange(!value)}
        aria-pressed={value}
        className={`relative shrink-0 h-5 w-9 rounded-full transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent-500 ${
          value ? "bg-accent-600" : "bg-bg-600/60"
        }`}
      >
        <span
          className={`absolute top-0.5 left-0.5 h-4 w-4 rounded-full bg-white shadow transition-transform ${
            value ? "translate-x-4" : "translate-x-0"
          }`}
        />
      </button>
    </div>
  );
}
