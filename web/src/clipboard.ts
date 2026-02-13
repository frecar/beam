import type { InputEvent } from "./connection";

/** Direction of a clipboard sync event */
export type ClipboardSyncDirection = "sent" | "received";

/** Callback signature for clipboard sync notifications */
export type ClipboardSyncCallback = (direction: ClipboardSyncDirection, preview: string) => void;

/** A single clipboard history entry */
export interface ClipboardHistoryEntry {
  timestamp: number;
  direction: ClipboardSyncDirection;
  text: string;
}

const MAX_HISTORY = 20;
const HISTORY_PREVIEW_LENGTH = 100;

/**
 * Bridges the browser clipboard with the remote desktop.
 * - On paste: reads local clipboard and sends text to remote
 * - On remote clipboard event: writes to local clipboard
 */
export class ClipboardBridge {
  private sendClipboard: (event: InputEvent) => void;
  private onPaste = this.handlePaste.bind(this);
  private syncCallback: ClipboardSyncCallback | null = null;
  private history: ClipboardHistoryEntry[] = [];
  private historyCallback: (() => void) | null = null;

  constructor(sendClipboard: (event: InputEvent) => void) {
    this.sendClipboard = sendClipboard;
  }

  /** Register a callback that fires when history changes */
  onHistoryChange(callback: () => void): void {
    this.historyCallback = callback;
  }

  /** Get clipboard history entries */
  getHistory(): ClipboardHistoryEntry[] {
    return this.history;
  }

  /** Clear all clipboard history */
  clearHistory(): void {
    this.history = [];
    this.historyCallback?.();
  }

  /** Truncate text for display preview */
  static truncatePreview(text: string): string {
    const singleLine = text.replace(/\n/g, " ").trim();
    if (singleLine.length <= HISTORY_PREVIEW_LENGTH) return singleLine;
    return singleLine.substring(0, HISTORY_PREVIEW_LENGTH - 3) + "...";
  }

  /** Add an entry to the history ring buffer */
  private addHistory(direction: ClipboardSyncDirection, text: string): void {
    this.history.push({ timestamp: Date.now(), direction, text });
    if (this.history.length > MAX_HISTORY) {
      this.history.shift();
    }
    this.historyCallback?.();
  }

  /** Register a callback that fires when clipboard text is synced */
  onClipboardSync(callback: ClipboardSyncCallback): void {
    this.syncCallback = callback;
  }

  /** Build a short preview string (max 40 chars, single line) */
  private buildPreview(text: string): string {
    const firstLine = text.split("\n")[0].trim();
    if (firstLine.length <= 40) return firstLine;
    return firstLine.substring(0, 37) + "...";
  }

  enable(): void {
    document.addEventListener("paste", this.onPaste);
  }

  disable(): void {
    document.removeEventListener("paste", this.onPaste);
  }

  /** Called when the remote sends clipboard text */
  handleRemoteClipboard(text: string): void {
    if (text) {
      this.syncCallback?.("received", this.buildPreview(text));
      this.addHistory("received", text);
    }
    navigator.clipboard.writeText(text).catch(() => {
      // Clipboard write permission denied — ignore silently
    });
  }

  /**
   * Try to read the browser clipboard and send it as PRIMARY selection
   * to the remote agent. Called before middle-click so the remote X11
   * PRIMARY buffer is populated when the app handles the button press.
   *
   * Returns a Promise that resolves when the clipboard has been sent
   * (or immediately if clipboard read fails — e.g. permissions denied).
   */
  async sendPrimaryClipboard(): Promise<void> {
    const MAX_CLIPBOARD_BYTES = 1_048_576; // 1 MB
    try {
      const text = await navigator.clipboard.readText();
      if (text && text.length <= MAX_CLIPBOARD_BYTES) {
        this.sendClipboard({ t: "cp", text });
        this.syncCallback?.("sent", this.buildPreview(text));
        this.addHistory("sent", text);
      }
    } catch {
      // Clipboard read permission denied or not focused — ignore silently.
      // The middle-click will still go through, just without clipboard sync.
    }
  }

  private handlePaste(e: ClipboardEvent): void {
    const MAX_CLIPBOARD_BYTES = 1_048_576; // 1 MB
    const text = e.clipboardData?.getData("text");
    if (text && text.length <= MAX_CLIPBOARD_BYTES) {
      e.preventDefault();
      this.sendClipboard({ t: "c", text });
      this.syncCallback?.("sent", this.buildPreview(text));
      this.addHistory("sent", text);
    }
  }
}
