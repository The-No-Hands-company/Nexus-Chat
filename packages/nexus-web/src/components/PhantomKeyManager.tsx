import { useState, useEffect } from "react";
import { useTranslation } from "react-i18next";
import {
  generateAndStore, hasStoredKeys, exportKeys, clearKeys,
  type PhantomPublicKeys,
} from "../lib/phantomKeys";
import { useStore } from "../store";

const BASE = import.meta.env.VITE_API_BASE ?? "";

async function registerWithServer(keys: PhantomPublicKeys): Promise<void> {
  const r = await fetch(`${BASE}/api/users/@me/phantom`, {
    method: "POST",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ kem_public: keys.kemPublic, signing_public: keys.signingPublic }),
  });
  if (!r.ok) throw new Error(`Server registration failed: ${r.status}`);
}

type Step = "idle" | "generating" | "confirm" | "done" | "error";

export default function PhantomKeyManager() {
  const { t } = useTranslation();
  const session = useStore((s) => s.session);

  const [hasKeys, setHasKeys]   = useState(false);
  const [step, setStep]         = useState<Step>("idle");
  const [password, setPassword] = useState("");
  const [confirm, setConfirm]   = useState("");
  const [error, setError]       = useState<string | null>(null);
  const [exporting, setExporting] = useState(false);

  useEffect(() => {
    hasStoredKeys().then(setHasKeys).catch(() => setHasKeys(false));
  }, []);

  const generate = async () => {
    if (!password || password !== confirm) {
      setError("Passwords must match.");
      return;
    }
    setStep("generating");
    setError(null);
    try {
      const keys = await generateAndStore(password);
      await registerWithServer(keys);
      setHasKeys(true);
      setStep("done");
      setPassword("");
      setConfirm("");
    } catch (e) {
      setError(e instanceof Error ? e.message : "Key generation failed");
      setStep("error");
    }
  };

  const doExport = async () => {
    if (!password) { setError("Enter your key password to export."); return; }
    setExporting(true);
    setError(null);
    try {
      const json = await exportKeys(password);
      const blob = new Blob([json], { type: "application/json" });
      const url = URL.createObjectURL(blob);
      const a = document.createElement("a");
      a.href = url;
      a.download = `phantom-keys-${session?.userId ?? "user"}.json`;
      a.click();
      URL.revokeObjectURL(url);
    } catch (e) {
      setError(e instanceof Error ? e.message : "Export failed — check your password.");
    } finally {
      setExporting(false);
    }
  };

  const doDelete = async () => {
    await clearKeys();
    setHasKeys(false);
    setStep("idle");
    setPassword("");
    setConfirm("");
    setError(null);
  };

  return (
    <div className="flex flex-col gap-3">
      <div className="flex items-start gap-3 rounded-lg bg-bg-700 border border-bg-600/40 px-4 py-3">
        <div className="mt-0.5 shrink-0">
          <div
            className={`w-2.5 h-2.5 rounded-full ${hasKeys ? "bg-state-success" : "bg-bg-500"}`}
            title={hasKeys ? "Keys present on this device" : "No keys on this device"}
          />
        </div>
        <div className="flex-1 min-w-0">
          <p className="text-sm font-semibold text-fg">Post-quantum identity</p>
          <p className="text-xs text-muted mt-0.5">
            {hasKeys
              ? "ML-KEM-1024 + ML-DSA-87 keys stored on this device. Private keys never leave your browser."
              : "Generate a post-quantum identity. Keys are created here; only the public key is sent to the server."}
          </p>
        </div>
      </div>

      {error && (
        <p role="alert" className="text-xs text-state-danger px-1">{error}</p>
      )}

      {!hasKeys && step !== "done" && (
        <div className="flex flex-col gap-2">
          <p className="text-xs text-muted">
            Choose a password to protect your private key on this device.
          </p>
          <input
            type="password"
            className="nx-input text-sm"
            placeholder="Key password"
            value={password}
            onChange={(e) => setPassword(e.target.value)}
            autoComplete="new-password"
          />
          <input
            type="password"
            className="nx-input text-sm"
            placeholder="Confirm password"
            value={confirm}
            onChange={(e) => setConfirm(e.target.value)}
            autoComplete="new-password"
          />
          <button
            className="nx-btn nx-btn-primary self-start"
            onClick={generate}
            disabled={step === "generating" || !password}
          >
            {step === "generating" ? "Generating…" : "Generate identity"}
          </button>
        </div>
      )}

      {(hasKeys || step === "done") && (
        <div className="flex flex-col gap-2">
          <p className="text-xs text-state-success font-medium">
            ✓ Phantom identity active. Your private key is encrypted on this device only.
          </p>

          <div className="flex flex-col gap-1.5">
            <p className="text-xs text-muted">Enter your key password to export a backup:</p>
            <div className="flex gap-2">
              <input
                type="password"
                className="nx-input text-sm flex-1"
                placeholder="Key password"
                value={password}
                onChange={(e) => setPassword(e.target.value)}
                autoComplete="current-password"
              />
              <button
                className="nx-btn nx-btn-ghost shrink-0"
                onClick={doExport}
                disabled={exporting}
              >
                {exporting ? "Exporting…" : "Export key"}
              </button>
            </div>
          </div>

          <button
            className="nx-btn nx-btn-danger self-start text-xs"
            onClick={doDelete}
          >
            Remove from this device
          </button>
        </div>
      )}
    </div>
  );
}
