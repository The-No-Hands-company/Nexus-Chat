import { useEffect, useRef, useState, lazy, Suspense, type KeyboardEvent, type FormEvent } from "react";
const UserPopover = lazy(() => import("./UserPopover"));
import { useParams } from "react-router-dom";
import clsx from "clsx";
import { format, isToday, isYesterday } from "date-fns";
import { useTranslation } from "react-i18next";
import { useStore, type NxMessage } from "../store";

export default function ChatView() {
  const { t } = useTranslation();
  const [popover, setPopover] = useState<{ userId: string; username: string; avatar: string | null; rect: DOMRect } | null>(null);
  const { channelId } = useParams<{ channelId: string }>();
  const {
    session,
    channels,
    messages,
    loadMessages,
    loadMoreMessages,
    sendMessage,
    deleteMessage,
    addReaction,
    markRead,
    typing,
  } = useStore();

  const channelMessages = channelId ? (messages[channelId] ?? []) : [];
  const channel = channels.find((c) => c.id === channelId);
  const typingUsers = channelId ? (typing[channelId] ?? []) : [];

  const [draft, setDraft] = useState("");
  const [replyTo, setReplyTo] = useState<NxMessage | null>(null);
  const [loadingMore, setLoadingMore] = useState(false);
  const [sending, setSending] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const bottomRef = useRef<HTMLDivElement>(null);
  const inputRef = useRef<HTMLTextAreaElement>(null);

  // Clear per-channel composer state whenever the channel changes.
  useEffect(() => {
    setReplyTo(null);
    setDraft("");
  }, [channelId]);

  /**
   * Load once we have both a channel and a session.
   *
   * `session` is a dependency, and that is the whole point. Since the SSO
   * cutover nobody logs in through this app: the store starts with
   * `session: null` on every boot and App.tsx fills it in from an async
   * bootstrapSession(). This effect used to depend only on `channelId`, so on
   * a full reload it ran first, hit `if (!s) return` inside loadMessages, and
   * was never retried once the session arrived — the channel rendered empty
   * and looked like the history had been lost. It had not; the fetch simply
   * never happened.
   */
  useEffect(() => {
    if (!channelId || !session) return;
    loadMessages(channelId);
    markRead(channelId);
  }, [channelId, session, loadMessages, markRead]);

  // Scroll to bottom on new messages
  useEffect(() => {
    bottomRef.current?.scrollIntoView({ behavior: "smooth" });
  }, [channelMessages.length]);

  async function handleSend(e?: FormEvent) {
    e?.preventDefault();
    if (!channelId || !draft.trim() || sending) return;
    setSending(true);
    setError(null);
    try {
      await sendMessage(channelId, draft.trim(), replyTo?.id);
      setDraft("");
      setReplyTo(null);
    } catch (err) {
      setError((err as Error).message);
    } finally {
      setSending(false);
    }
  }

  function handleKeyDown(e: KeyboardEvent<HTMLTextAreaElement>) {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      handleSend();
    }
    if (e.key === "Escape" && replyTo) {
      setReplyTo(null);
    }
  }

  async function handleLoadMore() {
    if (!channelId || loadingMore) return;
    setLoadingMore(true);
    try {
      await loadMoreMessages(channelId);
    } finally {
      setLoadingMore(false);
    }
  }

  if (!channelId) return null;

  return (
    <div className="flex flex-col flex-1 min-h-0 bg-bg-900">
      {/* Channel header */}
      <div className="flex items-center gap-2 px-4 py-2.5 border-b border-bg-600/40 shrink-0 bg-bg-800">
        <svg width="14" height="14" viewBox="0 0 24 24" fill="currentColor" className="text-muted shrink-0">
          <path d="M20 2H4c-1.1 0-2 .9-2 2v18l4-4h14c1.1 0 2-.9 2-2V4c0-1.1-.9-2-2-2z"/>
        </svg>
        <span className="font-semibold text-sm text-fg truncate">
          {channel?.name ?? channelId}
        </span>
        {channel?.topic && (
          <>
            <div className="w-px h-4 bg-bg-500 shrink-0" />
            <span className="text-xs text-muted truncate">{channel.topic}</span>
          </>
        )}
        {channel?.is_e2ee && (
          <span className="ml-auto text-[10px] text-green-400 font-medium shrink-0">
            🔒 E2EE
          </span>
        )}
      </div>

      {/* Message list */}
      <div className="flex-1 overflow-y-auto px-4 py-4 flex flex-col gap-0.5">
        {/* Load more */}
        {channelMessages.length >= 50 && (
          <div className="flex justify-center pb-2">
            <button
              onClick={handleLoadMore}
              disabled={loadingMore}
              className="text-xs text-muted hover:text-fg bg-bg-700 hover:bg-bg-600 rounded px-3 py-1 transition-colors disabled:opacity-50"
            >
              {loadingMore ? t("chat.loadingOlderMessages") : t("chat.loadOlderMessages")}
            </button>
          </div>
        )}

        {channelMessages.length === 0 && (
          <div className="flex-1 flex flex-col items-center justify-center text-center py-12">
            <div className="text-4xl mb-3">💬</div>
            <p className="text-sm font-semibold text-fg mb-1">
              #{channel?.name ?? "channel"}
            </p>
            <p className="text-xs text-muted">
              This is the beginning of the channel. Say hi!
            </p>
          </div>
        )}

        {/* Group messages by author + proximity */}
        {groupMessages(channelMessages).map((group) => (
          <MessageGroup
            key={group[0].id}
            messages={group}
            currentUserId={session?.userId ?? ""}
            onReply={setReplyTo}
            onDelete={async (id) => {
              if (!channelId) return;
              await deleteMessage(channelId, id);
            }}
            onReact={async (id, emoji) => {
              if (!channelId) return;
              await addReaction(channelId, id, emoji);
            }}
            onOpenProfile={(uid, uname, ava, rect) =>
              setPopover(p => p?.userId === uid ? null : { userId: uid, username: uname, avatar: ava, rect })
            }
          />
        ))}

        {/* Typing indicator */}
        {typingUsers.length > 0 && (
          <div className="text-xs text-muted italic px-1 pt-1">
            {typingUsers.length === 1
              ? t("chat.typing.one", { user: typingUsers[0] })
              : typingUsers.length === 2
              ? t("chat.typing.two", { user1: typingUsers[0], user2: typingUsers[1] })
              : t("chat.typing.many")}
          </div>
        )}

        <div ref={bottomRef} />
      </div>

      {/* Input area */}
      <div className="px-4 pb-4 pt-2 shrink-0">
        {/* Reply banner */}
        {replyTo && (
          <div className="flex items-center gap-2 mb-1.5 px-3 py-1.5 bg-bg-700 rounded-t text-xs text-muted border border-bg-600/40 border-b-0">
            <span className="flex-1 truncate">
              Replying to <span className="font-semibold text-fg">{replyTo.author_username}</span>
              {" — "}{replyTo.content.slice(0, 60)}
            </span>
            <button
              onClick={() => setReplyTo(null)}
              className="text-muted hover:text-fg shrink-0"
              aria-label={t("chat.cancelReply")}
            >
              ✕
            </button>
          </div>
        )}

        {error && (
          <div className="mb-1.5 text-xs text-red-400 bg-red-950/30 border border-red-800/40 rounded px-2 py-1">
            {error}
          </div>
        )}

        <form onSubmit={handleSend} className="flex items-end gap-2">
          <div className="flex-1 relative">
            <textarea
              ref={inputRef}
              value={draft}
              onChange={(e) => setDraft(e.target.value)}
              onKeyDown={handleKeyDown}
              placeholder={`Message #${channel?.name ?? "channel"}`}
              rows={1}
              disabled={sending}
              className={clsx(
                "w-full bg-bg-700 text-sm text-fg rounded px-3 py-2.5",
                "placeholder-muted/50 resize-none outline-none",
                "focus:ring-1 focus:ring-accent-500/60",
                "border border-bg-600/50 focus:border-accent-500/40",
                "transition-colors disabled:opacity-60",
                replyTo && "rounded-t-none border-t-0"
              )}
              style={{ maxHeight: 160 }}
              onInput={(e) => {
                const t = e.currentTarget;
                t.style.height = "auto";
                t.style.height = `${Math.min(t.scrollHeight, 160)}px`;
              }}
            />
          </div>
          <button
            type="submit"
            disabled={!draft.trim() || sending}
            className="px-3 py-2.5 bg-accent-500 hover:bg-accent-400 text-white rounded text-sm font-medium disabled:opacity-40 transition-colors shrink-0"
            aria-label={t("chat.sendMessage")}
          >
            <svg width="16" height="16" viewBox="0 0 24 24" fill="currentColor">
              <path d="M2.01 21L23 12 2.01 3 2 10l15 2-15 2z"/>
            </svg>
          </button>
        </form>
      </div>
      {popover && (
        <Suspense fallback={null}>
          <UserPopover
            userId={popover.userId}
            username={popover.username}
            avatar={popover.avatar}
            anchor={popover.rect}
            onClose={() => setPopover(null)}
          />
        </Suspense>
      )}
    </div>
  );
}

// ── Message grouping ──────────────────────────────────────────────────────────

function groupMessages(messages: NxMessage[]): NxMessage[][] {
  const groups: NxMessage[][] = [];
  for (const msg of [...messages].reverse()) {
    const last = groups[groups.length - 1];
    if (
      last &&
      last[last.length - 1].author_id === msg.author_id &&
      new Date(msg.created_at).getTime() -
        new Date(last[last.length - 1].created_at).getTime() <
        5 * 60 * 1000
    ) {
      last.push(msg);
    } else {
      groups.push([msg]);
    }
  }
  return groups;
}

function formatTimestamp(iso: string): string {
  const d = new Date(iso);
  if (isToday(d)) return `Today at ${format(d, "h:mm a")}`;
  if (isYesterday(d)) return `Yesterday at ${format(d, "h:mm a")}`;
  return format(d, "MM/dd/yyyy h:mm a");
}

// ── MessageGroup ──────────────────────────────────────────────────────────────

const QUICK_REACTIONS = ["👍", "❤️", "😂", "😮", "😢", "🎉"];

function MessageGroup({
  messages,
  currentUserId,
  onReply,
  onDelete,
  onReact,
  onOpenProfile,
}: {
  messages: NxMessage[];
  currentUserId: string;
  onReply: (m: NxMessage) => void;
  onDelete: (id: string) => Promise<void>;
  onReact: (id: string, emoji: string) => Promise<void>;
  onOpenProfile: (userId: string, username: string, avatar: string | null, rect: DOMRect) => void;
}) {
  const { t } = useTranslation();
  const first = messages[0];
  const isOwn = first.author_id === currentUserId;
  const [hoveredId, setHoveredId] = useState<string | null>(null);

  return (
    <div className="flex gap-3 group py-0.5 hover:bg-bg-800/40 rounded px-1 -mx-1">
      {/* Avatar */}
      <button
        type="button"
        onClick={(e) => onOpenProfile(first.author_id, first.author_username ?? "Unknown", first.author_avatar ?? null, e.currentTarget.getBoundingClientRect())}
        className="w-9 h-9 rounded-full bg-accent-500/30 flex items-center justify-center text-xs font-bold text-accent-300 shrink-0 mt-0.5 hover:ring-2 hover:ring-accent-400/60 transition-all cursor-pointer border-0 p-0"
        title="View profile"
      >
        {first.author_avatar ? (
          <img
            src={first.author_avatar}
            alt=""
            className="w-full h-full rounded-full object-cover"
          />
        ) : (
          (first.author_username ?? "?").slice(0, 2).toUpperCase()
        )}
      </button>

      {/* Messages */}
      <div className="flex-1 min-w-0">
        {/* Author + timestamp (shown on first msg in group) */}
        <div className="flex items-baseline gap-2 mb-0.5">
          <button
            type="button"
            onClick={(e) => onOpenProfile(first.author_id, first.author_username ?? "Unknown", first.author_avatar ?? null, e.currentTarget.getBoundingClientRect())}
            className={clsx("text-sm font-semibold hover:underline cursor-pointer bg-transparent border-0 p-0 text-left", isOwn ? "text-accent-400" : "text-fg")}
          >
            {first.author_username ?? "Unknown"}
          </button>
          <span className="text-[10px] text-muted">
            {formatTimestamp(first.created_at)}
          </span>
        </div>

        {messages.map((msg) => (
          <div
            key={msg.id}
            className="relative"
            onMouseEnter={() => setHoveredId(msg.id)}
            onMouseLeave={() => setHoveredId(null)}
          >
            <p className="text-sm text-fg leading-relaxed break-words whitespace-pre-wrap">
              {msg.content}
            </p>

            {/* Attachments */}
            {msg.attachments?.length > 0 && (
              <div className="flex flex-wrap gap-2 mt-1">
                {msg.attachments.map((att) =>
                  att.url && att.content_type?.startsWith("image/") ? (
                    <img
                      key={att.id}
                      src={att.url}
                      alt={att.filename}
                      className="rounded max-w-xs max-h-48 object-cover border border-bg-600/40"
                    />
                  ) : att.url ? (
                    <a
                      key={att.id}
                      href={att.url}
                      target="_blank"
                      rel="noopener noreferrer"
                      className="text-xs text-accent-400 hover:underline"
                    >
                      📎 {att.filename}
                    </a>
                  ) : null
                )}
              </div>
            )}

            {/* Reactions */}
            {msg.reactions?.length > 0 && (
              <div className="flex flex-wrap gap-1 mt-1">
                {msg.reactions.map((r) => (
                  <button
                    key={r.emoji}
                    onClick={() => onReact(msg.id, r.emoji)}
                    className={clsx(
                      "flex items-center gap-1 px-1.5 py-0.5 rounded text-xs border transition-colors",
                      r.me
                        ? "bg-accent-500/20 border-accent-500/40 text-accent-300"
                        : "bg-bg-700 border-bg-600/40 text-muted hover:bg-bg-600"
                    )}
                  >
                    <span>{r.emoji}</span>
                    <span>{r.count}</span>
                  </button>
                ))}
              </div>
            )}

            {/* Hover actions */}
            {hoveredId === msg.id && (
              <div className="absolute right-0 top-0 -translate-y-full flex items-center gap-1 bg-bg-700 border border-bg-600/40 rounded shadow-lg px-1 py-0.5 z-10">
                {QUICK_REACTIONS.map((e) => (
                  <button
                    key={e}
                    onClick={() => onReact(msg.id, e)}
                    className="text-sm hover:scale-125 transition-transform px-0.5"
                    title={e}
                  >
                    {e}
                  </button>
                ))}
                <div className="w-px h-4 bg-bg-500 mx-0.5" />
                <button
                  onClick={() => onReply(msg)}
                  className="text-[11px] text-muted hover:text-fg px-1 rounded hover:bg-bg-600 transition-colors"
                  title="Reply"
                >
                  ↩
                </button>
                {msg.author_id === currentUserId && (
                  <button
                    onClick={() => onDelete(msg.id)}
                    className="text-[11px] text-red-400 hover:text-red-300 px-1 rounded hover:bg-bg-600 transition-colors"
                    title="Delete"
                  >
                    🗑
                  </button>
                )}
              </div>
            )}

            {msg.edited_at && (
              <span className="text-[10px] text-muted/50 ml-1">{t("chat.edited")}</span>
            )}
          </div>
        ))}
      </div>
    </div>
  );
}
