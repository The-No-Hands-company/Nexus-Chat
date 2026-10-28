import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import path from "path";
import type { Plugin } from "vite";
import type { IncomingMessage, ServerResponse } from "http";

const AUTH_BASE = process.env.NEXUS_AUTH_INTERNAL_URL ?? "http://localhost:4310";
const NEXUS_SERVER_NAME = process.env.NEXUS__SERVER__NAME ?? "localhost";

// Per-session token cache: avoids calling Auth on every request (tokens live 120s).
const identityCache = new Map<string, { token: string; expiresAt: number }>();

async function mintIdentityToken(sessionCookie: string): Promise<string | null> {
  const cached = identityCache.get(sessionCookie);
  if (cached && cached.expiresAt > Date.now() + 5_000) return cached.token;

  try {
    const res = await fetch(`${AUTH_BASE}/api/v1/auth/identity-token`, {
      method: "POST",
      headers: { "Content-Type": "application/json", Cookie: `nexus_session=${sessionCookie}` },
      body: JSON.stringify({ audience: NEXUS_SERVER_NAME }),
    });
    if (!res.ok) return null;
    const { token, expiresIn } = await res.json() as { token: string; expiresIn: number };
    identityCache.set(sessionCookie, { token, expiresAt: Date.now() + expiresIn * 1000 });
    return token;
  } catch {
    return null;
  }
}

// Extracts nexus_session value from Cookie header.
function extractSessionCookie(cookieHeader: string | undefined): string | null {
  if (!cookieHeader) return null;
  const match = cookieHeader.match(/(?:^|;\s*)nexus_session=([^;]+)/);
  return match?.[1] ?? null;
}

// Vite plugin: inject X-Nexus-Identity on proxied /api requests using the
// browser's nexus_session cookie to mint a short-lived identity token from Auth.
function nexusDevAuthPlugin(): Plugin {
  return {
    name: "nexus-dev-auth",
    configureServer(server) {
      server.middlewares.use(async (req: IncomingMessage, res: ServerResponse, next: () => void) => {
        if (!req.url?.startsWith("/api/")) return next();
        const sessionToken = extractSessionCookie(req.headers.cookie);
        if (sessionToken) {
          const identityToken = await mintIdentityToken(sessionToken);
          if (identityToken) {
            req.headers["x-nexus-identity"] = identityToken;
          }
        }
        next();
      });
    },
  };
}

export default defineConfig({
  plugins: [react(), tailwindcss(), nexusDevAuthPlugin()],
  resolve: {
    alias: {
      "@": path.resolve(__dirname, "src"),
    },
  },
  server: {
    host: '0.0.0.0',
    port: 5174,
    proxy: {
      "/api": { target: "http://localhost:8080", changeOrigin: true },
      "/gateway": { target: "ws://localhost:8081", ws: true, changeOrigin: true },
    },
  },
});
