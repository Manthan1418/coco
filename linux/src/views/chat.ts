// Chat view — DOM port of PromptView / ChatBubble / TypingDotsView from
// IslandViewContent.swift.

import { h, svg, clear } from "./dom";
import { ICONS } from "./icons";
import { Bridge, onEvent, type ChatContext, type OpencodeTurn, type SessionCard, type ChatMsg } from "../core/bridge";
import { Sound } from "../core/sound";
import { State, type ChatMessage } from "../core/state";
import type { ViewHost } from "./views";

let nextId = 1;

function bubble(message: ChatMessage): HTMLElement {
  if (message.role === "user") {
    return h(
      "div",
      { class: "chat-row user" },
      h("div", { class: "bubble", text: message.content }),
    );
  }
  return h("div", { class: "chat-row" }, h("div", { class: "reply", text: message.content }));
}

function typingDots(): HTMLElement {
  return h(
    "div",
    { class: "chat-row" },
    h("div", { class: "typing" }, h("i"), h("i"), h("i")),
  );
}

/** The coloured chip showing what the question is about (a dropped file). */
function contextChip(label: string): HTMLElement {
  const chip = h("div", { class: "chip" }, h("i", { class: "chip-dot" }), h("span", { text: label }));
  requestAnimationFrame(() => chip.classList.add("settled"));
  return chip;
}

export function buildPrompt(onHeightChange: () => void): ViewHost {
  const chipRow = h("div", { class: "chip-row" });
  const log = h("div", { class: "chat-log" });
  const input = h("input", {
    type: "text",
    class: "chat-input",
    placeholder: "Ask me anything…",
    spellcheck: "false",
  }) as HTMLInputElement;
  const send = h("button", { class: "send-btn", title: "Send" }, svg(ICONS.arrowUp, 11));
  const histBtn = h("button", { class: "ghost-btn", title: "Conversations" }, svg(ICONS.timer, 12));
  const bar = h("div", { class: "chat-bar" }, input, histBtn, send);
  const histPane = h("div", { class: "hist-list" });
  histPane.style.display = "none"; // See showHistory: author display:flex beats [hidden].

  /** Unix seconds → "2m", "3h", "4d". Anything odd renders blank, not wrong. */
  function relTime(updated: number): string {
    if (!updated || updated <= 0) return "";
    const mins = Math.max(0, Math.round((Date.now() / 1000 - updated) / 60));
    if (mins < 1) return "now";
    if (mins < 60) return `${mins}m`;
    const hours = Math.round(mins / 60);
    if (hours < 24) return `${hours}h`;
    return `${Math.round(hours / 24)}d`;
  }

  let historyOpen = false;

  function showHistory(open: boolean) {
    historyOpen = open;
    // Browsing is reading: pin while open or the 5 s auto-collapse drops the
    // input shape mid-read and the list goes dead under the cursor. Approval
    // cards own the pin when one is up — never steal it back from them.
    State.isPinned = open || State.pendingApproval != null;
    // Explicit display, never the `hidden` attribute: .hist-list sets
    // display:flex in author CSS, which beats the UA stylesheet's
    // [hidden]{display:none} — so hidden="" alone never hides the pane.
    // That was the whole bug: the list rendered (rows append regardless)
    // but no close path could ever hide it again.
    (log as HTMLElement).style.display = open ? "none" : "";
    histPane.style.display = open ? "" : "none";
    if (open) void loadHistory();
  }

  async function loadHistory() {
    clear(histPane);
    histPane.append(h("div", { class: "hist-empty", text: "Loading…" }));
    let sessions: SessionCard[];
    try {
      sessions = await Bridge.opencodeSessions();
    } catch (err) {
      clear(histPane);
      histPane.append(h("div", { class: "hist-empty", text: `Couldn't list conversations: ${String(err).replace(/^Error:\s*/, "")}` }));
      return;
    }
    clear(histPane);
    if (sessions.length === 0) {
      histPane.append(h("div", { class: "hist-empty", text: "No conversations yet." }));
      return;
    }
    for (const s of sessions) {
      const row = h(
        "button",
        { class: "hist-row", title: s.title },
        h("span", { class: "hist-title", text: s.title }),
        h("span", { class: "hist-time", text: relTime(s.updated) }),
      );
      row.addEventListener("click", () => void openConversation(s.id));
      histPane.append(row);
    }
  }

  async function openConversation(id: string) {
    let msgs: ChatMsg[];
    try {
      msgs = await Bridge.opencodeOpen(id);
    } catch (err) {
      clear(histPane);
      histPane.append(h("div", { class: "hist-empty", text: `Couldn't open it: ${String(err).replace(/^Error:\s*/, "")}` }));
      return;
    }
    State.chatHistory = msgs.map((m) => ({
      id: nextId++,
      role: m.role === "user" ? ("user" as const) : ("assistant" as const),
      content: m.text,
    }));
    State.chatGen++;
    State.chatSending = false;
    State.stateOverride = null;
    State.view = "prompt";
    showHistory(false);
    State.notify();
    onHeightChange();
  }

  histBtn.addEventListener("click", () => showHistory(!historyOpen));

  const el = h(
    "div",
    { class: "view" },
    h("div", { class: "card wash chat-card" }, h("div", { class: "chat-body" }, chipRow, log, histPane, bar)),
  );
  (el.querySelector(".card") as HTMLElement).style.setProperty("--wash", "rgba(99,102,241,0.5)");

  let renderedCount = -1;
  // Sending state lives in State.chatSending so the history list can
  // unblock it when a conversation is opened mid-turn.
  // The generation this submit belongs to. A reset bumps State.chatGen, and a
  // turn that finishes afterwards is ignored — see the opencode-turn handler.
  let activeGen = 0;

  // The brain answers asynchronously: chatSend only enqueues, and the turn
  // lands here when the agent goes idle. Registered once per chat view.
  void onEvent<OpencodeTurn>("opencode-turn", (turn) => {
    if (!State.chatSending) return;
    State.chatSending = false;
    // The turn held the pin (see submit); hand it back unless something else
    // — history browsing, an approval card — still needs it.
    State.isPinned = historyOpen || State.pendingApproval != null;
    // A reset (file drop) starts a new conversation: unblock either way, but
    // only land the answer when it belongs to the current one.
    if (State.chatGen !== activeGen) {
      State.isPinned = historyOpen || State.pendingApproval != null;
      State.notify();
      return;
    }
    State.stateOverride = null;
    if (turn.text) {
      State.chatHistory.push({ id: nextId++, role: "assistant", content: turn.text });
      Sound.play("finish");
    } else {
      State.noteMessage = (turn.error ?? "The turn failed.").replace(/^Error:\s*/, "");
      State.view = "note";
      Sound.play("error");
    }
    State.notify();
    onHeightChange();
    input.focus();
  });

  async function submit() {
    const query = input.value.trim();
    if (!query || State.chatSending) return;
    input.value = "";
    State.chatSending = true;
    Sound.play("send");

    State.chatHistory.push({ id: nextId++, role: "user", content: query });
    State.stateOverride = "thinking";
    State.notify();
    onHeightChange();

    const file = State.droppedFile;
    const context: ChatContext | null =
      State.chatHistory.length === 1 && file ? { kind: "file", name: file.name, path: file.path } : null;

    try {
      // Enqueue only: the answer arrives as an `opencode-turn` event when the
      // agent goes idle (after tools and any permission cards). chatSending
      // stays true until then, and a second submit is refused below.
      await Bridge.chatSend(query, context);
      activeGen = State.chatGen;
      // A turn takes seconds to minutes (tools, cards). Without the pin the
      // auto-collapse fires mid-answer, the shape shrinks to the strip, and
      // the reply lands in an island the user can no longer touch.
      State.isPinned = true;
    } catch (err) {
      State.stateOverride = null;
      State.noteMessage = String(err).replace(/^Error:\s*/, "");
      State.view = "note";
      Sound.play("error");
      State.chatSending = false;
    } finally {
      // NOTE: chatSending stays true on success — only the opencode-turn handler
      // or the catch above clears it. Clearing here would drop the answer.
      State.notify();
      onHeightChange();
      input.focus();
    }
  }

  send.addEventListener("click", () => void submit());
  input.addEventListener("keydown", (e) => {
    if ((e as KeyboardEvent).key === "Enter") {
      e.preventDefault();
      void submit();
    }
    e.stopPropagation(); // Escape closes the island, not the chat
  });

  return {
    el,
    sync() {
      const file = State.droppedFile;
      const wantChip = file?.name ?? "";
      if (chipRow.dataset.label !== wantChip) {
        chipRow.dataset.label = wantChip;
        clear(chipRow);
        if (wantChip) chipRow.append(contextChip(wantChip));
      }

      const thinking = State.stateOverride === "thinking";
      const count = State.chatHistory.length + (thinking ? 0.5 : 0);
      if (count !== renderedCount) {
        renderedCount = count;
        clear(log);
        for (const m of State.chatHistory) log.append(bubble(m));
        if (thinking) log.append(typingDots());
        log.scrollTop = log.scrollHeight;
      }

      input.placeholder = State.chatHistory.length === 0 ? "Ask me anything…" : "Continue…";
      input.disabled = State.chatSending;
    },
    focus() {
      input.focus();
      input.select();
    },
  };
}
