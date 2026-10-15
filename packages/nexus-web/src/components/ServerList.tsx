import { useState } from "react";
import { useNavigate } from "react-router-dom";
import { useTranslation } from "react-i18next";
import { useStore } from "../store";
import clsx from "clsx";

interface Props {
  onOpenSettings?: () => void;
  onOpenJoin?: () => void;
}

export default function ServerList({ onOpenSettings, onOpenJoin }: Props) {
  const { t } = useTranslation();
  const {
    servers, activeServerId, setActiveServer, loadChannels,
    session, setSession, loadDms, gatewayStatus,
  } = useStore();
  const navigate = useNavigate();
  const [creating, setCreating] = useState(false);
  const [newName, setNewName] = useState("");
  const [homeMode, setHomeMode] = useState(false);

  const handleServer = (id: string) => {
    setHomeMode(false);
    setActiveServer(id);
    loadChannels(id);
    navigate("/");
  };

  const handleHome = () => {
    setHomeMode(true);
    setActiveServer(null);
    loadDms();
    navigate("/");
  };

  const handleCreate = async () => {
    if (!newName.trim()) return;
    try {
      const srv = await useStore.getState().createServer(newName.trim());
      setCreating(false);
      setNewName("");
      handleServer(srv.id);
    } catch (e) { console.error(e); }
  };

  const gwColor =
    gatewayStatus === "online" ? "var(--nexus-color-state-success)"
    : gatewayStatus === "connecting" ? "#facc15"
    : "var(--nexus-color-state-danger)";

  return (
    <nav
      aria-label={t("nav.servers")}
      className="w-14 bg-bg-900 border-r border-bg-600/40 flex flex-col items-center py-2 shrink-0 overflow-y-auto gap-1"
    >
      {/* Nexus brand mark */}
      <div className="w-10 h-10 flex items-center justify-center shrink-0 mb-0.5" aria-hidden="true">
        <svg width="18" height="18" viewBox="0 0 24 24" fill="currentColor" style={{ color: "#ccff00" }}>
          <path d="M13 2L3 14h9l-1 8 10-12h-9l1-8z" />
        </svg>
      </div>

      {/* Home / DMs */}
      <SpaceBtn active={homeMode} title={t("server.home")} onClick={handleHome}>
        <svg width="18" height="18" viewBox="0 0 24 24" fill="currentColor">
          <path d="M10 20v-6h4v6h5v-8h3L12 3 2 12h3v8z"/>
        </svg>
      </SpaceBtn>

      <Divider />

      {/* Server list */}
      {servers.map((srv) => (
        <SpaceBtn
          key={srv.id}
          active={activeServerId === srv.id}
          title={srv.name}
          onClick={() => handleServer(srv.id)}
        >
          {srv.icon ? (
            <img src={srv.icon} alt="" className="w-full h-full object-cover rounded-xl" />
          ) : (
            <span className="text-xs font-bold leading-none">
              {srv.name.slice(0, 2).toUpperCase()}
            </span>
          )}
        </SpaceBtn>
      ))}

      <div className="flex-1" />

      {/* Join server */}
      <SpaceBtn title="Join a server" onClick={() => onOpenJoin?.()}>
        <svg width="16" height="16" viewBox="0 0 24 24" fill="currentColor">
          <path d="M12 2C6.48 2 2 6.48 2 12s4.48 10 10 10 10-4.48 10-10S17.52 2 12 2zm5 11h-4v4h-2v-4H7v-2h4V7h2v4h4v2z"/>
        </svg>
      </SpaceBtn>

      {/* Create server */}
      {creating ? (
        <div className="w-10 flex flex-col gap-1">
          <input
            autoFocus
            className="nx-input text-[11px] px-1 py-1 text-center"
            value={newName}
            onChange={(e) => setNewName(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") handleCreate();
              if (e.key === "Escape") { setCreating(false); setNewName(""); }
            }}
            maxLength={50}
            placeholder={t("server.serverName")}
          />
        </div>
      ) : (
        <SpaceBtn title={t("server.createServer")} onClick={() => setCreating(true)}>
          <svg width="16" height="16" viewBox="0 0 24 24" fill="currentColor">
            <path d="M19 13h-6v6h-2v-6H5v-2h6V5h2v6h6v2z"/>
          </svg>
        </SpaceBtn>
      )}

      <Divider />

      {/* Gateway dot */}
      <div
        className="w-2 h-2 rounded-full my-0.5"
        style={{ background: gwColor }}
        title={`Gateway: ${gatewayStatus}`}
      />

      {/* User avatar / settings */}
      <button
        onClick={() => onOpenSettings?.()}
        title={`${session?.username} — Settings (Ctrl+,)`}
        className="w-8 h-8 rounded-full flex items-center justify-center text-xs font-bold hover:opacity-80 transition-opacity mt-0.5 relative"
        style={{ background: "rgba(124,58,237,0.25)", color: "var(--accent-300)" }}
      >
        {session?.username?.[0]?.toUpperCase() ?? "?"}
      </button>
    </nav>
  );
}

function Divider() {
  return <div className="w-6 h-px my-1 shrink-0" style={{ background: "var(--bg-600)" }} />;
}

function SpaceBtn({
  children, active, title, onClick,
}: {
  children: React.ReactNode;
  active?: boolean;
  title: string;
  onClick: () => void;
}) {
  return (
    <div className="relative w-full flex items-center justify-center group">
      <div className={clsx(
        "absolute left-0 top-1/2 -translate-y-1/2 w-0.5 rounded-r transition-all",
        active ? "h-6 bg-fg" : "h-2 bg-muted opacity-0 group-hover:opacity-100"
      )} />
      <button
        title={title}
        onClick={onClick}
        className={clsx(
          "w-10 h-10 rounded-xl flex items-center justify-center text-muted transition-all",
          "hover:rounded-lg hover:bg-accent-500/20 hover:text-accent-300",
          active && "!rounded-lg bg-accent-500/20 text-accent-300"
        )}
      >
        {children}
      </button>
    </div>
  );
}
