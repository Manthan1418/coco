// The `mail` view: send the dropped file (or just a note) by email.
//
// macOS handed this to Mail.app over AppleScript; here the Rust side speaks SMTP
// directly (mail.rs). This view is only the small form the spec asks for — To,
// Subject, one optional line — plus the attach line when a file was dropped.

import { h, clear } from "./dom";
import { State } from "../core/state";
import { Bridge } from "../core/bridge";
import { Sound } from "../core/sound";
import type { ViewActions, ViewHost } from "./views";

/** One labelled single-line field. */
function field(label: string, input: HTMLInputElement): HTMLElement {
  return h("div", { class: "mail-row" }, h("label", { text: label }), input);
}

export function buildMail(actions: ViewActions): ViewHost {
  const to = h("input", {
    class: "mail-input",
    type: "text",
    placeholder: "name@example.com",
    autocomplete: "off",
    spellcheck: "false",
  }) as HTMLInputElement;
  const subject = h("input", {
    class: "mail-input",
    type: "text",
    placeholder: "Subject",
    autocomplete: "off",
    spellcheck: "false",
  }) as HTMLInputElement;
  const body = h("input", {
    class: "mail-input",
    type: "text",
    placeholder: "Add a line (optional)",
    autocomplete: "off",
    spellcheck: "false",
  }) as HTMLInputElement;

  const attach = h("div", { class: "mail-attach" });
  const status = h("div", { class: "mail-status" });
  const send = h(
    "button",
    { class: "btn primary", onclick: () => void submit() },
    h("span", { text: "Send" }),
  ) as HTMLButtonElement;
  const cancel = h("button", {
    class: "btn secondary",
    text: "Cancel",
    onclick: () => actions.setView(State.defaultView()),
  }) as HTMLButtonElement;

  const el = h(
    "div",
    { class: "view" },
    h(
      "div",
      { class: "card" },
      h(
        "div",
        { class: "stack mail-stack" },
        field("To", to),
        field("Subject", subject),
        field("Message", body),
        attach,
        h("div", { class: "actions" }, cancel, send),
        status,
      ),
    ),
  );

  let sending = false;
  /** The file the form was last seeded for, so typing is never overwritten. */
  let seededFor: string | null = null;

  /** Prefill from the dropped file and the default recipient, once per file. */
  function seed() {
    const file = State.droppedFile;
    const key = file?.path ?? null;
    if (key === seededFor) return;
    seededFor = key;
    subject.value = file?.name ?? "";
    if (State.settings.smtpTo && !to.value) to.value = State.settings.smtpTo;
    clear(attach);
    if (file) attach.append(h("span", { text: `Attaching ${file.name}` }));
  }

  async function submit() {
    if (sending) return;
    const addr = to.value.trim();
    if (!addr) {
      status.textContent = "Who should the mail go to?";
      to.focus();
      return;
    }
    sending = true;
    send.disabled = true;
    cancel.disabled = true;
    status.textContent = "Sending…";
    try {
      await Bridge.mailSend({
        to: addr,
        subject: subject.value.trim(),
        body: body.value,
        filePath: State.droppedFile?.path ?? null,
        fileName: State.droppedFile?.name ?? null,
      });
      Sound.play("send");
      State.noteMessage = `Mail sent to ${addr}.`;
      actions.setView("note");
      // Same courtesy as the file-failure note: clear the card on its own.
      window.setTimeout(() => {
        if (State.view === "note") actions.setView(State.defaultView());
      }, 2400);
    } catch (err) {
      sending = false;
      send.disabled = false;
      cancel.disabled = false;
      status.textContent = String(err).replace(/^Error:\s*/, "");
      Sound.play("error");
    }
  }

  return {
    el,
    sync() {
      seed();
    },
    focus() {
      // Reopening the view starts a fresh attempt: re-enable and clear the last
      // error, then put the cursor in the first field still waiting for input.
      sending = false;
      send.disabled = false;
      cancel.disabled = false;
      status.textContent = "";
      seed();
      (to.value ? (subject.value ? body : subject) : to).focus();
    },
  };
}
