// Chat view — DOM port of PromptView / ChatBubble / TypingDotsView from
// IslandViewContent.swift.

import { h, svg, clear } from "./dom";
import { ICONS } from "./icons";
import { Bridge, onEvent, type ChatContext, type SessionCard, type ChatMsg } from "../core/bridge";
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
  // The live reply: its own row, updated in place. The log rebuild in sync()
  // is keyed on message count — re-rendering it per token would rebuild the
  // whole transcript every few milliseconds.
  const streamBubble = h("div", { class: "reply" });
  const streamRow = h("div", { class: "chat-row" }, streamBubble);
  streamRow.style.display = "none";
  const input = h("input", {
    type: "text",
    class: "chat-input",
    placeholder: "Ask me anything…",
    spellcheck: "false",
  }) as HTMLInputElement;
  // One button, two modes: arrow sends, cross stops the running turn.
  const send = h("button", { class: "send-btn", title: "Send" }, svg(ICONS.arrowUp, 11));
  let sendMode: "send" | "stop" = "send";
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

  /** One conversation row: click opens it, hover reveals rename and delete. */
  function sessionRow(s: SessionCard): HTMLElement {
    const title = h("span", { class: "hist-title", text: s.title });
    const row = h(
      "button",
      { class: "hist-row", title: s.title },
      title,
      h("span", { class: "hist-time", text: relTime(s.updated) }),
    );
    row.addEventListener("click", () => void openConversation(s.id));

    const pencil = h(
      "button",
      { class: "hist-act", title: "Rename" },
      svg(ICONS.pencil, 13, { stroke: 1.7 }),
    );
    const trash = h(
      "button",
      { class: "hist-act", title: "Delete" },
      svg(ICONS.trash, 13, { stroke: 1.7 }),
    );

    // Rename in place: the title span becomes a field, Enter/blur commits,
    // Escape puts the old one back. `done` gates blur too — it fires right
    // after Enter, and one rename must not fire twice.
    pencil.addEventListener("click", (ev) => {
      ev.stopPropagation();
      const field = h("input", { class: "hist-rename", type: "text", spellcheck: "false" }) as HTMLInputElement;
      field.value = s.title;
      title.replaceWith(field);
      row.title = "";
      field.focus();
      field.select();
      let done = false;
      const finish = (commit: boolean) => {
        if (done) return;
        done = true;
        const next = field.value;
        field.replaceWith(title);
        row.title = s.title;
        if (!commit || next.trim() === s.title) return;
        void Bridge.chatRename(s.id, next).then(
          () => void loadHistory(),
          (err) => void loadHistory(`Couldn't rename it: ${String(err).replace(/^Error:\s*/, "")}`),
        );
      };
      field.addEventListener("keydown", (ev2) => {
        const key = (ev2 as KeyboardEvent).key;
        if (key === "Enter") {
          ev2.preventDefault();
          finish(true);
        } else if (key === "Escape") {
          ev2.preventDefault();
          finish(false);
        }
        ev2.stopPropagation(); // Escape closes the island, not the history pane
      });
      field.addEventListener("blur", () => finish(true));
    });

    // Delete asks twice: one click arms the button, the second does it. There
    // is no undo, so a stray click must not be enough — and no dialog that
    // could get lost behind the island.
    let armed = false;
    trash.addEventListener("click", (ev) => {
      ev.stopPropagation();
      if (!armed) {
        armed = true;
        trash.classList.add("armed");
        trash.title = "Click again to delete";
        return;
      }
      void Bridge.chatDelete(s.id).then(
        (wasOpen) => {
          // Deleted the conversation on screen: go back to an empty one, or
          // the bubbles would outlive their file.
          if (wasOpen) {
            State.chatHistory = [];
            State.chatGen++;
            State.chatSending = false;
            State.chatProgress = null;
            State.chatStream = "";
            State.stateOverride = null;
            State.view = "prompt";
          }
          void loadHistory();
          State.notify();
          onHeightChange();
        },
        (err) => void loadHistory(`Couldn't delete it: ${String(err).replace(/^Error:\s*/, "")}`),
      );
    });

    const actions = h("span", { class: "hist-actions" }, pencil, trash);
    return h("div", { class: "hist-item" }, row, actions);
  }

  async function loadHistory(note?: string) {
    clear(histPane);
    if (note) histPane.append(h("div", { class: "hist-empty hist-error", text: note }));
    histPane.append(h("div", { class: "hist-empty", text: "Loading…" }));
    let sessions: SessionCard[];
    try {
      sessions = await Bridge.chatSessions();
    } catch (err) {
      clear(histPane);
      histPane.append(h("div", { class: "hist-empty", text: `Couldn't list conversations: ${String(err).replace(/^Error:\s*/, "")}` }));
      return;
    }
    clear(histPane);
    if (note) histPane.append(h("div", { class: "hist-empty hist-error", text: note }));
    const fresh = h(
      "button",
      { class: "hist-row hist-new", title: "Start a new conversation" },
      h("span", { class: "hist-title", text: "+ New conversation" }),
      h("span", { class: "hist-time", text: "" }),
    );
    fresh.addEventListener("click", () => {
      State.chatHistory = [];
      State.chatGen++;
      State.chatSending = false;
      State.chatProgress = null;
      State.chatStream = "";
      State.stateOverride = null;
      void Bridge.chatReset();
      showHistory(false);
      State.notify();
      onHeightChange();
      input.focus();
    });
    histPane.append(fresh);
    if (sessions.length === 0) {
      histPane.append(h("div", { class: "hist-empty", text: "No conversations yet." }));
      return;
    }
    for (const s of sessions) histPane.append(sessionRow(s));
  }

  async function openConversation(id: string) {
    let msgs: ChatMsg[];
    try {
      msgs = await Bridge.chatOpen(id);
    } catch (err) {
      clear(histPane);
      histPane.append(h("div", { class: "hist-empty", text: `Couldn't open it: ${String(err).replace(/^Error:\s*/, "")}` }));
      return;
    }
    // Full transcript, prompts included: the walker reads every wire shape
    // (flat user text included), so questions and answers stay together.
    State.chatHistory = msgs.map((m) => ({
      id: nextId++,
      role: m.role === "user" ? ("user" as const) : ("assistant" as const),
      content: m.text,
    }));
    State.chatGen++;
    State.chatSending = false;
    State.chatProgress = null;
    State.chatStream = "";
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

  let renderedKey = "";
  // Sending state lives in State.chatSending so the history list can
  // unblock it when a conversation is opened mid-turn.
  void onEvent<{ tool: string; detail: string }>("tool-progress", ({ tool, detail }) => {
    if (!State.chatSending) return;
    // A tool means this model call is over. Whatever it said before asking for
    // the tool was preamble, not the answer — drop it, or the next stream would
    // stack on stale words.
    if (tool) State.chatStream = "";
    State.chatProgress = tool ? detail || tool : null;
    State.notify();
  });

  // The generation this submit belongs to. A reset bumps State.chatGen, and a
  // reply that arrives afterwards is ignored instead of landing in the new
  // conversation.
  let activeGen = 0;
  // True only while our own turn is streaming: a late delta from a cancelled
  // or superseded turn is dropped instead of ghosting into the next answer.
  let streamActive = false;
  let streamFrame = 0;

  // Words land faster than they can be drawn (and a notify redraws the island).
  // Batch them into one render per frame — smooth to read, cheap to run.
  void onEvent<{ delta: string }>("chat-delta", ({ delta }) => {
    if (!streamActive || !delta || State.chatGen !== activeGen) return;
    State.chatStream += delta;
    if (streamFrame) return;
    streamFrame = requestAnimationFrame(() => {
      streamFrame = 0;
      State.notify();
    });
  });

  async function submit() {
    const query = input.value.trim();
    if (!query || State.chatSending) return;
    input.value = "";
    State.chatSending = true;
    State.chatProgress = null;
    Sound.play("send");

    State.chatHistory.push({ id: nextId++, role: "user", content: query });
    State.stateOverride = "thinking";
    State.notify();
    onHeightChange();

    const file = State.droppedFile;
    const context: ChatContext | null =
      State.chatHistory.length === 1 && file ? { kind: "file", name: file.name, path: file.path } : null;

    activeGen = State.chatGen;
    State.chatStream = "";
    streamActive = true;
    try {
      // Direct call: the reply comes back here, seconds later. The pin holds
      // the island open across LLM latency so the auto-collapse cannot drop
      // the input shape mid-answer (same reason the approval card pins).
      State.isPinned = true;
      const reply = await Bridge.chatSend(query, context);
      if (State.chatGen !== activeGen) return; // reset while awaiting: drop it
      State.chatHistory.push({ id: nextId++, role: "assistant", content: reply.text });
      State.stateOverride = null;
      State.chatProgress = null;
      Sound.play("finish");
    } catch (err) {
      State.stateOverride = null;
      State.chatProgress = null;
      State.noteMessage = String(err).replace(/^Error:\s*/, "");
      State.view = "note";
      Sound.play("error");
    } finally {
      streamActive = false;
      State.chatStream = "";
      State.chatSending = false;
      State.isPinned = historyOpen || State.pendingApproval != null;
      State.notify();
      onHeightChange();
      input.focus();
    }
  }

  send.addEventListener("click", () => {
    if (State.chatSending) void Bridge.chatCancel();
    else void submit();
  });
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
      const streaming = streamActive && State.chatStream.length > 0;
      // Dots until the first words land, then the words themselves.
      const waiting = thinking && !streaming;
      const count = State.chatHistory.length + (waiting ? 0.5 : 0);
      const progress = thinking ? State.chatProgress : null;
      const key = `${count}:${progress ?? ""}`;
      if (key !== renderedKey) {
        renderedKey = key;
        clear(log);
        for (const m of State.chatHistory) log.append(bubble(m));
        if (waiting) {
          if (progress) log.append(h("div", { class: "chat-row" }, h("div", { class: "prog", text: progress })));
          log.append(typingDots());
        }
        log.scrollTop = log.scrollHeight;
      }

      // The streaming row sits outside that rebuild and is updated in place:
      // each delta costs one textContent, never a transcript rebuild.
      if (streaming) {
        streamBubble.textContent = State.chatStream;
        if (!log.contains(streamRow)) log.append(streamRow);
        log.scrollTop = log.scrollHeight;
      } else if (log.contains(streamRow)) {
        streamRow.remove();
      }

      const mode = State.chatSending ? "stop" : "send";
      if (mode !== sendMode) {
        sendMode = mode;
        clear(send);
        send.append(svg(mode === "stop" ? ICONS.xmark : ICONS.arrowUp, 11));
        send.title = mode === "stop" ? "Stop this turn" : "Send";
        send.classList.toggle("stop", mode === "stop");
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
