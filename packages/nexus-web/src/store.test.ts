import { describe, it, expect } from "vitest";
import { reconcileSent, type NxMessage } from "./store";

function msg(id: string, content = "hello"): NxMessage {
  return {
    id,
    content,
    author_id: "u1",
    author_username: "founder",
    author_avatar: null,
    channel_id: "c1",
    created_at: "2026-08-15T10:00:00.000Z",
    edited_at: null,
    attachments: [],
    reactions: [],
    reply_to: null,
  };
}

describe("reconcileSent", () => {
  const sent = msg("real-1");

  it("swaps the placeholder for the server's message when the response wins the race", () => {
    const out = reconcileSent([msg("older"), msg("temp-1")], "temp-1", sent);
    expect(out.map((m) => m.id)).toEqual(["older", "real-1"]);
  });

  it("drops the placeholder when the gateway echo already delivered the message", () => {
    // The bug this exists for: the echo lands first, so the real message is
    // already present. Swapping the placeholder would leave it twice, and
    // every message appeared in pairs.
    const out = reconcileSent([msg("older"), msg("temp-1"), sent], "temp-1", sent);
    expect(out.map((m) => m.id)).toEqual(["older", "real-1"]);
  });

  it("never leaves the same message twice, whichever order they arrive in", () => {
    for (const list of [
      [msg("temp-1"), sent],
      [sent, msg("temp-1")],
      [msg("temp-1")],
    ]) {
      const ids = reconcileSent(list, "temp-1", sent).map((m) => m.id);
      expect(new Set(ids).size).toBe(ids.length);
      expect(ids).toContain("real-1");
      expect(ids).not.toContain("temp-1");
    }
  });

  it("leaves unrelated messages alone", () => {
    const out = reconcileSent([msg("a"), msg("b")], "temp-1", sent);
    expect(out.map((m) => m.id)).toEqual(["a", "b"]);
  });
});
