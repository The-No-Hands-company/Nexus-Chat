import { useEffect, useState, useCallback, useRef } from "react";
import { useNavigate, useParams } from "react-router-dom";
import clsx from "clsx";
import { useStore, type NxChannel, type NxUserSearchResult } from "../store";
import ChatView from "./ChatView";

function formatTime(iso: string): string {
  const d = new Date(iso);
  const diff = Date.now() - d.getTime();
  if (diff < 60_000) return "now";
  if (diff < 3_600_000) return `${Math.floor(diff / 60_000)}m`;
  if (diff < 86_400_000) return `${Math.floor(diff / 3_600_000)}h`;
  const days = Math.floor(diff / 86_400_000);
  if (days < 7) return d.toLocaleDateString(undefined, { weekday: "short" });
  return d.toLocaleDateString(undefined, { month: "short", day: "numeric" });
}

function dmName(dm: NxChannel, myId?: string): string {
  return (
    dm.name ??
    dm.recipients
      ?.filter((r) => r.id !== myId)
      .map((r) => r.username)
      .join(", ") ??
    "Direct Message"
  );
}

function recipientAvatar(dm: NxChannel, myId?: string): string | null {
  return dm.recipients?.find((r) => r.id !== myId)?.avatar ?? null;
}

interface AvatarProps {
  name: string;
  src: string | null;
  size?: "sm" | "md";
}

function Avatar({ name, src, size = "md" }: AvatarProps) {
  const initials = name.slice(0, 2).toUpperCase();
  const cls =
    size === "sm"
      ? "w-6 h-6 text-[9px]"
      : "w-10 h-10 text-[11px]";
  if (src) {
    return (
      <img
        src={src}
        alt={name}
        className={`${cls} rounded-full object-cover shrink-0`}
      />
    );
  }
  return (
    <div
      className={`${cls} rounded-full bg-accent-500/25 flex items-center justify-center text-accent-300 font-bold shrink-0 select-none`}
    >
      {initials}
    </div>
  );
}

export default function MessagesLayout() {
  const { channelId } = useParams<{ channelId: string }>();
  const navigate = useNavigate();
  const {
    session,
    dmChannels,
    messages,
    unread,
    loadDms,
    loadMessages,
    setActiveChannel,
    createDm,
    searchUsers,
  } = useStore();

  const [composing, setComposing] = useState(false);
  const [query, setQuery] = useState("");
  const [results, setResults] = useState<NxUserSearchResult[]>([]);
  const [fetchingResults, setFetchingResults] = useState(false);
  const debounceRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  useEffect(() => {
    loadDms();
  }, [loadDms]);

  useEffect(() => {
    if (channelId) {
      setActiveChannel(channelId);
      loadMessages(channelId);
    }
  }, [channelId, setActiveChannel, loadMessages]);

  // Debounced user search
  useEffect(() => {
    if (!composing || !query.trim()) {
      setResults([]);
      return;
    }
    if (debounceRef.current) clearTimeout(debounceRef.current);
    debounceRef.current = setTimeout(async () => {
      setFetchingResults(true);
      try {
        setResults(await searchUsers(query.trim()));
      } finally {
        setFetchingResults(false);
      }
    }, 300);
    return () => {
      if (debounceRef.current) clearTimeout(debounceRef.current);
    };
  }, [query, composing, searchUsers]);

  const openDm = useCallback(
    async (userId: string) => {
      setComposing(false);
      setQuery("");
      setResults([]);
      const existing = dmChannels.find((d) =>
        d.recipients?.some((r) => r.id === userId && r.id !== session?.userId),
      );
      const target = existing ?? (await createDm(userId).catch(() => null));
      if (target) navigate(`/messages/${target.id}`);
    },
    [dmChannels, createDm, navigate, session?.userId],
  );

  const closeCompose = useCallback(() => {
    setComposing(false);
    setQuery("");
    setResults([]);
  }, []);

  // Sort conversations by last message time, most recent first
  const sortedDms = [...dmChannels].sort((a, b) => {
    const aArr = messages[a.id];
    const bArr = messages[b.id];
    const at = aArr && aArr.length > 0 ? aArr[aArr.length - 1].created_at : undefined;
    const bt = bArr && bArr.length > 0 ? bArr[bArr.length - 1].created_at : undefined;
    if (!at && !bt) return 0;
    if (!at) return 1;
    if (!bt) return -1;
    return new Date(bt).getTime() - new Date(at).getTime();
  });

  return (
    <div className="flex flex-1 min-w-0 h-full overflow-hidden">
      {/* ── Conversation list ── */}
      <aside
        aria-label="Conversations"
        className="w-72 shrink-0 flex flex-col bg-bg-800 border-r border-bg-600/40 overflow-hidden"
      >
        {/* Header */}
        <div className="px-4 py-3 border-b border-bg-600/40 flex items-center gap-2 shrink-0">
          <span className="flex-1 text-sm font-semibold tracking-wide">Messages</span>
          <button
            type="button"
            title={composing ? "Cancel" : "New conversation"}
            aria-label={composing ? "Cancel new conversation" : "New conversation"}
            onClick={() => (composing ? closeCompose() : setComposing(true))}
            className={clsx(
              "w-7 h-7 flex items-center justify-center rounded-md transition-colors focus-visible:outline-2 focus-visible:outline-offset-1 focus-visible:outline-accent-500",
              composing
                ? "bg-accent-500/20 text-accent-300"
                : "text-muted hover:text-fg hover:bg-bg-600",
            )}
          >
            <PencilIcon />
          </button>
        </div>

        {/* New conversation search */}
        {composing && (
          <div className="px-3 pt-2 pb-2 border-b border-bg-600/40 shrink-0">
            <input
              autoFocus
              className="nx-input w-full text-sm py-1.5"
              placeholder="Search by username…"
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Escape") closeCompose();
              }}
            />
            {query.trim() && (
              <ul className="mt-1.5 rounded border border-bg-600/60 bg-bg-700 divide-y divide-bg-600/40 overflow-hidden">
                {results.map((u) => (
                  <li key={u.id}>
                    <button
                      type="button"
                      className="w-full flex items-center gap-2.5 px-3 py-2.5 hover:bg-bg-600 text-left transition-colors focus-visible:outline-none focus-visible:bg-bg-600"
                      onClick={() => openDm(u.id)}
                    >
                      <Avatar name={u.username} src={u.avatar} size="sm" />
                      <div className="flex flex-col min-w-0">
                        <span className="text-sm font-medium text-fg truncate">
                          {u.display_name ?? u.username}
                        </span>
                        {u.display_name && (
                          <span className="text-xs text-muted/70">@{u.username}</span>
                        )}
                      </div>
                    </button>
                  </li>
                ))}
                {!fetchingResults && results.length === 0 && (
                  <li className="px-3 py-3 text-xs text-muted/60 text-center">
                    No users found
                  </li>
                )}
              </ul>
            )}
          </div>
        )}

        {/* Conversations */}
        <div className="flex-1 overflow-y-auto" role="list">
          {sortedDms.length === 0 && (
            <div
              role="listitem"
              className="flex flex-col items-center justify-center gap-2 py-16 select-none"
            >
              <BubbleIcon size={40} />
              <p className="text-xs text-muted/50 text-center px-6 leading-relaxed">
                No conversations yet.
                <br />
                Tap the pencil to start one.
              </p>
            </div>
          )}

          {sortedDms.map((dm) => {
            const name = dmName(dm, session?.userId);
            const avatar = recipientAvatar(dm, session?.userId);
            const dmMsgs = messages[dm.id];
            const lastMsg = dmMsgs && dmMsgs.length > 0 ? dmMsgs[dmMsgs.length - 1] : undefined;
            const active = channelId === dm.id;
            const hasUnread = !!unread[dm.id];

            return (
              <button
                key={dm.id}
                type="button"
                role="listitem"
                aria-label={name}
                aria-current={active ? "page" : undefined}
                onClick={() => navigate(`/messages/${dm.id}`)}
                className={clsx(
                  "w-full flex items-center gap-3 px-4 py-3 text-left transition-colors focus-visible:outline-none focus-visible:bg-bg-600/80",
                  active ? "bg-bg-600/70" : "hover:bg-bg-700/50",
                )}
              >
                <div className="relative shrink-0">
                  <Avatar name={name} src={avatar} />
                  {hasUnread && (
                    <span
                      aria-label="Unread messages"
                      className="absolute -top-0.5 -right-0.5 w-3 h-3 rounded-full bg-accent-500 border-2 border-bg-800"
                    />
                  )}
                </div>

                <div className="flex flex-col min-w-0 flex-1 gap-0.5">
                  <div className="flex items-baseline gap-1">
                    <span
                      className={clsx(
                        "text-sm truncate",
                        hasUnread ? "font-semibold text-fg" : "font-medium text-fg/80",
                      )}
                    >
                      {name}
                    </span>
                    {lastMsg && (
                      <span className="text-[10px] text-muted/50 shrink-0 ml-auto tabular-nums">
                        {formatTime(lastMsg.created_at)}
                      </span>
                    )}
                  </div>

                  {lastMsg ? (
                    <span
                      className={clsx(
                        "text-xs truncate leading-tight",
                        hasUnread ? "text-muted font-medium" : "text-muted/50",
                      )}
                    >
                      {lastMsg.author_id === session?.userId && (
                        <span className="opacity-60">You: </span>
                      )}
                      {lastMsg.content ||
                        (lastMsg.attachments.length > 0 ? "📎 Attachment" : "")}
                    </span>
                  ) : (
                    <span className="text-xs text-muted/30 italic">No messages yet</span>
                  )}
                </div>
              </button>
            );
          })}
        </div>
      </aside>

      {/* ── Chat area ── */}
      <main className="flex flex-col flex-1 min-w-0 bg-bg-900">
        {channelId ? (
          <ChatView />
        ) : (
          <div className="flex-1 flex flex-col items-center justify-center gap-3 select-none">
            <BubbleIcon size={56} />
            <p className="text-sm text-muted/60">Select a conversation</p>
            <p className="text-xs text-muted/40">or tap the pencil to start a new one</p>
          </div>
        )}
      </main>
    </div>
  );
}

function PencilIcon() {
  return (
    <svg width="14" height="14" viewBox="0 0 24 24" fill="currentColor" aria-hidden="true">
      <path d="M3 17.25V21h3.75L17.81 9.94l-3.75-3.75L3 17.25zM20.71 7.04c.39-.39.39-1.02 0-1.41l-2.34-2.34c-.39-.39-1.02-.39-1.41 0l-1.83 1.83 3.75 3.75 1.83-1.83z" />
    </svg>
  );
}

function BubbleIcon({ size }: { size: number }) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="currentColor"
      className="opacity-[0.12]"
      aria-hidden="true"
    >
      <path d="M20 2H4c-1.1 0-2 .9-2 2v18l4-4h14c1.1 0 2-.9 2-2V4c0-1.1-.9-2-2-2z" />
    </svg>
  );
}
