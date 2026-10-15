import { useState, useRef } from "react";
import { useTranslation } from "react-i18next";
import { useStore } from "../store";
import clsx from "clsx";
import PhantomKeyManager from "./PhantomKeyManager";

type Tab = "profile" | "security" | "appearance" | "connection";

export default function SettingsPanel({ onClose }: { onClose: () => void }) {
  const { t } = useTranslation();
  const {
    session,
    setSession,
    gatewayStatus,
    changePassword,
    updateProfile,
    deleteAccount,
    cancelAccountDeletion,
  } = useStore();
  const [tab, setTab] = useState<Tab>("profile");

  // Profile
  const [displayName, setDisplayName] = useState(session?.displayName ?? "");
  const [bio, setBio] = useState(session?.bio ?? "");
  const [status, setStatus] = useState(session?.status ?? "");
  const [avatar, setAvatar] = useState(session?.avatar ?? "");
  const [profileSaving, setProfileSaving] = useState(false);
  const [profileMsg, setProfileMsg] = useState<{ ok: boolean; text: string } | null>(null);

  // Security
  const [currentPw, setCurrentPw] = useState("");
  const [newPw, setNewPw] = useState("");
  const [confirmPw, setConfirmPw] = useState("");
  const [pwSaving, setPwSaving] = useState(false);
  const [pwMsg, setPwMsg] = useState<{ ok: boolean; text: string } | null>(null);
  const [deletePassword, setDeletePassword] = useState("");
  const [deleteSaving, setDeleteSaving] = useState(false);
  const [deleteMsg, setDeleteMsg] = useState<{ ok: boolean; text: string } | null>(null);

  // Appearance
  const [fontSize, setFontSize] = useState(() => localStorage.getItem("nx_font_size") ?? "14");
  const [density, setDensity] = useState<"compact" | "cozy" | "roomy">(
    () => (localStorage.getItem("nx_density") as "compact" | "cozy" | "roomy") ?? "cozy"
  );

  const dialogRef = useRef<HTMLDivElement>(null);

  async function saveProfile() {
    setProfileSaving(true);
    setProfileMsg(null);
    try {
      await updateProfile({
        display_name: displayName.trim() || undefined,
        bio: bio.trim() || undefined,
        status: status.trim() || undefined,
        avatar: avatar.trim() || undefined,
      });
      setProfileMsg({ ok: true, text: t("settings.saved") });
    } catch (e) {
      setProfileMsg({ ok: false, text: (e as Error).message });
    } finally {
      setProfileSaving(false);
    }
  }

  async function savePassword() {
    if (!currentPw || !newPw || !confirmPw) {
      setPwMsg({ ok: false, text: "All fields are required." });
      return;
    }
    if (newPw !== confirmPw) {
      setPwMsg({ ok: false, text: "New passwords don't match." });
      return;
    }
    if (newPw.length < 8) {
      setPwMsg({ ok: false, text: "Password must be at least 8 characters." });
      return;
    }
    if (newPw === currentPw) {
      setPwMsg({ ok: false, text: "New password must differ from current." });
      return;
    }
    setPwSaving(true);
    setPwMsg(null);
    try {
      await changePassword(currentPw, newPw);
      setPwMsg({ ok: true, text: "Password changed. All other sessions signed out." });
      setCurrentPw(""); setNewPw(""); setConfirmPw("");
    } catch (e) {
      setPwMsg({ ok: false, text: (e as Error).message });
    } finally {
      setPwSaving(false);
    }
  }

  async function requestDeletion() {
    if (!deletePassword.trim()) {
      setDeleteMsg({ ok: false, text: "Enter your password to confirm deletion." });
      return;
    }
    setDeleteSaving(true);
    setDeleteMsg(null);
    try {
      await deleteAccount(deletePassword);
      setDeleteMsg({ ok: true, text: "Deletion scheduled. You can cancel it below." });
      setDeletePassword("");
    } catch (e) {
      setDeleteMsg({ ok: false, text: (e as Error).message });
    } finally {
      setDeleteSaving(false);
    }
  }

  async function cancelDeletion() {
    setDeleteSaving(true);
    setDeleteMsg(null);
    try {
      await cancelAccountDeletion();
      setDeleteMsg({ ok: true, text: "Deletion cancelled." });
    } catch (e) {
      setDeleteMsg({ ok: false, text: (e as Error).message });
    } finally {
      setDeleteSaving(false);
    }
  }

  function applyAppearance() {
    document.documentElement.style.fontSize = `${fontSize}px`;
    localStorage.setItem("nx_font_size", fontSize);
    localStorage.setItem("nx_density", density);
    document.documentElement.setAttribute("data-density", density);
  }

  const gwColor =
    gatewayStatus === "online" ? "var(--nexus-color-state-success)"
    : gatewayStatus === "connecting" ? "#facc15"
    : "var(--nexus-color-state-danger)";

  const TABS: { id: Tab; label: string }[] = [
    { id: "profile",    label: t("settings.profile") },
    { id: "security",   label: t("settings.devices.title") },
    { id: "appearance", label: t("settings.appearance.title") },
    { id: "connection", label: t("settings.connection.title") },
  ];

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/60 backdrop-blur-sm"
      onClick={(e) => e.target === e.currentTarget && onClose()}
      role="dialog"
      aria-modal="true"
      aria-label={t("settings.title")}
    >
      <div
        ref={dialogRef}
        className="flex w-full max-w-2xl max-h-[85vh] rounded-2xl overflow-hidden shadow-2xl"
        style={{ background: "var(--bg-800)", border: "1px solid rgba(255,255,255,0.07)" }}
      >
        {/* Sidebar */}
        <aside className="w-44 shrink-0 flex flex-col gap-0.5 p-3" style={{ background: "var(--bg-900)" }}>
          <p className="text-[11px] font-bold uppercase tracking-widest px-2 py-1 mb-1" style={{ color: "var(--muted)" }}>
            {t("settings.title")}
          </p>
          {TABS.map(({ id, label }) => (
            <button
              key={id}
              onClick={() => setTab(id)}
              className={clsx(
                "w-full text-left px-2 py-1.5 rounded-lg text-sm transition-colors",
                tab === id
                  ? "font-semibold"
                  : "hover:opacity-80"
              )}
              style={{
                background: tab === id ? "rgba(255,255,255,0.07)" : "transparent",
                color: tab === id ? "var(--fg)" : "var(--muted)",
              }}
            >
              {label}
            </button>
          ))}

          <div className="flex-1" />

          <button
            onClick={() => setSession(null)}
            className="w-full text-left px-2 py-1.5 rounded-lg text-sm transition-colors hover:opacity-80"
            style={{ color: "var(--nexus-color-state-danger)" }}
          >
            {t("settings.danger.deleteAccount").includes("delete") ? "Sign out" : t("auth.signOut")}
          </button>
        </aside>

        {/* Main */}
        <div className="flex-1 flex flex-col min-w-0">
          {/* Header */}
          <div className="flex items-center justify-between px-6 py-4 border-b shrink-0" style={{ borderColor: "rgba(255,255,255,0.07)" }}>
            <h2 className="text-base font-bold" style={{ color: "var(--fg)" }}>
              {TABS.find(t => t.id === tab)?.label}
            </h2>
            <button
              onClick={onClose}
              className="w-7 h-7 rounded-lg flex items-center justify-center text-lg transition-colors hover:opacity-70"
              style={{ color: "var(--muted)" }}
              aria-label="Close settings"
            >
              ✕
            </button>
          </div>

          {/* Content */}
          <div className="flex-1 overflow-y-auto px-6 py-5 space-y-5">

            {/* ── Profile ─────────────────────────────────────────────────── */}
            {tab === "profile" && (
              <div className="space-y-4">
                {/* Avatar preview */}
                <div className="flex items-center gap-4">
                  <div
                    className="w-16 h-16 rounded-full flex items-center justify-center text-2xl font-bold shrink-0"
                    style={{ background: "rgba(124,58,237,0.2)", color: "var(--accent-400)" }}
                  >
                    {(session?.displayName ?? session?.username ?? "?")[0].toUpperCase()}
                  </div>
                  <div>
                    <p className="font-semibold" style={{ color: "var(--fg)" }}>
                      {session?.displayName ?? session?.username}
                    </p>
                    <p className="text-sm" style={{ color: "var(--muted)" }}>
                      @{session?.username}
                    </p>
                  </div>
                </div>

                <Field label={t("settings.displayName")}>
                  <input
                    className="nx-input w-full"
                    value={displayName}
                    onChange={(e) => setDisplayName(e.target.value)}
                    placeholder={session?.username ?? "Display name"}
                    maxLength={64}
                  />
                </Field>

                <Field label="Bio">
                  <textarea
                    className="nx-input w-full resize-none"
                    rows={3}
                    value={bio}
                    onChange={(e) => setBio(e.target.value)}
                    placeholder="Tell people a little about yourself"
                    maxLength={190}
                  />
                </Field>

                <Field label={t("settings.status.customStatus")}>
                  <input
                    className="nx-input w-full"
                    value={status}
                    onChange={(e) => setStatus(e.target.value)}
                    placeholder={t("settings.status.customStatusPlaceholder")}
                    maxLength={128}
                  />
                </Field>

                <Field label="Avatar URL">
                  <input
                    className="nx-input w-full"
                    value={avatar}
                    onChange={(e) => setAvatar(e.target.value)}
                    placeholder="https://example.com/avatar.png"
                    type="url"
                  />
                  <p className="text-xs mt-1" style={{ color: "var(--muted)" }}>Must be an HTTPS URL.</p>
                </Field>

                {profileMsg && (
                  <Alert ok={profileMsg.ok}>{profileMsg.text}</Alert>
                )}

                <button
                  className="nx-btn nx-btn-primary px-5 py-2 text-sm"
                  onClick={saveProfile}
                  disabled={profileSaving}
                >
                  {profileSaving ? t("settings.saving") : t("common.save")}
                </button>
              </div>
            )}

            {/* ── Security ────────────────────────────────────────────────── */}
            {tab === "security" && (
              <div className="space-y-4">
                <SectionTitle>Change Password</SectionTitle>

                <Field label="Current password">
                  <input
                    className="nx-input w-full"
                    type="password"
                    value={currentPw}
                    onChange={(e) => setCurrentPw(e.target.value)}
                    autoComplete="current-password"
                  />
                </Field>

                <Field label="New password">
                  <input
                    className="nx-input w-full"
                    type="password"
                    value={newPw}
                    onChange={(e) => setNewPw(e.target.value)}
                    autoComplete="new-password"
                  />
                </Field>

                <Field label="Confirm new password">
                  <input
                    className="nx-input w-full"
                    type="password"
                    value={confirmPw}
                    onChange={(e) => setConfirmPw(e.target.value)}
                    autoComplete="new-password"
                  />
                </Field>

                {pwMsg && <Alert ok={pwMsg.ok}>{pwMsg.text}</Alert>}

                <button
                  className="nx-btn nx-btn-primary px-5 py-2 text-sm"
                  onClick={savePassword}
                  disabled={pwSaving}
                >
                  {pwSaving ? "Saving…" : "Change Password"}
                </button>

                <div className="pt-4 border-t" style={{ borderColor: "rgba(255,255,255,0.07)" }}>
                  <SectionTitle>Post-Quantum Identity</SectionTitle>
                  <p className="text-sm mt-1 mb-3" style={{ color: "var(--muted)" }}>
                    Your Phantom identity uses ML-KEM-1024 + ML-DSA-87. Keys are generated in
                    your browser and never sent to the server.
                  </p>
                  <PhantomKeyManager />
                </div>

                <div className="pt-4 border-t" style={{ borderColor: "rgba(255,255,255,0.07)" }}>
                  <SectionTitle>Active Sessions</SectionTitle>
                  <p className="text-sm mt-1" style={{ color: "var(--muted)" }}>
                    You can sign out of all other sessions by changing your password.
                    Session management via the API is available at{" "}
                    <code className="text-xs px-1 py-0.5 rounded" style={{ background: "var(--bg-600)" }}>
                      GET /api/v1/users/@me/sessions
                    </code>.
                  </p>
                </div>

                <div className="pt-4 border-t" style={{ borderColor: "rgba(255,255,255,0.07)" }}>
                  <SectionTitle>Account Lifecycle</SectionTitle>
                  <p className="text-sm mt-1" style={{ color: "var(--muted)" }}>
                    Schedule your account for deletion. You can cancel it below.
                  </p>

                  <Field label="Password">
                    <input
                      className="nx-input w-full"
                      type="password"
                      value={deletePassword}
                      onChange={(e) => setDeletePassword(e.target.value)}
                      autoComplete="current-password"
                    />
                  </Field>

                  {deleteMsg && <Alert ok={deleteMsg.ok}>{deleteMsg.text}</Alert>}

                  <div className="flex gap-2">
                    <button
                      className="nx-btn nx-btn-primary px-5 py-2 text-sm"
                      onClick={requestDeletion}
                      disabled={deleteSaving}
                    >
                      {deleteSaving ? "Deleting…" : "Delete Account"}
                    </button>
                    <button
                      className="nx-btn nx-btn-secondary px-5 py-2 text-sm"
                      onClick={cancelDeletion}
                      disabled={deleteSaving}
                    >
                      {deleteSaving ? "Cancelling…" : "Cancel Deletion"}
                    </button>
                  </div>
                </div>
              </div>
            )}

            {/* ── Appearance ──────────────────────────────────────────────── */}
            {tab === "appearance" && (
              <div className="space-y-5">
                <Field label={t("settings.appearance.fontSize")}>
                  <div className="flex items-center gap-3">
                    <input
                      type="range"
                      min="12" max="18" step="1"
                      value={fontSize}
                      onChange={(e) => setFontSize(e.target.value)}
                      className="flex-1"
                    />
                    <span className="text-sm w-10 text-right" style={{ color: "var(--fg)" }}>
                      {fontSize}px
                    </span>
                  </div>
                </Field>

                <Field label={t("settings.appearance.density")}>
                  <div className="flex gap-2">
                    {(["compact", "cozy", "roomy"] as const).map((d) => (
                      <button
                        key={d}
                        onClick={() => setDensity(d)}
                        className={clsx(
                          "flex-1 py-2 rounded-lg text-sm font-medium border transition-colors capitalize",
                          density === d
                            ? "border-accent-500 text-accent-300"
                            : "border-transparent hover:border-bg-500"
                        )}
                        style={{
                          background: density === d ? "rgba(124,58,237,0.15)" : "var(--bg-600)",
                          color: density === d ? "var(--accent-300)" : "var(--muted)",
                          borderColor: density === d ? "var(--accent-500)" : "transparent",
                        }}
                      >
                        {t(`settings.appearance.${d}`)}
                      </button>
                    ))}
                  </div>
                </Field>

                <button
                  className="nx-btn nx-btn-primary px-5 py-2 text-sm"
                  onClick={applyAppearance}
                >
                  {t("common.apply")}
                </button>
              </div>
            )}

            {/* ── Connection ──────────────────────────────────────────────── */}
            {tab === "connection" && (
              <div className="space-y-4">
                <Row label={t("settings.connection.serverUrl")}>
                  <code className="text-xs px-2 py-1 rounded max-w-[260px] truncate block" style={{ background: "var(--bg-600)", color: "var(--fg)" }}>
                    {session?.serverUrl ?? "—"}
                  </code>
                </Row>

                <Row label="Gateway status">
                  <span className="flex items-center gap-1.5 text-sm font-semibold" style={{ color: gwColor }}>
                    <span className="inline-block w-2 h-2 rounded-full" style={{ background: gwColor }} />
                    {gatewayStatus}
                  </span>
                </Row>

                <Row label="Signed in as">
                  <span className="text-sm" style={{ color: "var(--fg)" }}>@{session?.username}</span>
                </Row>

                <div className="pt-4 border-t space-y-2" style={{ borderColor: "rgba(255,255,255,0.07)" }}>
                  <p className="text-xs" style={{ color: "var(--muted)" }}>
                    {t("settings.connection.changeServer")}
                  </p>
                </div>
              </div>
            )}
          </div>
        </div>
      </div>
    </div>
  );
}

// ── Helpers ───────────────────────────────────────────────────────────────────

function Field({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="space-y-1.5">
      <label className="block text-xs font-semibold uppercase tracking-wide" style={{ color: "var(--muted)" }}>
        {label}
      </label>
      {children}
    </div>
  );
}

function SectionTitle({ children }: { children: React.ReactNode }) {
  return (
    <h3 className="text-sm font-semibold" style={{ color: "var(--fg)" }}>
      {children}
    </h3>
  );
}

function Row({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="flex items-center justify-between py-3 border-b" style={{ borderColor: "rgba(255,255,255,0.05)" }}>
      <span className="text-sm" style={{ color: "var(--muted)" }}>{label}</span>
      {children}
    </div>
  );
}

function Alert({ ok, children }: { ok: boolean; children: React.ReactNode }) {
  return (
    <div
      className="px-3 py-2.5 rounded-lg text-sm"
      style={{
        background: ok ? "rgba(74,222,128,0.1)" : "rgba(248,113,113,0.1)",
        color: ok ? "var(--nexus-color-state-success)" : "var(--nexus-color-state-danger)",
        border: `1px solid ${ok ? "rgba(74,222,128,0.25)" : "rgba(248,113,113,0.25)"}`,
      }}
    >
      {children}
    </div>
  );
}
