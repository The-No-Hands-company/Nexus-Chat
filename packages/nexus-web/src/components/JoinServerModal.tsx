import { useState } from "react";
import { useStore } from "../store";

export default function JoinServerModal({ onClose }: { onClose: () => void }) {
  const { joinServer, loadChannels, setActiveServer } = useStore();
  const [code, setCode] = useState("");
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function submit() {
    const c = code.trim();
    if (!c) return;
    setLoading(true);
    setError(null);
    try {
      const srv = await joinServer(c);
      setActiveServer(srv.id);
      loadChannels(srv.id);
      onClose();
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setLoading(false);
    }
  }

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/60 backdrop-blur-sm"
      onClick={(e) => e.target === e.currentTarget && onClose()}
    >
      <div
        className="w-full max-w-sm rounded-2xl p-6 shadow-2xl"
        style={{ background: "var(--bg-800)", border: "1px solid rgba(255,255,255,0.07)" }}
      >
        <h2 className="text-lg font-bold mb-1" style={{ color: "var(--fg)" }}>
          Join a Server
        </h2>
        <p className="text-sm mb-4" style={{ color: "var(--muted)" }}>
          Enter an invite code to join an existing server.
        </p>

        <input
          className="nx-input w-full mb-3"
          value={code}
          onChange={(e) => setCode(e.target.value)}
          onKeyDown={(e) => e.key === "Enter" && submit()}
          placeholder="Invite code (e.g. ABC12DEF34GH)"
          autoFocus
          autoCapitalize="none"
          autoCorrect="off"
          spellCheck={false}
          maxLength={24}
        />

        {error && (
          <p className="text-sm mb-3 px-3 py-2 rounded-lg" style={{ background: "rgba(248,113,113,0.1)", color: "var(--nexus-color-state-danger)" }}>
            {error}
          </p>
        )}

        <div className="flex justify-end gap-2">
          <button className="nx-btn nx-btn-ghost text-sm px-4 py-2" onClick={onClose}>
            Cancel
          </button>
          <button
            className="nx-btn nx-btn-primary text-sm px-4 py-2"
            onClick={submit}
            disabled={loading || !code.trim()}
          >
            {loading ? "Joining…" : "Join Server"}
          </button>
        </div>
      </div>
    </div>
  );
}
