// ── VoiceBar — persistent voice status bar ───────────────────────────────────
//
// Rendered at the bottom of the channel list column whenever the user is in
// a voice channel. Shows: channel name, connection status, participant count,
// mute/deafen toggles, and a leave button.

import clsx from "clsx";
import type { UseVoiceReturn } from "../hooks/useVoice";
import { useStore } from "../store";

interface Props {
  voice: UseVoiceReturn;
}

export default function VoiceBar({ voice }: Props) {
  const { channels } = useStore();
  const { status, channelId, participants, muted, deafened, leaveVoice, toggleMute, toggleDeafen } = voice;

  if (status === "idle") return null;

  const channel = channels.find((c) => c.id === channelId);
  const channelName = channel?.name ?? "Voice";

  const statusColor =
    status === "connected"  ? "var(--nexus-color-state-success)"
    : status === "connecting" ? "#facc15"
    : "var(--nexus-color-state-danger)";

  return (
    <div
      className="shrink-0 border-t"
      style={{
        background: "var(--bg-900)",
        borderColor: "rgba(74,222,128,0.15)",
        borderTopColor: statusColor + "40",
      }}
    >
      {/* Status row */}
      <div className="flex items-center gap-1.5 px-3 pt-2 pb-1">
        <span
          className="inline-block w-2 h-2 rounded-full shrink-0"
          style={{ background: statusColor }}
        />
        <span className="text-xs font-semibold truncate flex-1" style={{ color: statusColor }}>
          {status === "connected"
            ? `🔊 ${channelName}`
            : status === "connecting"
            ? "Connecting…"
            : "Voice error"}
        </span>
        {status === "connected" && (
          <span className="text-[10px]" style={{ color: "var(--muted)" }}>
            {participants.length} {participants.length === 1 ? "user" : "users"}
          </span>
        )}
      </div>

      {/* Controls */}
      <div className="flex items-center gap-1 px-2 pb-2">
        {/* Mute */}
        <VoiceBtn
          title={muted ? "Unmute" : "Mute"}
          active={muted}
          activeColor="var(--nexus-color-state-danger)"
          onClick={toggleMute}
        >
          {muted ? (
            <svg width="14" height="14" viewBox="0 0 24 24" fill="currentColor">
              <path d="M19 11h-1.7c0 .74-.16 1.43-.43 2.05l1.23 1.23c.56-.98.9-2.09.9-3.28zm-4.02.17c0-.06.02-.11.02-.17V5c0-1.66-1.34-3-3-3S9 3.34 9 5v.18l5.98 5.99zM4.27 3L3 4.27l6.01 6.01V11c0 1.66 1.33 3 2.99 3 .22 0 .44-.03.65-.08l1.66 1.66c-.71.33-1.5.52-2.31.52-2.76 0-5.3-2.1-5.3-5.1H5c0 3.41 2.72 6.23 6 6.72V21h2v-3.28c.91-.13 1.77-.45 2.54-.9L19.73 21 21 19.73 4.27 3z"/>
            </svg>
          ) : (
            <svg width="14" height="14" viewBox="0 0 24 24" fill="currentColor">
              <path d="M12 14c1.66 0 2.99-1.34 2.99-3L15 5c0-1.66-1.34-3-3-3S9 3.34 9 5v6c0 1.66 1.34 3 3 3zm5.3-3c0 3-2.54 5.1-5.3 5.1S6.7 14 6.7 11H5c0 3.41 2.72 6.23 6 6.72V21h2v-3.28c3.28-.48 6-3.3 6-6.72h-1.7z"/>
            </svg>
          )}
        </VoiceBtn>

        {/* Deafen */}
        <VoiceBtn
          title={deafened ? "Undeafen" : "Deafen"}
          active={deafened}
          activeColor="var(--nexus-color-state-danger)"
          onClick={toggleDeafen}
        >
          {deafened ? (
            <svg width="14" height="14" viewBox="0 0 24 24" fill="currentColor">
              <path d="M12 2C6.48 2 2 6.48 2 12s4.48 10 10 10 10-4.48 10-10S17.52 2 12 2zm1 15h-2v-6h2v6zm0-8h-2V7h2v2z"/>
            </svg>
          ) : (
            <svg width="14" height="14" viewBox="0 0 24 24" fill="currentColor">
              <path d="M3 9v6h4l5 5V4L7 9H3zm13.5 3c0-1.77-1.02-3.29-2.5-4.03v8.05c1.48-.73 2.5-2.25 2.5-4.02z"/>
            </svg>
          )}
        </VoiceBtn>

        <div className="flex-1" />

        {/* Leave */}
        <button
          onClick={leaveVoice}
          title="Disconnect from voice"
          className="flex items-center gap-1 px-2 py-1 rounded text-xs font-semibold transition-colors hover:bg-red-900/30"
          style={{ color: "var(--nexus-color-state-danger)" }}
        >
          <svg width="12" height="12" viewBox="0 0 24 24" fill="currentColor">
            <path d="M17 7l-1.41 1.41L18.17 11H8v2h10.17l-2.58 2.58L17 17l5-5zM4 5h8V3H4c-1.1 0-2 .9-2 2v14c0 1.1.9 2 2 2h8v-2H4V5z"/>
          </svg>
          Leave
        </button>
      </div>

      {/* Participants */}
      {participants.length > 0 && status === "connected" && (
        <div className="px-3 pb-2 flex flex-wrap gap-1">
          {participants.map((p) => (
            <div
              key={p.user_id}
              className="flex items-center gap-1 px-1.5 py-0.5 rounded text-[10px]"
              style={{ background: "var(--bg-700)", color: p.muted ? "var(--muted)" : "var(--fg)" }}
            >
              <span
                className="inline-block w-1.5 h-1.5 rounded-full shrink-0"
                style={{ background: p.muted ? "var(--muted)" : "var(--nexus-color-state-success)" }}
              />
              <span className="truncate max-w-[80px]">{p.username}</span>
              {p.deafened && <span title="Deafened" className="opacity-50">🔇</span>}
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

function VoiceBtn({
  children, title, active, activeColor, onClick,
}: {
  children: React.ReactNode;
  title: string;
  active: boolean;
  activeColor: string;
  onClick: () => void;
}) {
  return (
    <button
      title={title}
      onClick={onClick}
      className="w-7 h-7 rounded flex items-center justify-center transition-colors"
      style={{
        background: active ? activeColor + "22" : "var(--bg-700)",
        color: active ? activeColor : "var(--muted)",
      }}
    >
      {children}
    </button>
  );
}
