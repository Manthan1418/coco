// OpenCode permission events → the shared approval card.
// Mirrors hooks.ts (Claude Code), minus everything transport-specific to the
// pipe relay: no ack window (the reply IS the ack), no terminal fallback (a
// denial lets the agent continue), no expiry (OpenCode waits indefinitely,
// so the card stays until answered or replied-elsewhere).
//
// One card, one request, whichever agent asked first — the same rule as the
// hook relay, for the same reason: a second card must never quietly replace
// the first and leave a human answering the wrong request.

import { Bridge, onEvent, type OpencodePermission } from "../core/bridge";
import { Sound } from "../core/sound";
import { State } from "../core/state";
import type { Island } from "./island";

/**
 * What the Allow button authorises. Same principle as the hook card: the
 * command, file path or URL — not just the tool name. OpenCode's `message`
 * is usually that string already; `resources[0]` is the fallback.
 */
function approvalTarget(p: OpencodePermission): string {
  const message = (p.message ?? "").trim();
  if (message) return `${p.action} · ${message}`;
  const first = (p.resources[0] ?? "").trim();
  if (first) return `${p.action} · ${first}`;
  return p.action;
}

function showCard(island: Island, p: OpencodePermission) {
  State.pendingApproval = {
    requestId: p.id,
    sessionId: p.sessionId,
    tool: p.action,
    command: approvalTarget(p),
    source: "opencode",
  };
  State.isPinned = true;
  Sound.play("approval");
  if (State.mode === "expanded") {
    island.setView("approval");
  } else {
    State.setPillBadge("integration_claude", "approval");
    island.reveal();
  }
  State.notify();
}

function clearCard(island: Island, dropView: boolean) {
  if (!State.pendingApproval || State.pendingApproval.source !== "opencode") return;
  State.pendingApproval = null;
  State.isPinned = false;
  island.dropPin();
  State.setPillBadge("integration_claude", null);
  if (dropView && State.view === "approval") island.setView(State.defaultView());
  State.notify();
}

export function registerOpencodeHandlers(island: Island) {
  // A fresh request from the event feed.
  void onEvent<OpencodePermission>("opencode-permission", (p) => {
    if (State.paused) {
      // Rust already auto-rejected it — a paused island cannot answer, and an
      // unanswered request wedges the agent. Nothing to show.
      return;
    }
    const current = State.pendingApproval;
    if (current && current.requestId !== p.id) {
      // One card. The newcomer loses: deny it so its agent continues with a
      // denial instead of hanging on a card nobody can see.
      void Bridge.opencodeReply(p.sessionId, p.id, "deny").catch((err) =>
        Bridge.log(`opencode second-card auto-deny failed: ${String(err)}`),
      );
      return;
    }
    if (current) return; // Same request re-emitted; the card is already up.
    showCard(island, p);
  });

  // Answered elsewhere (TUI, another client): drop a card that would be lying.
  void onEvent<{ sessionId: string; requestId: string }>("opencode-replied", ({ requestId }) => {
    if (State.pendingApproval?.requestId === requestId) clearCard(island, true);
  });

  // Reconcile on every (re)connect: the server is the truth, the card is a
  // cache. A card whose request is gone is dropped; a pending request with no
  // card gets one (covers events missed while the feed was down).
  void onEvent<OpencodePermission[]>("opencode-pending", (pending) => {
    const current = State.pendingApproval;
    if (current?.source === "opencode") {
      if (!pending.some((p) => p.id === current.requestId)) clearCard(island, true);
      return;
    }
    if (current || State.paused || pending.length === 0) return;
    showCard(island, pending[0]);
  });
}
