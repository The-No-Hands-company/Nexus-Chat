// ── UserPopover — profile card shown on username/avatar click in chat ─────────
//
// Rendered as a floating card anchored to the click target.
// Shows avatar, username, display name, presence, status message,
// and quick actions: Start DM, close.
//
// Usage: <UserPopover userId={id} username={name} anchor={domRect} onClose={fn} />

import { useEffect, useRef, useState } from "react";
import { useNavigate } from "react-router-dom";
import { useStore } from "../store";
import type { NxUser } from "../store";

const PRESENCE_COLOR: Record<string, string> = {
  online:    "var(--nexus-color-state-success)",
  idle:      "#facc15",
  dnd:       "var(--nexus-color-state-danger)",
  invisible: "#6b7280",
  offline:   "#6b7280",
};

const PRESENCE_LABEL: Record<string, string> = {
  online:    "Online",
  idle:      "Idle",
  dnd:       "Do Not Disturb",
  invisible: "Invisible",
  offline:   "Offline",
};

interface Props {
  userId: string;
  username: string;
  avatar: string | null;
  /** DOMRect of the element that was clicked — used to position the popover */
  anchor: DOMRect;
  onClose: () => void;
}

export default function UserPopover({ userId, username, avatar, anchor, onClose }: Props) {
  const { session, createDm, loadMessages, setActiveChannel, onlineUsers } = useStore();
  const navigate = useNavigate();
  const ref = useRef<HTMLDivElement>(null);

  const [profile, setProfile] = useState<NxUser | null>(null);
  const [startingDm, setStartingDm] = useState(false);

  // Fetch public profile
  useEffect(() => {
    const { session: s } = useStore.getState();
    if (!s) return;
    fetch(`${s.serverUrl}/api/v1/users/${userId}/profile`, {
      headers: { Authorization: `Bearer ${s.accessToken}` },
    })
      .then((r) => r.ok ? r.json() : null)
      .then((data) => data && setProfile(data.user ?? data))
      .catch(() => {});
  }, [userId]);

  // Close on outside click or Escape
  useEffect(() => {
    const handler = (e: MouseEvent | KeyboardEvent) => {
      if (e instanceof KeyboardEvent && e.key !== "Escape") return;
      if (e instanceof MouseEvent && ref.current?.contains(e.target as Node)) return;
      onClose();
    };
    document.addEventListener("mousedown", handler);
    document.addEventListener("keydown", handler);
    return () => {
      document.removeEventListener("mousedown", handler);
      document.removeEventListener("keydown", handler);
    };
  }, [onClose]);

  // Position: try right of anchor; flip left if would overflow viewport
  const cardW = 260;
  const cardH = 280;
  const vw = window.innerWidth;
  const vh = window.innerHeight;

  let left = anchor.right + 8;
  let top  = anchor.top;
  if (left + cardW > vw - 8) left = anchor.left - cardW - 8;
  if (top + cardH > vh - 8)  top  = vh - cardH - 8;
  top = Math.max(8, top);

  const isOwn = userId === session?.userId;
  const presence = (onlineUsers as Record<string, string>)[userId] ?? profile?.presence ?? "offline";
  const presenceColor = PRESENCE_COLOR[presence] ?? "#6b7280";

  async function openDm() {
    if (isOwn) { onClose(); return; }
    setStartingDm(true);
    try {
      const dm = await createDm(userId);
      setActiveChannel(dm.id);
      loadMessages(dm.id);
      navigate(`/channel/${dm.id}`);
      onClose();
    } catch (e) {
      console.error(e);
    } finally {
      setStartingDm(false);
    }
  }

  return (
    <div
      ref={ref}
      role="dialog"
      aria-label={`Profile: ${username}`}
      style={{
        position: "fixed",
        left,
        top,
        width: cardW,
        zIndex: 200,
        background: "var(--bg-700)",
        border: "1px solid rgba(255,255,255,0.08)",
        borderRadius: 12,
        boxShadow: "0 8px 32px rgba(0,0,0,0.5)",
        overflow: "hidden",
      }}
    >
      {/* Banner */}
      <div style={{
        height: 52,
        background: "linear-gradient(135deg, rgba(124,58,237,0.4), rgba(39,201,165,0.2))",
      }} />

      {/* Avatar (overlaps banner) */}
      <div style={{ position: "relative", paddingInline: 14, marginTop: -22 }}>
        <div style={{
          width: 52, height: 52, borderRadius: "50%",
          background: "rgba(124,58,237,0.25)",
          border: "3px solid var(--bg-700)",
          display: "flex", alignItems: "center", justifyContent: "center",
          fontSize: 20, fontWeight: 800, color: "var(--accent-300)",
          overflow: "hidden", flexShrink: 0,
        }}>
          {avatar ? (
            <img src={avatar} alt="" style={{ width: "100%", height: "100%", objectFit: "cover" }} />
          ) : (
            (profile?.display_name ?? username).slice(0, 1).toUpperCase()
          )}
        </div>

        {/* Presence dot */}
        <span style={{
          position: "absolute", bottom: 2, left: 48,
          width: 14, height: 14, borderRadius: "50%",
          background: presenceColor,
          border: "2px solid var(--bg-700)",
        }} />
      </div>

      {/* Info */}
      <div style={{ padding: "8px 14px 14px" }}>
        <p style={{ fontSize: 16, fontWeight: 700, color: "var(--fg)", marginBottom: 2 }}>
          {profile?.display_name ?? username}
        </p>
        <p style={{ fontSize: 12, color: "var(--muted)", marginBottom: 2 }}>
          @{username}
        </p>
        <p style={{ fontSize: 12, color: presenceColor, marginBottom: 8 }}>
          {PRESENCE_LABEL[presence] ?? "Offline"}
        </p>

        {profile?.status && (
          <p style={{
            fontSize: 12, color: "var(--muted)",
            padding: "6px 8px", background: "var(--bg-600)",
            borderRadius: 6, marginBottom: 10,
            fontStyle: "italic",
          }}>
            {profile.status}
          </p>
        )}

        {/* Actions */}
        <div style={{ display: "flex", gap: 6 }}>
          {!isOwn && (
            <button
              onClick={openDm}
              disabled={startingDm}
              style={{
                flex: 1, padding: "7px 12px", borderRadius: 8,
                background: "var(--accent-500)", color: "#fff",
                fontSize: 12, fontWeight: 600, cursor: "pointer",
                border: "none", opacity: startingDm ? 0.6 : 1,
                transition: "opacity 0.15s",
              }}
            >
              {startingDm ? "Opening…" : "💬 Message"}
            </button>
          )}
          <button
            onClick={onClose}
            style={{
              padding: "7px 10px", borderRadius: 8,
              background: "var(--bg-600)", color: "var(--muted)",
              fontSize: 12, cursor: "pointer", border: "none",
            }}
          >
            ✕
          </button>
        </div>
      </div>
    </div>
  );
}
