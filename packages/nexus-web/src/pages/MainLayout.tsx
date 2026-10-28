import { useEffect, useRef, useState, lazy, Suspense } from "react";
import { Routes, Route, useLocation } from "react-router-dom";
import clsx from "clsx";
import { useStore } from "../store";
import { useGateway } from "../gateway";
import { usePushNotifications } from "../hooks/usePushNotifications";
import ServerList from "../components/ServerList";
import { useVoice } from "../hooks/useVoice";
import ChannelList from "../components/ChannelList";
import ChatView from "../components/ChatView";
import MessagesLayout from "../components/MessagesLayout";
import { isEmbedded, shouldShowNotifBanner } from "../embed";

const SettingsPanel   = lazy(() => import("../components/SettingsPanel"));
const JoinServerModal = lazy(() => import("../components/JoinServerModal"));

export default function MainLayout() {
  const { session, loadServers, setSession } = useStore();
  const [showSettings, setShowSettings] = useState(false);
  const [notifDismissed, setNotifDismissed] = useState(
    () => localStorage.getItem("nx_notif_dismissed") === "1"
  );
  const showNotifBanner = shouldShowNotifBanner({
    embedded: isEmbedded(),
    dismissed: notifDismissed,
    supported: typeof Notification !== "undefined",
    hasSession: !!session,
    permissionDefault:
      typeof Notification !== "undefined" && Notification.permission === "default",
  });
  const voice = useVoice();
  const [showJoin, setShowJoin]     = useState(false);
  const [mobileNavOpen, setMobileNavOpen] = useState(false);

  const location = useLocation();
  const isMessaging = location.pathname.startsWith("/messages");

  useEffect(() => {
    setMobileNavOpen(false);
  }, [location.pathname]);

  // Real-time WebSocket gateway
  useGateway();

  // Push notification auto-subscribe
  const { enablePush } = usePushNotifications();
  useEffect(() => {
    if (!session) return;
    if (typeof Notification !== "undefined" && Notification.permission === "granted") {
      enablePush().catch(() => {});
    }
  }, [session, enablePush]);

  // Refresh access token every 10 minutes (TTL = 15 min)
  const refreshRef = useRef<ReturnType<typeof setInterval> | null>(null);
  useEffect(() => {
    const refresh = async () => {
      const { session: s } = useStore.getState();
      if (!s?.refreshToken) return;
      try {
        const res = await fetch(`${s.serverUrl}/api/v1/auth/refresh`, {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ refresh_token: s.refreshToken }),
        });
        if (!res.ok) return;
        const body = await res.json();
        if (typeof body.access_token === "string") {
          useStore.getState().setSession({
            ...s,
            accessToken: body.access_token,
            refreshToken: body.refresh_token ?? s.refreshToken,
          });
        }
      } catch { /* ignore */ }
    };
    refreshRef.current = setInterval(refresh, 10 * 60 * 1000);
    return () => { if (refreshRef.current) clearInterval(refreshRef.current); };
  }, [session?.refreshToken]);

  useEffect(() => { loadServers(); }, [loadServers]);

  // Keyboard shortcut: Ctrl+, opens Settings
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if ((e.ctrlKey || e.metaKey) && e.key === ",") {
        e.preventDefault();
        setShowSettings(true);
      }
      if (e.key === "Escape") {
        setShowSettings(false);
        setShowJoin(false);
        setMobileNavOpen(false);
      }
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, []);

  return (
    <div className="flex h-screen overflow-hidden bg-bg-900 text-fg relative">
      {/* Skip-to-main for keyboard navigation (WCAG 2.4.1) */}
      <a
        href="#main-content"
        className="sr-only focus:not-sr-only focus:absolute focus:z-50 focus:top-2 focus:left-2 focus:rounded focus:bg-bg-700 focus:px-4 focus:py-2 focus:text-fg focus:ring-2 focus:ring-accent-500 focus:outline-none"
      >
        Skip to main content
      </a>

      {/* Push notification permission banner */}
      {showNotifBanner && (
        <div
          role="dialog"
          aria-label="Enable notifications"
          className="fixed bottom-5 right-5 z-[300] w-[312px] rounded-[14px] border border-white/[0.09] bg-bg-700 p-[14px_16px] shadow-[0_8px_28px_rgba(0,0,0,0.45)] flex flex-col gap-[10px]"
        >
          <div className="flex items-start gap-[10px]">
            <span className="text-xl leading-none" aria-hidden="true">🔔</span>
            <div>
              <p className="text-[13px] font-bold text-fg m-0">Enable notifications</p>
              <p className="text-[12px] text-muted m-0 mt-1 leading-[1.5]">
                Get alerted for mentions and DMs even when the tab is in the background.
              </p>
            </div>
          </div>
          <div className="flex gap-[6px]">
            <button
              className="nx-btn nx-btn-primary flex-1 text-[12px] py-[7px] px-3"
              onClick={async () => {
                try { await enablePush(); } catch { /* ignore */ }
                localStorage.setItem("nx_notif_dismissed", "1");
                setNotifDismissed(true);
              }}
            >
              Enable
            </button>
            <button
              className="nx-btn nx-btn-ghost text-[12px] py-[7px] px-3"
              onClick={() => {
                localStorage.setItem("nx_notif_dismissed", "1");
                setNotifDismissed(true);
              }}
            >
              Not now
            </button>
          </div>
        </div>
      )}

      {/* Mobile backdrop — dismiss channel panel on tap outside */}
      {mobileNavOpen && (
        <div
          className="fixed inset-0 z-40 sm:hidden bg-black/50"
          onClick={() => setMobileNavOpen(false)}
          aria-hidden="true"
        />
      )}

      {/* Column 1 — server rail (always visible) */}
      <ServerList
        onOpenSettings={() => setShowSettings(true)}
        onOpenJoin={() => setShowJoin(true)}
      />

      {/* Columns 2+3 — messaging inbox OR discord channel layout */}
      {isMessaging ? (
        <Routes>
          <Route path="/messages" element={<MessagesLayout />} />
          <Route path="/messages/:channelId" element={<MessagesLayout />} />
        </Routes>
      ) : (
        <>
          {/* Column 2 — channel panel
              Mobile: fixed overlay starting at the rail edge (left-14 = 56px).
              Slides in/out via translateX. sm+: static layout column. */}
          <div
            id="channel-panel"
            className={clsx(
              "flex flex-col h-full",
              "fixed left-14 top-0 bottom-0 z-50 transition-transform duration-200 ease-out",
              "sm:static sm:z-auto sm:translate-x-0",
              mobileNavOpen ? "translate-x-0" : "-translate-x-full"
            )}
          >
            <ChannelList voice={voice} />
          </div>

          {/* Column 3 — main content */}
          <div id="main-content" role="main" className="flex flex-col flex-1 min-w-0">
            {/* Mobile hamburger: shows only on phone, opens channel panel */}
            <div className="flex items-center gap-2 px-3 py-2 border-b border-bg-600/40 shrink-0 sm:hidden bg-bg-800">
              <button
                type="button"
                onClick={() => setMobileNavOpen(true)}
                aria-label="Open channels"
                aria-controls="channel-panel"
                aria-expanded={mobileNavOpen}
                className="text-muted hover:text-fg p-1.5 rounded hover:bg-bg-600 transition-colors focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent-500"
              >
                <svg width="18" height="18" viewBox="0 0 24 24" fill="currentColor" aria-hidden="true">
                  <path d="M3 18h18v-2H3v2zm0-5h18v-2H3v2zm0-7v2h18V6H3z"/>
                </svg>
              </button>
              <span className="text-sm font-medium text-muted">Channels</span>
            </div>

            <Routes>
              <Route path="/" element={<EmptyState />} />
              <Route path="/channel/:channelId" element={<ChatView />} />
              <Route path="*" element={<EmptyState />} />
            </Routes>
          </div>
        </>
      )}

      {/* Modals */}
      <Suspense fallback={null}>
        {showSettings && <SettingsPanel onClose={() => setShowSettings(false)} />}
        {showJoin     && <JoinServerModal onClose={() => setShowJoin(false)} />}
      </Suspense>
    </div>
  );
}

function EmptyState() {
  useEffect(() => {
    document.title = "Nexus";
  }, []);

  return (
    <div className="flex-1 flex flex-col items-center justify-center gap-2 select-none">
      <span className="text-4xl opacity-20" aria-hidden="true">💬</span>
      <p className="text-sm" style={{ color: "var(--muted)" }}>
        Select a channel to start chatting
      </p>
      <p className="text-xs" style={{ color: "var(--muted)", opacity: 0.6 }}>
        Ctrl+, to open settings
      </p>
    </div>
  );
}
