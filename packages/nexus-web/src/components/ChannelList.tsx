import { useState, type KeyboardEvent } from "react";
import { useNavigate, useParams } from "react-router-dom";
import { useTranslation } from "react-i18next";
import clsx from "clsx";
import { useStore } from "../store";
import VoiceBar from "./VoiceBar";
import type { UseVoiceReturn } from "../hooks/useVoice";

export default function ChannelList({ voice }: { voice?: UseVoiceReturn }) {
  const { t } = useTranslation();
  const {
    channels, activeServerId, servers, activeChannelId, setActiveChannel,
    loadMessages, unread, loadDms, dmChannels, session, createChannel, createDm,
  } = useStore();

  const navigate = useNavigate();
  const joinVoice = voice?.joinVoice;
  const voiceChannelId = voice?.channelId;
  const { channelId } = useParams<{ channelId: string }>();
  const [creatingText, setCreatingText] = useState(false);
  const [newName, setNewName] = useState("");
  /**
   * Why this exists: creation used to fail with a 422 and the handler wrote it
   * to console.error, so the button looked simply dead. A failure the user
   * cannot see is indistinguishable from a feature that was never wired up.
   */
  const [createError, setCreateError] = useState<string | null>(null);
  const [composingDm, setComposingDm] = useState(false);
  const [dmTarget, setDmTarget] = useState("");

  const activeServer = servers.find((s) => s.id === activeServerId);
  const homeMode = !activeServerId;

  const textChannels = channels.filter(
    (c) => c.channel_type === "text" || c.channel_type === "announcement"
  );
  const voiceChannels = channels.filter((c) => c.channel_type === "voice");

  const handleChannel = (id: string) => {
    setActiveChannel(id);
    loadMessages(id);
    navigate(`/channel/${id}`);
  };

  const confirmCreate = async () => {
    if (!newName.trim()) {
      setCreateError(t("channel.nameRequired"));
      return;
    }
    if (!activeServerId) {
      // Silently returning here was the other half of the "dead button": with
      // no server selected there is nowhere to put a channel, and nothing said so.
      setCreateError(t("channel.selectServerFirst"));
      return;
    }
    setCreateError(null);
    try {
      const ch = await createChannel(activeServerId, newName.trim(), "text");
      setCreatingText(false);
      setNewName("");
      handleChannel(ch.id);
    } catch (e) {
      setCreateError(e instanceof Error ? e.message : String(e));
    }
  };

  const onKeyDown = (e: KeyboardEvent<HTMLInputElement>) => {
    if (e.key === "Enter") confirmCreate();
    if (e.key === "Escape") { setCreatingText(false); setNewName(""); setCreateError(null); }
  };

  // ── Home / DM mode ────────────────────────────────────────────────────────
  if (homeMode) {
    return (
      <nav
        aria-label={t("nav.directMessages")}
        className="w-56 bg-bg-800 flex flex-col shrink-0 border-r border-bg-600/40 overflow-hidden"
      >
        <div className="px-3 py-2 border-b border-bg-600/40 shrink-0 flex items-center gap-1">
          <span className="text-sm font-semibold flex-1">{t("nav.directMessages")}</span>
          <button
            type="button"
            onClick={() => { setComposingDm(true); setDmTarget(""); }}
            title="New direct message"
            className="w-5 h-5 flex items-center justify-center rounded text-muted hover:text-fg hover:bg-bg-600 transition-colors leading-none text-base"
          >+</button>
        </div>
        {composingDm && (
          <div className="px-2 pt-1.5 pb-1 border-b border-bg-600/40 shrink-0">
            <input
              autoFocus
              className="nx-input w-full text-xs py-1.5"
              placeholder="Paste a user ID, then press Enter"
              value={dmTarget}
              onChange={(e) => setDmTarget(e.target.value)}
              onKeyDown={async (e) => {
                if (e.key === "Escape") { setComposingDm(false); setDmTarget(""); }
                if (e.key === "Enter" && dmTarget.trim()) {
                  try {
                    const dm = await createDm(dmTarget.trim());
                    setActiveChannel(dm.id);
                    loadMessages(dm.id);
                    navigate(`/channel/${dm.id}`);
                    setComposingDm(false); setDmTarget("");
                  } catch { /* ignore */ }
                }
              }}
            />
          </div>
        )}
        <div className="flex-1 overflow-y-auto px-2 py-2 flex flex-col gap-px">
          {dmChannels.map((dm) => {
            const name =
              dm.name ??
              dm.recipients
                ?.filter((r) => r.id !== session?.userId)
                .map((r) => r.username)
                .join(", ") ??
              t("nav.directMessages");
            const initials = name.slice(0, 2).toUpperCase();
            const active = activeChannelId === dm.id || channelId === dm.id;
            return (
              <button
                key={dm.id}
                onClick={() => handleChannel(dm.id)}
                aria-label={name}
                aria-current={active ? "page" : undefined}
                className={clsx(
                  "flex items-center gap-2 px-2 py-1.5 rounded text-sm w-full text-left transition-colors",
                  active ? "bg-bg-600 text-fg" : "text-muted hover:bg-bg-700 hover:text-fg"
                )}
              >
                <div className="w-6 h-6 rounded-full bg-accent-500/30 flex items-center justify-center text-[9px] font-bold text-accent-300 shrink-0">
                  {initials}
                </div>
                <span className="truncate flex-1">{name}</span>
                {unread[dm.id] && (
                  <span className="w-2 h-2 rounded-full bg-white shrink-0" />
                )}
              </button>
            );
          })}
          {dmChannels.length === 0 && (
            <p className="text-xs text-muted/60 px-2 py-4 text-center">
              {t("chat.beginningOfDm", { name: "" }).replace(" with ", "")}
            </p>
          )}
        </div>
      </nav>
    );
  }

  // ── Server channel mode ───────────────────────────────────────────────────
  return (
    <nav
      aria-label={t("nav.channels")}
      className="w-56 bg-bg-800 flex flex-col shrink-0 border-r border-bg-600/40 overflow-hidden"
    >
      <div className="px-3 py-2.5 text-sm font-semibold border-b border-bg-600/40 shrink-0 truncate">
        {activeServer?.name ?? t("nav.channels")}
      </div>

      <div className="flex-1 overflow-y-auto px-2 py-2 flex flex-col gap-px">
        {/* Text channels */}
        <div className="flex items-center px-2 py-1 group">
          <span className="flex-1 text-[10px] text-muted/60 uppercase tracking-widest font-medium select-none">
            {t("nav.channels")}
          </span>
          <button
            onClick={() => setCreatingText((v) => !v)}
            className="opacity-0 group-hover:opacity-100 focus-visible:opacity-100 text-muted/60 hover:text-fg focus-visible:text-fg transition-all p-0.5 rounded focus-visible:outline-2 focus-visible:outline-offset-1 focus-visible:outline-accent-500"
            title={t("channel.createTextChannel")}
            aria-label={t("channel.createTextChannel")}
          >
            <PlusIcon />
          </button>
        </div>

        {textChannels.map((ch) => {
          const active = activeChannelId === ch.id || channelId === ch.id;
          return (
            <button
              key={ch.id}
              onClick={() => handleChannel(ch.id)}
              aria-label={t("channel.textChannel", { name: ch.name })}
              aria-current={active ? "page" : undefined}
              className={clsx(
                "flex items-center gap-2 px-2 py-1.5 rounded text-sm w-full text-left transition-colors",
                active ? "bg-bg-600 text-fg" : "text-muted hover:bg-bg-700 hover:text-fg"
              )}
            >
              <HashIcon />
              <span className="truncate flex-1">{ch.name}</span>
              {ch.is_e2ee && (
                <span className="text-[9px] text-green-400/80 shrink-0" title={t("channel.encrypted")}>E2E</span>
              )}
              {unread[ch.id] && (
                <span className="w-2 h-2 rounded-full bg-white shrink-0" aria-label={t("channel.unread")} />
              )}
            </button>
          );
        })}

        {creatingText && (
          <div className="px-1 py-1">
            <input
              autoFocus
              type="text"
              value={newName}
              onChange={(e) => setNewName(e.target.value)}
              onKeyDown={onKeyDown}
              placeholder={t("channel.channelName")}
              maxLength={100}
              className="w-full bg-bg-700 text-sm text-fg rounded px-2 py-1 outline-none focus:ring-1 focus:ring-accent-500 placeholder-muted/50"
            />
            {createError && (
              <p role="alert" className="mt-1 text-[11px] text-red-400">
                {createError}
              </p>
            )}
            <div className="flex gap-1 mt-1">
              <button
                onClick={confirmCreate}
                disabled={!newName.trim()}
                className="flex-1 text-[11px] bg-accent-500 hover:bg-accent-400 text-white rounded px-2 py-0.5 disabled:opacity-40 transition-colors"
              >
                {t("common.create")}
              </button>
              <button
                onClick={() => { setCreatingText(false); setNewName(""); setCreateError(null); }}
                className="flex-1 text-[11px] bg-bg-600 hover:bg-bg-500 text-muted rounded px-2 py-0.5 transition-colors"
              >
                {t("common.cancel")}
              </button>
            </div>
          </div>
        )}

        {/* Voice channels */}
        {voiceChannels.length > 0 && (
          <>
            <div className="h-px bg-bg-600/40 mx-2 my-1.5" />
            <div className="px-2 py-1">
              <span className="text-[10px] text-muted/60 uppercase tracking-widest font-medium select-none">
                {t("nav.voice")}
              </span>
            </div>
            {voiceChannels.map((ch) => {
              const active = activeChannelId === ch.id || channelId === ch.id;
              return (
                <button
                  key={ch.id}
                  onClick={() => joinVoice?.(ch.id)}
                  aria-label={t("channel.voiceChannel", { name: ch.name })}
                  aria-current={voiceChannelId === ch.id ? "page" : undefined}
                  className={clsx(
                    "flex items-center gap-2 px-2 py-1.5 rounded text-sm w-full text-left transition-colors",
                    voiceChannelId === ch.id
                      ? "bg-green-900/30 text-green-300"
                      : active ? "bg-bg-600 text-fg" : "text-muted hover:bg-bg-700 hover:text-fg"
                  )}
                >
                  <MicIcon />
                  <span className="truncate flex-1">{ch.name}</span>
                  {voiceChannelId === ch.id && (
                    <span className="text-[9px] font-bold text-green-400">LIVE</span>
                  )}
                </button>
              );
            })}
          </>
        )}
      </div>
      {voice && <VoiceBar voice={voice} />}
    </nav>
  );
}

function HashIcon() {
  return (
    <svg width="14" height="14" viewBox="0 0 24 24" fill="currentColor" className="shrink-0 opacity-60">
      <path d="M20 2H4c-1.1 0-2 .9-2 2v18l4-4h14c1.1 0 2-.9 2-2V4c0-1.1-.9-2-2-2zm-2 12H6v-2h12v2zm0-3H6V9h12v2zm0-3H6V6h12v2z"/>
    </svg>
  );
}

function MicIcon() {
  return (
    <svg width="14" height="14" viewBox="0 0 24 24" fill="currentColor" className="shrink-0 opacity-60">
      <path d="M12 15c1.66 0 2.99-1.34 2.99-3L15 6c0-1.66-1.34-3-3-3S9 4.34 9 6v6c0 1.66 1.34 3 3 3zm5.3-3c0 3-2.54 5.1-5.3 5.1S6.7 15 6.7 12H5c0 3.42 2.72 6.23 6 6.72V22h2v-3.28c3.28-.48 6-3.3 6-6.72h-1.7z"/>
    </svg>
  );
}

function PlusIcon() {
  return (
    <svg width="12" height="12" viewBox="0 0 24 24" fill="currentColor">
      <path d="M19 13h-6v6h-2v-6H5v-2h6V5h2v6h6v2z"/>
    </svg>
  );
}
